//! IRMM agent: keeps an Iroh connection to the server, reports inventory and serves
//! channels (remote desktop, shell, files) arriving via Iroh or WebRTC.

mod channels;
mod inventory;
mod scripts;
mod state;
pub mod update;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, TransportAddr};
use irmm_proto::{AgentMsg, ServerMsg, read_frame, write_frame};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub use channels::rtc;
pub use channels::{AsyncStream, DesktopProvider, InProcessDesktop, NoDesktop};
pub use state::AgentStateFile;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct Config {
    pub server_url: String,
    pub state_dir: PathBuf,
    /// Required on first start only.
    pub enroll_token: Option<String>,
    pub heartbeat: Duration,
    /// Reported version; tests override it to exercise updates.
    pub version: String,
    pub desktop: Arc<dyn DesktopProvider>,
    pub updater: Arc<dyn update::Updater>,
}

impl Config {
    /// Real platform integrations.
    pub fn production(server_url: String, state_dir: PathBuf, enroll_token: Option<String>) -> Self {
        Self {
            server_url,
            state_dir,
            enroll_token,
            heartbeat: Duration::from_secs(60),
            version: VERSION.to_owned(),
            desktop: Arc::new(NoDesktop),
            updater: Arc::new(update::SelfReplace),
        }
    }

