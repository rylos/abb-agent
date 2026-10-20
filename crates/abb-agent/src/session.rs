//! Session with the server: initial pairing, authenticated requests, events.

use crate::host::Host;
use crate::state::State;
use abb_proto::transport::{Error, Result, fingerprint_hex};
use abb_proto::{Connection, Value, request};

pub struct Session {
    pub state: State,
    pub host: Host,
}

/// An authenticated request: `ds_id` and `token` are added automatically.
pub fn authed(state: &State, action: &str, mut fields: Vec<(&'static str, Value)>) -> Value {
    fields.push(("ds_id", Value::str(state.ds_id.clone())));
    fields.push(("token", Value::str(state.token.clone())));
    request(action, fields)
}

fn int(v: &Value, k: &str) -> Result<u64> {
    v.get(k).and_then(Value::as_int).ok_or_else(|| Error::Protocol(format!("missing field {k}")))
}

fn string(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Protocol(format!("missing field {k}")))
}

/// Initial pairing: `auth` with username and password, then `confirm_auth`, which creates
/// the device and the task (from the default template) on the server.
pub fn pair(server: &str, port: u16, username: &str, password: &str, otp: Option<&str>) -> Result<State> {
    let mut c = Connection::open(server, port, None)?;
    let fingerprint = c.fingerprint;
    let info = c.call(&request("query_server_info", vec![]))?;
    drop(c);
    let host = Host::detect(None);
    eprintln!(
        "server {} (ds_id {}), certificate {}",
        server,
        string(&info, "ds_id")?,
        fingerprint_hex(&fingerprint)
    );

    let mut c = Connection::open(server, port, Some(fingerprint))?;
    let resp = c.call(&request(
        "auth",
        vec![
            ("device_uuid", Value::str(host.device_uuid.clone())),
            ("driver_status", Value::str("enable")),
            ("host_name", Value::str(host.host_name.clone())),
            ("os_name", Value::str(host.os_name.clone())),
            ("otp", otp.map_or(Value::Null, Value::str)),
            ("password", Value::str(password)),
            ("username", Value::str(username)),
        ],
    ))?;
    let state = State {
        server: server.to_string(),
        port,
        fingerprint: fingerprint_hex(&fingerprint),
        ds_id: string(&resp, "ds_id")?,
        token: string(&resp, "token")?,
        username: username.to_string(),
        device_uuid: host.device_uuid.clone(),
        last_event_id: 0,
    };
    let template_id = resp
        .get("task_template")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_int)
        .ok_or_else(|| Error::Protocol("task_template missing from response".into()))?;
    drop(c);

    let mut c = Connection::open(server, port, Some(fingerprint))?;
    c.call(&authed(
        &state,
        "confirm_auth",
        vec![
            ("backup_type", Value::Int(3)),
            ("connect_type", Value::str("backup")),
            // Sic: the official agent spells the field this way.
            ("drvier_status", Value::str("enable")),
            ("dsm_model", Value::str("")),
            ("dsm_unique", Value::str("")),
            ("kernel_version", Value::str(host.kernel_version.clone())),
            ("template_id", Value::Int(template_id)),
            ("time_zone", Value::Int(host.time_zone as u64)),
        ],
    ))?;
    Ok(state)
}

impl Session {
    pub fn new(state: State) -> Session {
        let host = Host::detect(Some(&state.device_uuid));
        Session { state, host }
    }

    pub fn connect(&self) -> Result<Connection> {
        Connection::open(&self.state.server, self.state.port, self.state.pin())
    }

    /// `auth` with the token (with `init:1` at startup, then periodically).
    pub fn auth_by_token(&self, init: bool) -> Result<()> {
        let mut c = self.connect()?;
        c.call(&authed(
            &self.state,
            "auth",
            vec![
                ("device_uuid", Value::str(self.state.device_uuid.clone())),
                ("driver_status", Value::str("enable")),
                ("host_name", Value::str(self.host.host_name.clone())),
                ("init", Value::Int(init as u64)),
                ("kernel_version", Value::str(self.host.kernel_version.clone())),
                ("os_name", Value::str(self.host.os_name.clone())),
                ("time_zone", Value::Int(self.host.time_zone as u64)),
                ("username", Value::str(self.state.username.clone())),
            ],
        ))?;
        Ok(())
    }

