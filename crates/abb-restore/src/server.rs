//! Requests of the ABB recovery media. They carry the DSM user and password each time (there
//! is no device token), and each one goes on a new connection.

use abb_proto::transport::{Error, Fingerprint, Result};
use abb_proto::{Connection, Value, request};
use std::time::Duration;

/// Biggest piece of image data accepted in one message (the server sends up to 1 MiB).
const MAX_CHUNK: u64 = 64 << 20;

#[derive(Clone)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub username: String,
    password: String,
    otp: String,
    /// Certificate fingerprint, pinned on the first connection.
    pub pin: Option<Fingerprint>,
}

/// One range of a disk image to download.
#[derive(Clone, Debug)]
pub struct Range {
    pub image_path: String,
    pub object_id: u64,
    pub disk_number: u64,
    /// 0 for a whole disk.
    pub partition_number: u64,
    pub alias_name: String,
    pub offset: u64,
    pub length: u64,
    /// Bytes already written (resume).
    pub done: u64,
}

/// A piece of image data as the server sends it.
pub enum Chunk {
    Data { image_path: String, offset: u64, data: Vec<u8> },
    /// End of the range ending at `offset`.
    EndOfRange { image_path: String, offset: u64 },
    Done,
}

pub fn int(v: &Value, k: &str) -> u64 {
    match v.get(k) {
        Some(Value::Int(n)) => *n,
        Some(Value::String(_)) => v.get(k).and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0),
        _ => 0,
    }
}