    pub fn for_tests() -> Self {
        Self {
            heartbeat: Duration::from_secs(1),
            ..Self::production(String::new(), PathBuf::new(), None)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentState {
    Connecting,
    Online { device_id: Uuid },
    Rejected { reason: String },
}

pub struct AgentHandle {
    state: watch::Receiver<AgentState>,
    task: JoinHandle<()>,
    endpoint: Endpoint,
}

impl AgentHandle {
    pub fn state(&self) -> AgentState {
        self.state.borrow().clone()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub async fn stop(self) {
        self.task.abort();
        self.endpoint.close().await;
    }

    /// Runs until the agent stops (rejected or aborted).
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

pub async fn start(config: Config) -> anyhow::Result<AgentHandle> {
    let mut file = AgentStateFile::load_or_bootstrap(&config.state_dir, &config.server_url).await?;
    let relay_mode = match &file.relay_url {
        Some(url) => RelayMode::Custom(iroh::RelayMap::from(url.parse::<iroh::RelayUrl>()?)),
        None => RelayMode::Disabled,
    };
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(file.secret_key()?)
        .relay_mode(relay_mode)
        .bind()
        .await
        .context("bind agent endpoint")?;
    let (state_tx, state_rx) = watch::channel(AgentState::Connecting);
    let ep = endpoint.clone();
    let task = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(200);
        loop {
            match session(&config, &mut file, &ep, &state_tx).await {
                Ok(()) => backoff = Duration::from_millis(200),
                Err(SessionEnd::Rejected(reason)) => {
                    tracing::warn!(%reason, "server rejected agent");
                    let _ = state_tx.send(AgentState::Rejected { reason });
                    return;
                }
                Err(SessionEnd::Error(e)) => tracing::debug!(error = %format!("{e:#}"), "agent session ended"),
            }
            let _ = state_tx.send(AgentState::Connecting);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
    Ok(AgentHandle {
        state: state_rx,
        task,
        endpoint,
    })
}

enum SessionEnd {
    Rejected(String),
    Error(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for SessionEnd {
    fn from(e: E) -> Self {
        Self::Error(e.into())
    }
}

pub fn server_addr(file: &AgentStateFile) -> anyhow::Result<EndpointAddr> {
    let id: PublicKey = file.server_id.parse().context("server id")?;
    let mut addrs = Vec::new();
    for a in &file.direct_addrs {
        addrs.push(TransportAddr::Ip(a.parse()?));
    }
    if let Some(r) = &file.relay_url {
        addrs.push(TransportAddr::Relay(r.parse()?));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
}

/// The pieces of agent context channel handlers need.
#[derive(Clone)]
pub struct Ctx {
    pub device_id: Uuid,
    pub server_id: PublicKey,
    pub desktop: Arc<dyn DesktopProvider>,
    pub used_tickets: Arc<std::sync::Mutex<std::collections::HashSet<Uuid>>>,
    pub ice_servers: Vec<String>,
    pub to_server: mpsc::UnboundedSender<AgentMsg>,
}

async fn session(
    config: &Config,
    file: &mut AgentStateFile,
    endpoint: &Endpoint,
    state_tx: &watch::Sender<AgentState>,
) -> Result<(), SessionEnd> {
    let conn: Connection = endpoint.connect(server_addr(file)?, irmm_proto::AGENT_ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let info = inventory::collect(&config.version, config.desktop.monitors());
    let hello = match (file.device_id, &config.enroll_token) {
        (None, Some(token)) => AgentMsg::Enroll { token: token.clone(), info },
        (None, None) => return Err(SessionEnd::Rejected("not enrolled and no enrollment token".into())),
        (Some(_), _) => AgentMsg::Hello { info },
    };
    write_frame(&mut send, &hello).await?;
    let device_id = match read_frame::<_, ServerMsg>(&mut recv).await? {
        Some(ServerMsg::Welcome { device_id }) => device_id,
        Some(ServerMsg::Rejected { reason }) => return Err(SessionEnd::Rejected(reason)),
        other => return Err(anyhow::anyhow!("unexpected server reply {other:?}").into()),
    };
    if file.device_id != Some(device_id) {
        file.device_id = Some(device_id);
        file.save(&config.state_dir)?;
    }
    let _ = state_tx.send(AgentState::Online { device_id });

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AgentMsg>();
    let ctx = Ctx {
        device_id,
        server_id: file.server_id.parse().context("server id")?,
        desktop: config.desktop.clone(),
        used_tickets: Default::default(),
        ice_servers: file.ice_servers.clone(),
        to_server: out_tx.clone(),
    };

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            write_frame(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    });
    let hb_tx = out_tx.clone();
    let (hb_every, version, desktop) = (config.heartbeat, config.version.clone(), config.desktop.clone());
    tasks.spawn(async move {
        loop {
            tokio::time::sleep(hb_every).await;
            let info = tokio::task::spawn_blocking({
                let v = version.clone();
                let monitors = desktop.monitors();
                move || inventory::collect(&v, monitors)
            })
            .await?;
            if hb_tx.send(AgentMsg::Heartbeat { info }).is_err() {
                return anyhow::Ok(());
            }
        }
    });
    let ch_conn = conn.clone();
    let ch_ctx = ctx.clone();
    tasks.spawn(async move {
        loop {
            let (send, recv) = ch_conn.accept_bi().await?;
            let ctx = ch_ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = channels::serve(ctx, Box::new(recv), Box::new(send)).await {
                    tracing::debug!(error = %format!("{e:#}"), "channel ended");
                }
            });
        }
    });
    let updater = config.updater.clone();
    let release_key: PublicKey = file.release_key.parse().context("release key")?;
    let server_url = config.server_url_or(&file.server_url);
    let my_version = config.version.clone();
    tasks.spawn(async move {
        while let Some(msg) = read_frame::<_, ServerMsg>(&mut recv).await? {
            match msg {
                ServerMsg::RunScript { run_id, shell, body, timeout_secs } => {
                    let tx = out_tx.clone();
                    tokio::spawn(async move {
                        let msg = scripts::run(run_id, shell, &body, Duration::from_secs(timeout_secs)).await;
                        let _ = tx.send(msg);
                    });
                }
                ServerMsg::RtcOffer { req_id, sdp } => {
                    let ctx = ctx.clone();
                    let tx = out_tx.clone();
                    tokio::spawn(async move {
                        let sdp = channels::rtc::answer(ctx, sdp).await.map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AgentMsg::RtcAnswer { req_id, sdp });
                    });
                }
                ServerMsg::Update(release) => {
                    if release.version != my_version {
                        let tx = out_tx.clone();
                        let updater = updater.clone();
                        let url = server_url.clone();
                        tokio::spawn(async move {
                            let error = update::apply(&*updater, &url, &release, &release_key).await.err().map(|e| format!("{e:#}"));
                            let _ = tx.send(AgentMsg::UpdateStatus { version: release.version.clone(), error });
                        });
                    }
                }
                ServerMsg::Welcome { .. } | ServerMsg::Rejected { .. } => bail!("unexpected server message"),
            }
        }
        anyhow::Ok(())
    });

    // Any task ending (connection lost) ends the session.
    let res = tasks.join_next().await;
    tasks.abort_all();
    conn.close(0u32.into(), b"bye");
    match res {
        Some(Ok(Err(e))) => Err(e.into()),
        _ => Ok(()),
    }
}

impl Config {
    fn server_url_or(&self, saved: &str) -> String {
        if self.server_url.is_empty() { saved.to_owned() } else { self.server_url.clone() }
    }
}