    /// Offset to add to the local Unix time to get the server's wall clock as Unix seconds
    /// (UTC offset of the server plus clock difference), from `get_timezone`.
    pub fn server_offset(&self, c: &mut Connection) -> Result<i64> {
        let r = self.call(c, "get_timezone", vec![])?;
        let num = |k: &str| -> Option<i64> {
            match r.get(k)? {
                Value::Int(n) => Some(*n as i64),
                v => v.as_str()?.trim().parse().ok(),
            }
        };
        let epoch = num("epoch").ok_or_else(|| Error::Protocol("get_timezone: epoch missing".into()))?;
        let tz = num("timezone").unwrap_or(0);
        Ok(tz + epoch - crate::host::now() as i64)
    }

    pub fn heart_beat(&self, c: &mut Connection) -> Result<()> {
        c.call(&authed(&self.state, "heart_beat", vec![("driver_status", Value::str("enable"))]))?;
        Ok(())
    }

    pub fn call(&self, c: &mut Connection, action: &str, fields: Vec<(&'static str, Value)>) -> Result<Value> {
        c.call(&authed(&self.state, action, fields))
    }

    /// Waits up to `timeout` seconds for an event; `true` if there are any.
    pub fn long_poll(&self, c: &mut Connection, timeout: u64) -> Result<bool> {
        let r = self.call(
            c,
            "long_poll",
            vec![("last_event_id", Value::Int(self.state.last_event_id)), ("timeout", Value::Int(timeout))],
        )?;
        Ok(r.get("has_event").and_then(Value::as_int) == Some(1))
    }

    /// Pending events.
    pub fn pull_events(&self, c: &mut Connection) -> Result<Vec<Event>> {
        let r = self.call(c, "pull_event", vec![("event_list", Value::Array(vec![]))])?;
        let Some(Value::Array(list)) = r.get("event_list") else { return Ok(vec![]) };
        Ok(list
            .iter()
            .filter_map(|e| {
                Some(Event {
                    id: e.get("id")?.as_int()?,
                    action: e.get("action")?.as_str()?.to_string(),
                    task_id: e.get("task_id").and_then(Value::as_int).unwrap_or(0),
                    body: e.clone(),
                })
            })
            .collect())
    }

    pub fn done_event(&self, c: &mut Connection, id: u64) -> Result<()> {
        self.call(c, "done_event", vec![("done_events", Value::Array(vec![Value::Int(id)]))])?;
        Ok(())
    }

    /// Tasks waiting to be run (`backup_task` with `status:0`).
    pub fn running_tasks(&self, c: &mut Connection) -> Result<Vec<RunningTask>> {
        let r = self.call(c, "get_running_task", vec![])?;
        let Some(Value::Array(list)) = r.get("running_task_list") else { return Ok(vec![]) };
        Ok(list
            .iter()
            .filter_map(|e| {
                let bt = e.get("backup_task")?;
                Some(RunningTask {
                    running_task_id: bt.get("running_task_id")?.as_int()?,
                    task_id: bt.get("task_id")?.as_int()?,
                    task_uuid: bt.get("task_uuid").and_then(Value::as_str).unwrap_or("").to_string(),
                    status: e.get("status").and_then(Value::as_int).unwrap_or(0),
                })
            })
            .collect())
    }

    pub fn tasks(&self, c: &mut Connection) -> Result<Vec<Value>> {
        let r = self.call(c, "get_task", vec![])?;
        match r.get("task_list") {
            Some(Value::Array(list)) => Ok(list.clone()),
            _ => Ok(vec![]),
        }
    }
}

/// An event from `pull_event`.
#[derive(Clone, Debug)]
pub struct Event {
    pub id: u64,
    pub action: String,
    pub task_id: u64,
    /// The whole event (e.g. `webapi_task` for `act_client`).
    pub body: Value,
}

#[derive(Clone, Debug)]
pub struct RunningTask {
    pub running_task_id: u64,
    pub task_id: u64,
    pub task_uuid: String,
    /// 0 = to start, 1 = running.
    pub status: u64,
}

pub fn get_int(v: &Value, k: &str) -> Result<u64> {
    int(v, k)
}
