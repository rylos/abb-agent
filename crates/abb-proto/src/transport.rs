//! Agent → server connection: TCP 5510, one plaintext `encrypt_channel`, then TLS on the
//! same socket. One request at a time per connection. The backup data connection stays in
//! plaintext from the start when the task does not encrypt the transfer.

use crate::pobject::Value;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 5510;

/// Advertised version: the same as the registered official agent (3.2.2-5554).
pub const AGENT_MAJOR: u64 = 3;
pub const AGENT_MINOR: u64 = 2;
pub const AGENT_MINI: u64 = 2;
pub const AGENT_BUILD: &str = "5554";

/// SHA-256 fingerprint of the server certificate (DER).
pub type Fingerprint = [u8; 32];

pub fn fingerprint_hex(f: &Fingerprint) -> String {
    f.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Tls(rustls::Error),
    /// The server replied with `error:{code, reason}`.
    Server { code: u64, reason: String },
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "network: {e}"),
            Error::Tls(e) => write!(f, "TLS: {e}"),
            Error::Server { code, reason } => write!(f, "server error 0x{code:x}: {reason}"),
            Error::Protocol(s) => write!(f, "protocol: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Self {
        Error::Tls(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

static ANNOUNCED_BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Build announced to the server: [`AGENT_BUILD`], or the one set with [`announce_build`].
pub fn agent_build() -> &'static str {
    ANNOUNCED_BUILD.get().map_or(AGENT_BUILD, String::as_str)
}

/// Announces another build (once, before the first request): for a server that refuses
/// [`AGENT_BUILD`] as too old, until a release announces the new one.
pub fn announce_build(build: &str) {
    let _ = ANNOUNCED_BUILD.set(build.to_string());
}

/// The `agent` field present in every request.
pub fn agent_field() -> Value {
    Value::map([(
        "version",
        Value::map([
            ("build", Value::str(agent_build())),
            ("major", Value::Int(AGENT_MAJOR)),
            ("mini", Value::Int(AGENT_MINI)),
            ("minor", Value::Int(AGENT_MINOR)),
        ]),
    )])
}

/// Request in the official agent's order: `agent`, `action`, then the other keys in
/// alphabetical order.
pub fn request(action: &str, mut fields: Vec<(&str, Value)>) -> Value {
    fields.sort_by(|a, b| a.0.cmp(b.0));
    let mut entries = vec![("agent".to_string(), agent_field()), ("action".to_string(), Value::str(action))];
    entries.extend(fields.into_iter().map(|(k, v)| (k.to_string(), v)));
    Value::Map(entries)
}

/// Certificate check like the official agent: no CA chain, only the fingerprint pinned
/// on first contact. Handshake signatures are still verified.
#[derive(Debug)]
struct PinVerifier {
    pin: Option<Fingerprint>,
    seen: Mutex<Option<Fingerprint>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fp: Fingerprint = Sha256::digest(end_entity.as_ref()).into();
        *self.seen.lock().unwrap() = Some(fp);
        match self.pin {
            Some(pin) if pin != fp => Err(rustls::Error::General(format!(
                "server certificate changed: {} instead of {}",
                fingerprint_hex(&fp),
                fingerprint_hex(&pin)
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

enum Stream {
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
    Plain(TcpStream),
}

impl Stream {
    fn io(&mut self) -> &mut dyn ReadWrite {
        match self {
            Stream::Tls(s) => s.as_mut(),
            Stream::Plain(s) => s,
        }
    }

    fn sock(&self) -> &TcpStream {
        match self {
            Stream::Tls(s) => &s.sock,
            Stream::Plain(s) => s,
        }
    }
}

trait ReadWrite: io::Read + Write {}
impl<T: io::Read + Write> ReadWrite for T {}

fn tcp_connect(host: &str, port: u16) -> Result<(TcpStream, std::net::SocketAddr)> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::Protocol(format!("could not resolve address: {host}")))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(15))?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(600)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(600)))?;
    Ok((tcp, addr))
}

pub struct Connection {
    stream: Stream,
    /// Fingerprint of the certificate the server presented on this connection.
    pub fingerprint: Fingerprint,
    /// Plaintext reply to `encrypt_channel` (contains the server versions).
    pub hello: Value,
}

impl Connection {
    /// Opens an encrypted connection. With `pin = None` it accepts any certificate and
    /// returns its fingerprint in `fingerprint` (first contact).
    pub fn open(host: &str, port: u16, pin: Option<Fingerprint>) -> Result<Connection> {
        let (mut tcp, addr) = tcp_connect(host, port)?;

        // Plaintext phase.
        tcp.write_all(&request("encrypt_channel", vec![]).to_bytes())?;
        let hello = Value::read_from(&mut tcp)?;
        check_error(&hello)?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinVerifier { pin, seen: Mutex::new(None), provider: provider.clone() });
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        config.enable_sni = false;
        // The name is only for rustls: without SNI it is not sent and the certificate is
        // not checked against it.
        let name = ServerName::try_from(addr.ip().to_string())
            .map_err(|e| Error::Protocol(format!("server name: {e}")))?;
        let mut conn = ClientConnection::new(Arc::new(config), name)?;
        while conn.is_handshaking() {
            conn.complete_io(&mut tcp)?;
        }
        let fingerprint = verifier
            .seen
            .lock()
            .unwrap()
            .ok_or_else(|| Error::Protocol("the server did not present a certificate".into()))?;
        Ok(Connection { stream: Stream::Tls(Box::new(StreamOwned::new(conn, tcp))), fingerprint, hello })
    }

    /// Opens a plaintext connection (no `encrypt_channel`, no TLS), as the official agent does
    /// for the data of a task with `enable_encrypt_transfer=0`.
    pub fn open_plain(host: &str, port: u16) -> Result<Connection> {
        let (tcp, _) = tcp_connect(host, port)?;
        Ok(Connection { stream: Stream::Plain(tcp), fingerprint: [0; 32], hello: Value::Null })
    }

    pub fn send(&mut self, msg: &Value) -> Result<()> {
        let io = self.stream.io();
        io.write_all(&msg.to_bytes())?;
        io.flush()?;
        Ok(())
    }

    /// Next message, skipping any `type:"keep_alive"`.
    pub fn recv(&mut self) -> Result<Value> {
        loop {
            let v = Value::read_from(&mut self.stream.io())?;
            if v.get("type").and_then(Value::as_str) != Some("keep_alive") {
                return Ok(v);
            }
        }
    }

    /// Sends a request and returns the reply, or the server error.
    pub fn call(&mut self, msg: &Value) -> Result<Value> {
        self.send(msg)?;
        let resp = self.recv()?;
        check_error(&resp)?;
        Ok(resp)
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> Result<()> {
        Ok(self.stream.sock().set_read_timeout(t)?)
    }
}

pub fn check_error(resp: &Value) -> Result<()> {
    if let Some(err) = resp.get("error") {
        return Err(Error::Server {
            code: err.get("code").and_then(Value::as_int).unwrap_or(0),
            reason: err.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
        });
    }
    Ok(())
}
