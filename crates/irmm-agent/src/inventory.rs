use irmm_proto::{DeviceInfo, DiskInfo};
use sysinfo::{Disks, Networks, System};

pub fn collect(agent_version: &str) -> DeviceInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    let disks = Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| DiskInfo {
            mount: d.mount_point().display().to_string(),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
        })
        .collect();
    let mut ips: Vec<String> = Networks::new_with_refreshed_list()
        .values()
        .flat_map(|n| n.ip_networks().iter().map(|ip| ip.addr))
        .filter(|ip| !ip.is_loopback())
        .map(|ip| ip.to_string())
        .collect();
    ips.sort();
    ips.dedup();
    DeviceInfo {
        hostname: System::host_name().unwrap_or_default(),
        os: std::env::consts::OS.to_owned(),
        os_version: System::long_os_version().unwrap_or_default(),
        arch: std::env::consts::ARCH.to_owned(),
        agent_version: agent_version.to_owned(),
        cpu_model: cpus.first().map(|c| c.brand().trim().to_owned()).unwrap_or_default(),
        cpu_cores: cpus.len() as u32,
        memory_bytes: sys.total_memory(),
        disks,
        ips,
        logged_in_users: logged_in_users(),
    }
}

#[cfg(unix)]
fn logged_in_users() -> Vec<String> {
    let Ok(out) = std::process::Command::new("who").output() else { return vec![] };
    let mut users: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect();
    users.sort();
    users.dedup();
    users
}

#[cfg(windows)]
fn logged_in_users() -> Vec<String> {
    // `query user` is fixed-width and user names may contain spaces, so slice by the header columns.
    let Ok(out) = std::process::Command::new("query").arg("user").output() else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let Some(end) = lines.next().and_then(|h| h.find("SESSIONNAME")) else { return vec![] };
    let mut users: Vec<String> = lines
        .filter_map(|l| l.get(1..end.min(l.len())).map(|u| u.trim().to_owned()))
        .filter(|u| !u.is_empty())
        .collect();
    users.sort();
    users.dedup();
    users
}
