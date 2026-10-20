//! Information about the computer that the agent reports to the server.

use std::fs;

pub struct Host {
    /// Uppercase SMBIOS UUID, like the official agent (`/sys/class/dmi/id/product_uuid`).
    pub device_uuid: String,
    pub host_name: String,
    /// e.g. "Arch Linux (64-bit)"
    pub os_name: String,
    /// Only major.minor, e.g. "7.2".
    pub kernel_version: String,
    /// Local time zone offset in seconds (like the server's `get_timezone`).
    pub time_zone: i64,
    pub cpu_number: u64,
    /// MiB
    pub memory_size: u64,
}

impl Host {
    pub fn detect(fallback_uuid: Option<&str>) -> Host {
        let device_uuid = fs::read_to_string("/sys/class/dmi/id/product_uuid")
            .map(|s| s.trim().to_uppercase())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| fallback_uuid.map(str::to_string))
            .unwrap_or_else(random_uuid);
        let host_name = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
        let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        let field = |k: &str| {
            os_release
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .map(|v| v.trim_matches('"').to_string())
        };
        let name = field("NAME").unwrap_or_else(|| "Linux".into());
        let os_name = match field("VERSION_ID") {
            Some(v) => format!("{name} {v} (64-bit)"),
            None => format!("{name} (64-bit)"),
        };
        let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let kernel_version = release.trim().split('.').take(2).collect::<Vec<_>>().join(".");
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        unsafe { libc::localtime_r(&now, &mut tm) };
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mem_kib: u64 = meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
            .unwrap_or(0);
        Host {
            device_uuid,
            host_name,
            os_name,
            kernel_version,
            time_zone: tm.tm_gmtoff,
            cpu_number: std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
            memory_size: mem_kib / 1024,
        }
    }
}

pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}
