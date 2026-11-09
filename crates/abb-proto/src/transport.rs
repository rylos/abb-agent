//! Agent → server connection: TCP 5510, one plaintext `encrypt_channel`, then TLS on the
//! same socket. One request at a time per connection. The backup data connection stays in
//! plaintext from the start when the task does not encrypt the transfer.

use crate::pobject::Value;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature, verify_tls13_signature_with_raw_key};
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
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
        let algorithms = &self.provider.signature_verification_algorithms;
        // DSM's default certificate is X.509 v1, which webpki refuses to parse. The pin is what
        // authenticates it: all this has to prove is that the peer owns the key.
        match v1_public_key(cert.as_ref()) {
            Some(spki) => verify_tls13_signature_with_raw_key(message, &SubjectPublicKeyInfoDer::from(spki), dss, algorithms),
            None => verify_tls13_signature(message, cert, dss, algorithms),
        }
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// One DER element.
struct DerElement<'a> {
    tag: u8,
    content: &'a [u8],
    /// The element with its header.
    whole: &'a [u8],
    /// What follows it.
    rest: &'a [u8],
}

/// The DER element at the start of `der`. `None` if it is malformed or truncated.
fn der_element(der: &[u8]) -> Option<DerElement<'_>> {
    let (&tag, after_tag) = der.split_first()?;
    let (&first, mut body) = after_tag.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || body.len() < n {
            return None;
        }
        let (bytes, content) = body.split_at(n);
        body = content;
        bytes.iter().fold(0, |len, &b| (len << 8) | b as usize)
    };
    if body.len() < len {
        return None;
    }
    let (content, rest) = body.split_at(len);
    Some(DerElement { tag, content, whole: &der[..der.len() - body.len() + len], rest })
}

