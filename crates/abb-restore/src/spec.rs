//! What a backup version contains: the disks and partitions described by the device spec that
//! the agent uploaded, and where their images are on the server.

use crate::server::{array, int, string};
use abb_proto::Value;
use serde_json::Value as Json;

pub struct Task {
    pub device_id: u64,
    pub task_id: u64,
    pub task_name: String,
    pub host_name: String,
    pub os_name: String,
    /// Unix time of the latest version, 0 if none.
    pub latest: u64,
}

impl Task {
    pub fn from(v: &Value) -> Task {
        let dev = v.get("device_info").cloned().unwrap_or(Value::Null);
        let latest = v.get("latest_restore_point").map_or(0, |p| int(p, "time_start"));
        Task {
            device_id: int(&dev, "device_id"),
            task_id: int(v, "task_id"),
            task_name: string(v, "task_name"),
            host_name: string(&dev, "host_name"),
            os_name: string(&dev, "os_name"),
            latest,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Partition {
    pub number: u64,
    pub offset: u64,
    pub length: u64,
    /// Mount path, alias or device name: what to call it.
    pub label: String,
    pub mount_path: String,
    /// Device on the original machine, e.g. "/dev/sda2".
    pub device: String,
    pub fs: String,
    /// GPT attribute bits as the agent reports them: 9 EFI system partition, 65 BIOS boot.
    pub attribute: u64,
    /// Not backed up (unsupported or excluded): restored empty.
    pub excluded: bool,
}

impl Partition {
    pub fn is_esp(&self) -> bool {
        self.attribute == 9
    }

    pub fn is_bios_boot(&self) -> bool {
        self.attribute == 65
    }

    /// Needed to boot: boot partitions and the volumes holding `/` or `/boot`.
    pub fn is_system(&self) -> bool {
        self.is_esp()
            || self.is_bios_boot()
            || matches!(self.mount_path.as_str(), "/" | "/boot" | "/boot/efi" | "/efi")
    }
}

#[derive(Clone, Debug)]
pub struct Disk {
    pub number: u64,
    /// Path on the server, e.g. "ActiveBackup_2026-10-20_141125/sda.img".
    pub image_path: String,
    pub object_id: u64,
    pub size: u64,
    pub sector_size: u64,
    pub gpt: bool,
    pub partitions: Vec<Partition>,
}

impl Disk {
    pub fn name(&self) -> String {
        self.image_path.rsplit('/').next().unwrap_or("").trim_end_matches(".img").to_string()
    }
}

pub struct Version {
    pub id: u64,
    pub time: u64,
    pub bootable: bool,
    pub disks: Vec<Disk>,
}

impl Version {
    /// Builds the versions of a `list_task_version` reply. Versions whose device spec is
    /// missing or unreadable are left out.
    pub fn list(resp: &Value) -> Vec<Version> {
        let specs = resp.get("device_specs");
        let mut out = Vec::new();
        for v in array(resp, "versions") {
            let id = int(v, "version_id");
            let spec = specs.and_then(|s| s.get(&id.to_string())).and_then(|b| match b {
                Value::Buffer(b) | Value::String(b) => serde_json::from_slice::<Json>(b).ok(),
                _ => None,
            });
            let Some(spec) = spec else { continue };
            let objects: Vec<(String, u64, u64)> = array(v, "object_list")
                .iter()
                .map(|o| (string(o, "file_path"), int(o, "object_id"), int(o, "file_size")))
                .collect();
            if let Some(disks) = disks(&spec, &objects) {
                let bootable = v.get("other_spec").is_none_or(|o| int(o, "is_bootable") == 1);
                out.push(Version { id, time: int(v, "time_start"), bootable, disks });
            }
        }
        out
    }
}

fn num(v: &Json) -> u64 {
    match v {
        Json::Number(n) => n.as_u64().unwrap_or(0),
        Json::String(s) => s.parse().unwrap_or(0),
        Json::Bool(b) => *b as u64,
        _ => 0,
    }
}

fn text(v: &Json) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Disks of a device spec file. `objects` are (file path, object id, size) of the version's
/// images.
fn disks(doc: &Json, objects: &[(String, u64, u64)]) -> Option<Vec<Disk>> {
    let spec = &doc["device_spec"];
    let images = spec["images"].as_array()?;
    let layout = spec["disk_layout"].as_object()?;
    let mut out = Vec::new();
    for (n, img) in images.iter().enumerate() {
        let name = text(&img["image_name"]);
        let (path, object_id, size) = objects.iter().find(|(p, _, _)| p.rsplit('/').next() == Some(&name))?.clone();
        let parts = layout.get(&n.to_string()).and_then(Json::as_array).cloned().unwrap_or_default();
        let mut partitions: Vec<Partition> = parts
            .iter()
            .map(|p| {
                let mount_path = text(&p["mount_path"]);
                let alias = text(&p["alias_name"]);
                let device = text(&p["device_name"]);
                let attribute = num(&p["attribute"]);
                Partition {
                    number: num(&p["partition_number"]),
                    offset: num(&p["offset"]),
                    length: num(&p["length"]),
                    label: if !mount_path.is_empty() {
                        mount_path.clone()
                    } else if attribute == 9 {
                        "EFI system partition".into()
                    } else if attribute == 65 {
                        "BIOS boot partition".into()
                    } else if !alias.is_empty() {
                        alias
                    } else {
                        device.clone()
                    },
                    mount_path,
                    device,
                    fs: text(&p["file_system"]),
                    attribute,
                    excluded: p["is_excluded"].as_bool().unwrap_or(false),
                }
            })
            .collect();
        partitions.sort_by_key(|p| p.offset);
        let sector_size = parts.first().map_or(512, |p| num(&p["bytes_per_sector"]).max(512));
        out.push(Disk {
            number: n as u64,
            image_path: path,
            object_id,
            size: size.max(num(&img["length"])),
            sector_size,
            gpt: doc["disk_images"]
                .as_array()
                .and_then(|a| a.iter().find(|d| text(&d["image_name"]) == name))
                .is_none_or(|d| !text(&d["disk_layout"]).eq_ignore_ascii_case("mbr")),
            partitions,
        });
    }
    Some(out)
}