pub fn string(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

pub fn array<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    match v.get(k) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

impl Server {
    pub fn new(host: &str, port: u16, username: &str, password: &str, otp: &str) -> Server {
        Server {
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            otp: otp.to_string(),
            pin: None,
        }
    }

    fn open(&mut self) -> Result<Connection> {
        let c = Connection::open(&self.host, self.port, self.pin)?;
        self.pin.get_or_insert(c.fingerprint);
        Ok(c)
    }

    fn call(&mut self, action: &str, mut fields: Vec<(&str, Value)>) -> Result<Value> {
        fields.push(("username", Value::str(self.username.clone())));
        fields.push(("password", Value::str(self.password.clone())));
        let msg = request(action, fields);
        self.open()?.call(&msg)
    }

    /// First contact, before sending the password: the certificate fingerprint to confirm.
    pub fn probe(&mut self) -> Result<Fingerprint> {
        let mut c = Connection::open(&self.host, self.port, None)?;
        c.call(&request("query_server_info", vec![]))?;
        self.pin = Some(c.fingerprint);
        Ok(c.fingerprint)
    }

    /// Checks the account. Returns the server's package version, e.g. "3.2.1-25059".
    pub fn login(&mut self) -> Result<String> {
        let otp = Value::str(self.otp.clone());
        let resp = self.call("auth_account", vec![("from_recovery", Value::Int(1)), ("otp", otp)])?;
        let v = resp.get("server").and_then(|s| s.get("package_version")).cloned().unwrap_or(Value::Null);
        Ok(format!("{}.{}.{}-{}", int(&v, "major"), int(&v, "minor"), int(&v, "mini"), string(&v, "build")))
    }

    /// Image backup tasks with their device (`tasks[]` of `list_device_task`).
    pub fn tasks(&mut self) -> Result<Vec<Value>> {
        let resp = self.call("list_device_task", vec![])?;
        Ok(array(&resp, "tasks").to_vec())
    }

    /// Completed versions of a task, newest first, and the device spec of each one.
    pub fn versions(&mut self, device_id: u64, task_id: u64) -> Result<Value> {
        let filter = Value::map([
            ("direction", Value::str("DESC")),
            ("limit", Value::Int(1000)),
            ("offset", Value::Int(0)),
            ("order_by", Value::str("time_start")),
            ("version_status", Value::Array(vec![Value::Int(3)])),
        ]);
        self.call(
            "list_task_version",
            vec![("device_id", Value::Int(device_id)), ("filter", filter), ("task_id", Value::Int(task_id))],
        )
    }

    /// Creates the restore task on the server (shown in the ABB console). `mode`: 1 entire device,
    /// 2 system volumes, 3 selected volumes. Returns the running task id.
    pub fn start(
        &mut self,
        device_id: u64,
        task_id: u64,
        version_id: u64,
        mode: u64,
        device_uuid: &str,
        ranges: &[Range],
    ) -> Result<u64> {
        let parts = ranges
            .iter()
            .map(|r| {
                Value::map([
                    ("alias_name", Value::str(r.alias_name.clone())),
                    ("disk_number", Value::Int(r.disk_number)),
                    ("image_path", Value::str(r.image_path.clone())),
                    ("length", Value::str(r.length.to_string())),
                    ("object_id", Value::Int(r.object_id)),
                    ("offset", Value::str(r.offset.to_string())),
                    ("partition_number", Value::Int(r.partition_number)),
                ])
            })
            .collect();
        let resp = self.call(
            "image_restore",
            vec![
                ("device_id", Value::Int(device_id)),
                ("device_uuid", Value::str(device_uuid)),
                ("restore_mode", Value::Int(mode)),
                ("restore_partitions", Value::Array(parts)),
                ("task_id", Value::Int(task_id)),
                ("version_id", Value::Int(version_id)),
            ],
        )?;
        match resp.get("running_task_id").and_then(Value::as_int) {
            Some(id) => Ok(id),
            None => Err(Error::Protocol("running_task_id missing from response".into())),
        }
    }

    /// Asks for the image data of `ranges` (from `done` on) and passes each piece to `sink`, in
    /// the order the server sends it: increasing offsets, one range after the other, only the
    /// areas in use.
    pub fn download(
        &mut self,
        running_task_id: u64,
        ranges: &[Range],
        mut sink: impl FnMut(Chunk) -> std::io::Result<()>,
    ) -> Result<()> {
        let infos = ranges
            .iter()
            .map(|r| {
                Value::map([
                    ("length", Value::Int(r.length)),
                    ("offset", Value::Int(r.done)),
                    ("path_offset", Value::str(format!("{}_{}", r.image_path, r.offset))),
                ])
            })
            .collect();
        let msg = request(
            "image_restore",
            vec![
                ("password", Value::str(self.password.clone())),
                ("resume_infos", Value::Array(infos)),
                ("running_task_id", Value::Int(running_task_id)),
                ("subaction", Value::str("get_content")),
                ("username", Value::str(self.username.clone())),
            ],
        );
        let mut c = self.open()?;
        c.set_read_timeout(Some(Duration::from_secs(600)))?;
        c.send(&msg)?;
        loop {
            let m = c.recv()?;
            abb_proto::transport::check_error(&m)?;
            if m.get("close").is_some() {
                if m.get("status").and_then(Value::as_str) != Some("complete") {
                    return Err(Error::Protocol(format!("download ended with status {:?}", string(&m, "status"))));
                }
                sink(Chunk::Done)?;
                return Ok(());
            }
            if m.get("not_finish").is_some() {
                continue;
            }
            let image_path = string(&m, "image_path");
            let (offset, length) = (int(&m, "offset"), int(&m, "length"));
            if length == 0 {
                sink(Chunk::EndOfRange { image_path, offset })?;
                continue;
            }
            if length > MAX_CHUNK {
                return Err(Error::Protocol(format!("offset {offset}: a piece of {length} bytes")));
            }
            let data = match m.get("buffer") {
                Some(Value::Buffer(b)) | Some(Value::String(b)) => {
                    if int(&m, "compression") == 1 {
                        lz4_flex::block::decompress(b, length as usize)
                            .map_err(|e| Error::Protocol(format!("LZ4 at offset {offset}: {e}")))?
                    } else {
                        b.clone()
                    }
                }
                _ => return Err(Error::Protocol(format!("data message without buffer at offset {offset}"))),
            };
            if data.len() as u64 != length {
                return Err(Error::Protocol(format!(
                    "offset {offset}: {} bytes instead of {length}",
                    data.len()
                )));
            }
            sink(Chunk::Data { image_path, offset, data })?;
        }
    }

    /// Tells the server how the restore ended: `status` "complete", "error" or "cancel".
    pub fn finish(&mut self, running_task_id: u64, status: &str, code: u64) -> Result<()> {
        self.call(
            "image_restore",
            vec![
                ("log", Value::map([("code", Value::Int(code))])),
                ("running_task_id", Value::Int(running_task_id)),
                ("status", Value::str(status)),
                ("subaction", Value::str("check_complete")),
            ],
        )?;
        Ok(())
    }
}