/// The SubjectPublicKeyInfo of an X.509 v1 certificate, with its header. `None` for any other
/// version and for malformed input, which are left to webpki.
fn v1_public_key(cert: &[u8]) -> Option<&[u8]> {
    const INTEGER: u8 = 0x02;
    const SEQUENCE: u8 = 0x30;
    const VERSION: u8 = 0xa0;

    let sequence = |der| der_element(der).filter(|e| e.tag == SEQUENCE);
    let certificate = sequence(cert)?;
    let tbs = sequence(certificate.content)?;
    let mut field = der_element(tbs.content)?;
    if field.tag == VERSION {
        // A v1 certificate has no version field, but some encoders write the default anyway.
        if field.content != [INTEGER, 1, 0] {
            return None;
        }
        field = der_element(field.rest)?;
    }
    if field.tag != INTEGER {
        return None; // serial number
    }
    let mut rest = field.rest;
    for _ in ["signature", "issuer", "validity", "subject"] {
        rest = sequence(rest)?.rest;
    }
    sequence(rest).map(|e| e.whole)
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

/// Connects to the first resolved address that answers, each tried for up to 15 s.
fn tcp_connect(host: &str, port: u16) -> Result<(TcpStream, std::net::SocketAddr)> {
    let mut last = None;
    let (tcp, addr) = (host, port)
        .to_socket_addrs()?
        .find_map(|addr| match TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
            Ok(tcp) => Some((tcp, addr)),
            Err(e) => {
                last = Some(e);
                None
            }
        })
        .ok_or_else(|| match last.take() {
            Some(e) => Error::Io(e),
            None => Error::Protocol(format!("could not resolve address: {host}")),
        })?;
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
        tcp.write_all(&request("encrypt_channel", vec![]).to_bytes()?)?;
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
        io.write_all(&msg.to_bytes()?)?;
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

    /// Waits up to `timeout` for something to read (a message, or the server closing the
    /// connection): `false` if nothing came, or a signal interrupted the wait.
    pub fn wait_readable(&mut self, timeout: Duration) -> Result<bool> {
        if let Stream::Tls(s) = &mut self.stream
            && s.conn.process_new_packets()?.plaintext_bytes_to_read() > 0
        {
            return Ok(true);
        }
        let sock = self.stream.sock();
        let old = sock.read_timeout()?;
        sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let r = sock.peek(&mut [0u8; 1]);
        sock.set_read_timeout(old)?;
        match r {
            Ok(n) => match &mut self.stream {
                // TLS: bytes came, but maybe not a whole record, or not application data.
                Stream::Tls(s) if n > 0 => {
                    s.conn.read_tls(&mut s.sock)?;
                    Ok(s.conn.process_new_packets()?.plaintext_bytes_to_read() > 0)
                }
                _ => Ok(true),
            },
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => Ok(false),
            Err(e) => Err(e.into()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::{ClientHello, ResolvesServerCert, ServerConfig, ServerConnection};
    use rustls::sign::CertifiedKey;
    use std::net::TcpListener;

    // Throwaway certificates and keys, made for these tests and used nowhere else.
    const V1: &[u8] = include_bytes!("../testdata/v1.der");
    const V1_KEY: &[u8] = include_bytes!("../testdata/v1.key.der");
    const V3: &[u8] = include_bytes!("../testdata/v3.der");
    const V3_KEY: &[u8] = include_bytes!("../testdata/v3.key.der");

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        assert!(content.len() < 0x80);
        [&[tag, content.len() as u8], content].concat()
    }

    /// A certificate with the given fields at the start of its TBSCertificate.
    fn certificate(head: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
        let spki = tlv(0x30, b"key");
        let mut tbs = head.concat();
        for _ in 0..4 {
            tbs.extend(tlv(0x30, b""));
        }
        tbs.extend(&spki);
        (tlv(0x30, &tlv(0x30, &tbs)), spki)
    }

    #[test]
    fn finds_the_key_of_a_v1_certificate_only() {
        assert!(v1_public_key(V1).is_some());
        assert_eq!(v1_public_key(V3), None);

        let serial = tlv(0x02, &[7]);
        let (cert, spki) = certificate(std::slice::from_ref(&serial));
        assert_eq!(v1_public_key(&cert), Some(&spki[..]));
        let (cert, spki) = certificate(&[tlv(0xa0, &tlv(0x02, &[0])), serial.clone()]);
        assert_eq!(v1_public_key(&cert), Some(&spki[..]));
        let (cert, _) = certificate(&[tlv(0xa0, &tlv(0x02, &[1])), serial]);
        assert_eq!(v1_public_key(&cert), None);
    }

    #[test]
    fn malformed_certificates_are_refused() {
        for end in 0..V1.len() {
            assert_eq!(v1_public_key(&V1[..end]), None, "truncated at {end}");
        }
        for der in [&[0x30, 0x80][..], &[0x30, 0x85, 1, 1, 1, 1, 1], &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff], &[0x02, 0x00]] {
            assert_eq!(v1_public_key(der), None, "{der:02x?}");
        }
    }

    #[derive(Debug)]
    struct Fixed(Arc<CertifiedKey>);

    impl ResolvesServerCert for Fixed {
        fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    /// One `Connection::open` against a local server that presents `cert` and signs with `key`.
    fn handshake(cert: &'static [u8], key: &'static [u8], versions: &'static [&'static rustls::SupportedProtocolVersion]) -> Result<Connection> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let signing_key = provider.key_provider.load_private_key(PrivateKeyDer::try_from(key).unwrap()).unwrap();
        let certified = Arc::new(CertifiedKey::new(vec![CertificateDer::from(cert)], signing_key));
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Fixed(certified)));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            Value::read_from(&mut tcp).unwrap();
            tcp.write_all(&Value::Map(vec![]).to_bytes().unwrap()).unwrap();
            let mut conn = ServerConnection::new(Arc::new(config)).unwrap();
            while conn.is_handshaking() && conn.complete_io(&mut tcp).is_ok() {}
        });
        Connection::open("127.0.0.1", port, None)
    }

    const TLS13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
    const TLS12: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];

    #[test]
    fn tls13_accepts_a_v1_certificate() {
        let c = handshake(V1, V1_KEY, TLS13).unwrap();
        assert_eq!(c.fingerprint, <Fingerprint>::from(Sha256::digest(V1)));
    }

    #[test]
    fn tls13_still_needs_the_key_of_a_v1_certificate() {
        assert!(handshake(V1, V3_KEY, TLS13).is_err());
    }

    #[test]
    fn tls13_v3_certificates_are_unchanged() {
        assert!(handshake(V3, V3_KEY, TLS13).is_ok());
        assert!(handshake(V3, V1_KEY, TLS13).is_err());
    }

    #[test]
    fn tls12_still_refuses_a_v1_certificate() {
        assert!(handshake(V1, V1_KEY, TLS12).is_err());
    }
}
