//! Installs the agent as a system service: systemd (Linux), launchd (macOS) or the
//! Service Control Manager (Windows). The service restarts the agent whenever it
//! exits, which is also how a self-update takes effect.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::{AgentState, AgentStateFile};

pub const SERVICE_NAME: &str = if cfg!(windows) { "IRMMAgent" } else if cfg!(target_os = "macos") { "com.irmm.agent" } else { "irmm-agent" };

pub struct Paths {
    pub binary: PathBuf,
    pub state_dir: PathBuf,
}

pub fn default_paths() -> Paths {
    if cfg!(windows) {
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
        let pd = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into());
        Paths { binary: pf.join("IRMM").join("irmm-agent.exe"), state_dir: pd.join("IRMM") }
    } else if cfg!(target_os = "macos") {
        let base = PathBuf::from("/Library/Application Support/IRMM");
        Paths { binary: base.join("irmm-agent"), state_dir: base.join("state") }
    } else {
        Paths { binary: "/usr/local/bin/irmm-agent".into(), state_dir: "/var/lib/irmm-agent".into() }
    }
}

/// Enrolls (so a bad token fails here, not silently in the background), then
/// copies this executable into place and registers and starts the service.
pub async fn install(server_url: &str, token: &str, config: crate::Config) -> anyhow::Result<uuid::Uuid> {
    let paths = default_paths();
    // Re-installing over a running agent: stop it so the binary can be replaced.
    stop_service();
    let device_id = enroll(server_url, token, &paths.state_dir, config).await?;
    let exe = std::env::current_exe()?;
    if let Some(dir) = paths.binary.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !same_file(&exe, &paths.binary) {
        std::fs::copy(&exe, &paths.binary).with_context(|| format!("copy agent to {}", paths.binary.display()))?;
    }
    register_service(&paths)?;
    Ok(device_id)
}

async fn enroll(server_url: &str, token: &str, state_dir: &Path, mut config: crate::Config) -> anyhow::Result<uuid::Uuid> {
    std::fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    restrict_dir(state_dir)?;
    if let Some(saved) = AgentStateFile::load(state_dir)? {
        if saved.server_url != server_url.trim_end_matches('/') {
            // Moving to another server: start over with a new identity.
            std::fs::remove_file(state_dir.join("agent.json"))?;
        }
    }
    config.server_url = server_url.to_owned();
    config.state_dir = state_dir.to_owned();
    config.enroll_token = Some(token.to_owned());
    let mut agent = crate::start(config).await?;
    let state = tokio::time::timeout(Duration::from_secs(60), agent.settled()).await;
    agent.stop().await;
    match state {
        Ok(AgentState::Online { device_id }) => Ok(device_id),
        Ok(AgentState::Rejected { reason }) => {
            let _ = std::fs::remove_file(state_dir.join("agent.json"));
            bail!("enrollment rejected: {reason}")
        }
        _ => bail!("could not reach the server at {server_url} within 60 s"),
    }
}

pub fn uninstall() -> anyhow::Result<()> {
    let paths = default_paths();
    stop_service();
    unregister_service()?;
    let _ = std::fs::remove_dir_all(&paths.state_dir);
    let exe = std::env::current_exe()?;
    if same_file(&exe, &paths.binary) {
        self_replace::self_delete()?;
    } else if paths.binary.exists() {
        std::fs::remove_file(&paths.binary)?;
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}{}", args.join(" "), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const UNIT_PATH: &str = "/etc/systemd/system/irmm-agent.service";

#[cfg(target_os = "linux")]
fn register_service(paths: &Paths) -> anyhow::Result<()> {
    let unit = format!(
        "[Unit]\nDescription=IRMM agent\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nExecStart=\"{}\" run --state-dir \"{}\"\nRestart=always\nRestartSec=5\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        paths.binary.display(),
        paths.state_dir.display()
    );
    std::fs::write(UNIT_PATH, unit).context("write systemd unit")?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", SERVICE_NAME])
}

#[cfg(target_os = "linux")]
fn stop_service() {
    let _ = run("systemctl", &["stop", SERVICE_NAME]);
}

#[cfg(target_os = "linux")]
fn unregister_service() -> anyhow::Result<()> {
    let _ = run("systemctl", &["disable", SERVICE_NAME]);
    let _ = std::fs::remove_file(UNIT_PATH);
    let _ = run("systemctl", &["daemon-reload"]);
    Ok(())
}

#[cfg(target_os = "macos")]
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.irmm.agent.plist";

#[cfg(target_os = "macos")]
fn register_service(paths: &Paths) -> anyhow::Result<()> {
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{SERVICE_NAME}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>run</string><string>--state-dir</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardErrorPath</key><string>/Library/Logs/irmm-agent.log</string>
</dict>
</plist>
"#,
        paths.binary.display(),
        paths.state_dir.display()
    );
    std::fs::write(PLIST_PATH, plist).context("write launchd plist")?;
    run("launchctl", &["bootstrap", "system", PLIST_PATH])
}

#[cfg(target_os = "macos")]
fn stop_service() {
    let _ = run("launchctl", &["bootout", &format!("system/{SERVICE_NAME}")]);
}

#[cfg(target_os = "macos")]
fn unregister_service() -> anyhow::Result<()> {
    let _ = std::fs::remove_file(PLIST_PATH);
    Ok(())
}

#[cfg(windows)]
fn register_service(paths: &Paths) -> anyhow::Result<()> {
    let bin = format!("\"{}\" service", paths.binary.display());
    if run("sc.exe", &["query", SERVICE_NAME]).is_err() {
        run("sc.exe", &["create", SERVICE_NAME, "binPath=", &bin, "start=", "auto", "obj=", "LocalSystem", "DisplayName=", "IRMM Agent"])?;
    } else {
        run("sc.exe", &["config", SERVICE_NAME, "binPath=", &bin, "start=", "auto"])?;
    }
    // Restart on any exit (crash or self-update).
    run("sc.exe", &["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/5000/restart/5000/restart/5000"])?;
    run("sc.exe", &["failureflag", SERVICE_NAME, "1"])?;
    // Lets a service generate Ctrl+Alt+Del (SendSAS) for remote desktop.
    run(
        "reg.exe",
        &[
            "add",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System",
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ],
    )?;
    run("sc.exe", &["start", SERVICE_NAME])
}

#[cfg(windows)]
fn stop_service() {
    if run("sc.exe", &["stop", SERVICE_NAME]).is_ok() {
        for _ in 0..30 {
            let out = Command::new("sc.exe").args(["query", SERVICE_NAME]).output();
            if out.map(|o| String::from_utf8_lossy(&o.stdout).contains("STOPPED")).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

#[cfg(windows)]
fn unregister_service() -> anyhow::Result<()> {
    let _ = run("sc.exe", &["delete", SERVICE_NAME]);
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    let d = dir.display().to_string();
    run("icacls.exe", &[&d, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
}
