//! `irmm-agent`: run in the foreground, install or uninstall the system service.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use clap::{Parser, Subcommand};
use irmm_agent::{AgentState, Config, InProcessDesktop, install};

#[derive(Parser)]
#[command(name = "irmm-agent", version, about = "IRMM remote management agent")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground (service managers start this).
    Run {
        /// Server URL; only needed before enrollment.
        #[arg(long)]
        server: Option<String>,
        /// Site enrollment token; only needed before enrollment.
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Enroll this machine and install the agent as a system service (needs root/Administrator).
    Install {
        #[arg(long)]
        server: String,
        #[arg(long)]
        token: String,
    },
    /// Stop and remove the service, binary and state.
    Uninstall,
    /// Windows Service Control Manager entry point.
    #[command(hide = true)]
    Service,
}

fn config(server: Option<String>, token: Option<String>, state_dir: PathBuf) -> Config {
    let mut cfg = Config::production(server.unwrap_or_default(), state_dir, token);
    if let Some(d) = irmm_desktop::platform::native() {
        cfg.desktop = Arc::new(InProcessDesktop(d));
    }
    cfg
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=warn".into()))
        .with_writer(std::io::stderr)
        .init();
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let agent = irmm_agent::start(cfg).await?;
    match agent.wait().await {
        AgentState::Rejected { reason } => bail!("server rejected this agent: {reason}"),
        _ => Ok(()),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Cmd::Service = cli.command {
        #[cfg(windows)]
        return winsvc::run();
        #[cfg(not(windows))]
        bail!("`service` is only used on Windows; use `run`");
    }
    init_logging();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match cli.command {
            Cmd::Run { server, token, state_dir } => {
                run(config(server, token, state_dir.unwrap_or_else(|| install::default_paths().state_dir))).await
            }
            Cmd::Install { server, token } => {
                let device = install::install(&server, &token, config(None, None, PathBuf::new())).await?;
                println!("IRMM agent installed and enrolled (device {device}).");
                Ok(())
            }
            Cmd::Uninstall => {
                install::uninstall()?;
                println!("IRMM agent uninstalled.");
                Ok(())
            }
            Cmd::Service => unreachable!(),
        }
    })
}

#[cfg(windows)]
mod winsvc {
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::*;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> anyhow::Result<()> {
        service_dispatcher::start(install::SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }

    fn init_file_logging(dir: &std::path::Path) {
        let _ = std::fs::create_dir_all(dir);
        let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("agent.log"));
        let builder = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=warn".into()))
            .with_ansi(false);
        match file {
            Ok(f) => builder.with_writer(Mutex::new(f)).init(),
            Err(_) => builder.init(),
        }
    }

    fn service_main(_args: Vec<OsString>) {
        let paths = install::default_paths();
        init_file_logging(&paths.state_dir);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let stop_tx = Mutex::new(Some(stop_tx));
        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(tx) = stop_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let Ok(status) = service_control_handler::register(install::SERVICE_NAME, handler) else { return };
        let set = |state, accept, code| {
            let _ = status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(code),
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            });
        };
        set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, 0);
        let result = tokio::runtime::Runtime::new().map(|rt| {
            rt.block_on(async {
                tokio::select! {
                    r = super::run(config(None, None, paths.state_dir.clone())) => r,
                    _ = stop_rx => Ok(()),
                }
            })
        });
        let failed = !matches!(result, Ok(Ok(())));
        if let Ok(Err(e)) | Err(e) = result.map_err(anyhow::Error::from) {
            tracing::error!(error = %format!("{e:#}"), "agent stopped");
        }
        set(ServiceState::Stopped, ServiceControlAccept::empty(), if failed { 1 } else { 0 });
    }
}
