//! Task settings made in the ABB console, as `get_task` returns them: what to back up (source
//! type), the schedule and backup window, and the pre/post snapshot scripts.

use crate::backup::Selection;
use abb_proto::Value;

/// Server-local time: Unix seconds shifted by the server's UTC offset, so that day and hour
/// arithmetic on it gives the server's wall clock (the schedule is in server time).
pub type ServerTime = i64;

fn wday(t: ServerTime) -> u64 {
    (t.div_euclid(86400) + 4).rem_euclid(7) as u64 // 1970-01-01 was a Thursday; 0 = Sunday
}

fn hour(t: ServerTime) -> u64 {
    (t.rem_euclid(86400) / 3600) as u64
}

/// The `sched_content` of a task.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    /// Bit 1: scheduled; bit 2: events; bit 4: continuous. 0 = manual only.
    pub setting_type: u64,
    /// Days of the week, 0 = Sunday.
    pub weekdays: Vec<u64>,
    pub hour: u64,
    pub minute: u64,
    /// Repeat every hour from `hour` until the end of the day.
    pub hourly: bool,
    /// Allowed hours (index hour + 24 × weekday); `None` = always.
    pub window: Option<Vec<bool>>,
}

impl Schedule {
    fn parse(text: &str) -> Option<Schedule> {
        let j: serde_json::Value = serde_json::from_str(text).ok()?;
        let num = |k: &str| j.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        let window = (j.get("enable_backup_window").and_then(serde_json::Value::as_bool) == Some(true))
            .then(|| j.get("backup_window").and_then(|w| w.as_str()).map(|w| w.chars().map(|c| c == '1').collect()))
            .flatten();
        Some(Schedule {
            setting_type: num("schedule_setting_type"),
            weekdays: j
                .get("run_weekday")
                .and_then(|w| w.as_array())
                .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
                .unwrap_or_default(),
            hour: num("run_hour"),
            minute: num("run_min"),
            hourly: num("repeat_hour") != 0,
            window,
        })
    }

    pub fn is_scheduled(&self) -> bool {
        self.setting_type & 1 != 0
    }

    /// Backups may run at this time (backup window).
    pub fn allows(&self, t: ServerTime) -> bool {
        match &self.window {
            Some(w) => w.get((hour(t) + 24 * wday(t)) as usize).copied().unwrap_or(true),
            None => true,
        }
    }

    /// First scheduled start strictly after `t` (within the next 8 days).
    pub fn next_after(&self, t: ServerTime) -> Option<ServerTime> {
        if !self.is_scheduled() {
            return None;
        }
        let today = t.div_euclid(86400);
        for d in 0..8 {
            let day = (today + d) * 86400;
            if !self.weekdays.contains(&wday(day)) {
                continue;
            }
            let last_hour = if self.hourly { 23 } else { self.hour };
            for h in self.hour..=last_hour {
                let c = day + (h * 3600 + self.minute * 60) as i64;
                if c > t && self.allows(c) {
                    return Some(c);
                }
            }
        }
        None
    }
}

/// When the pre/post scripts' failure stops the backup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScriptMode {
    #[default]
    None,
    RequireSuccess,
    IgnoreFailure,
}

#[derive(Clone, Debug, Default)]
pub struct Scripts {
    pub mode: ScriptMode,
    /// Script run before the snapshot, if enabled: content, or the server's error code when it
    /// could not read it.
    pub pre: Option<Result<Vec<u8>, u64>>,
    /// Script run right after the snapshot.
    pub post: Option<Result<Vec<u8>, u64>>,
}

#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: u64,
    pub uuid: String,
    pub selection: Selection,
    pub schedule: Schedule,
    pub scripts: Scripts,
}

impl TaskInfo {
    pub fn parse(t: &Value) -> Option<TaskInfo> {
        let set = t.get("pre_post_script_setting");
        let flag = |k: &str| set.and_then(|s| s.get(k)).and_then(Value::as_int).unwrap_or(0);
        let script = |which: &str| -> Option<Result<Vec<u8>, u64>> {
            if flag(&format!("{which}_script_enabled")) == 0 {
                return None;
            }
            match t.get(&format!("{which}_script_content")) {
                Some(Value::Buffer(b)) | Some(Value::String(b)) => Some(Ok(b.clone())),
                _ => Some(Err(flag(&format!("{which}_script_error")))),
            }
        };
        let mode = match flag("script_exec_mode") {
            1 => ScriptMode::RequireSuccess,
            2 => ScriptMode::IgnoreFailure,
            _ => ScriptMode::None,
        };
        Some(TaskInfo {
            id: t.get("id")?.as_int()?,
            uuid: t.get("task_uuid").and_then(Value::as_str).unwrap_or("").to_string(),
            selection: Selection::from_task(t),
            schedule: t.get("sched_content").and_then(Value::as_str).and_then(Schedule::parse).unwrap_or_default(),
            scripts: if mode == ScriptMode::None { Scripts::default() } else { Scripts { mode, pre: script("pre"), post: script("post") } },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON_0230: ServerTime = 1_792_377_000; // Monday 2026-10-19 02:30 (as server time)

    fn sched(json: &str) -> Schedule {
        Schedule::parse(json).unwrap()
    }

    #[test]
    fn weekly() {
        let s = sched(r#"{"schedule_setting_type":1,"run_weekday":[1,2,3,4,5],"run_hour":3,"run_min":0,"repeat_hour":0}"#);
        assert_eq!(wday(MON_0230), 1);
        assert_eq!(s.next_after(MON_0230), Some(MON_0230 + 1800)); // Monday 03:00
        // Friday 03:00 → next Monday 03:00.
        let fri = MON_0230 + 4 * 86400 + 1800;
        assert_eq!(s.next_after(fri), Some(MON_0230 + 7 * 86400 + 1800));
    }

    #[test]
    fn hourly_and_window() {
        let mut w = "1".repeat(168);
        // Monday 04:00 not allowed.
        w.replace_range(24 + 4..24 + 5, "0");
        let s = sched(&format!(
            r#"{{"schedule_setting_type":1,"run_weekday":[1],"run_hour":3,"run_min":15,"repeat_hour":1,"enable_backup_window":true,"backup_window":"{w}"}}"#
        ));
        let t1 = s.next_after(MON_0230).unwrap();
        assert_eq!(t1, MON_0230 + 2700); // 03:15
        assert_eq!(s.next_after(t1), Some(t1 + 2 * 3600)); // 05:15, 04:15 is outside the window
        assert!(!s.allows(t1 + 3600));
    }

    #[test]
    fn manual_only() {
        let s = sched(r#"{"schedule_setting_type":0,"run_weekday":[1],"run_hour":3,"run_min":0}"#);
        assert_eq!(s.next_after(MON_0230), None);
    }
}
