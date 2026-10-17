//! IRMM server: HTTP API, agent hub (Iroh), WebRTC signaling and WSS gateway.

mod agents;
mod api;
mod auth;
mod db;
mod error;
mod gateway;
mod install;
mod jobs;
mod releases;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use iroh::{Endpoint, PublicKey, RelayMode, SecretKey, endpoint::presets};
use sqlx::postgres::PgPoolOptions;
use tokio::task::JoinHandle;

pub use agents::Agents;

pub struct Config {
    pub database_url: String,
    pub http_addr: SocketAddr,
    /// UDP address of the agent-facing Iroh endpoint.
    pub iroh_addr: SocketAddr,
    /// Externally reachable base URL, used in install commands. Defaults to `http://<http_addr>`.
    pub public_url: Option<String>,
    /// Server identity. Its public half is the `server_id` agents pin, and it signs session tickets.
    pub secret_key: SecretKey,
    /// Self-hosted iroh-relay. `None` disables relaying (direct connections only).
    pub relay_url: Option<String>,
    /// Public key agents use to verify release signatures.
    pub release_key: PublicKey,
    /// Release binaries live here.
    pub data_dir: PathBuf,
    /// Built web console (`web/dist`).
    pub static_dir: Option<PathBuf>,
    /// STUN/TURN URLs handed to agents and browsers for WebRTC.
    pub ice_servers: Vec<String>,
}

pub struct AppState {
    pub pool: sqlx::PgPool,
    pub endpoint: Endpoint,
    pub secret_key: SecretKey,
    pub agents: Agents,
    pub public_url: String,
    pub relay_url: Option<String>,
    pub release_key: PublicKey,
    pub data_dir: PathBuf,
    pub ice_servers: Vec<String>,
}

pub type State = Arc<AppState>;

pub struct ServerHandle {
    pub http_addr: SocketAddr,
    pub server_id: PublicKey,
    pub state: State,
    tasks: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        self.state.endpoint.close().await;
    }
}

pub async fn start(config: Config) -> anyhow::Result<ServerHandle> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .context("connect to postgres")?;
    sqlx::migrate!("./migrations").run(&pool).await.context("run migrations")?;

    let relay_mode = match &config.relay_url {
        Some(url) => RelayMode::Custom(iroh::RelayMap::from(url.parse::<iroh::RelayUrl>()?)),
        None => RelayMode::Disabled,
    };
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(config.secret_key.clone())
        .alpns(vec![irmm_proto::AGENT_ALPN.to_vec()])
        .relay_mode(relay_mode)
        .bind_addr(config.iroh_addr)?
        .bind()
        .await
        .context("bind iroh endpoint")?;

    let listener = tokio::net::TcpListener::bind(config.http_addr).await?;
    let http_addr = listener.local_addr()?;
    std::fs::create_dir_all(&config.data_dir)?;

    let state = Arc::new(AppState {
        pool,
        endpoint: endpoint.clone(),
        secret_key: config.secret_key.clone(),
        agents: Agents::default(),
        public_url: config.public_url.clone().unwrap_or_else(|| format!("http://{http_addr}")),
        relay_url: config.relay_url.clone(),
        release_key: config.release_key,
        data_dir: config.data_dir.clone(),
        ice_servers: config.ice_servers.clone(),
    });

    let app = api::router(state.clone(), config.static_dir.as_deref());
    let http = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "http server stopped");
        }
    });
    let hub = tokio::spawn(agents::accept_loop(state.clone()));

    tracing::info!(%http_addr, server_id = %endpoint.id(), "irmm server started");
    Ok(ServerHandle {
        http_addr,
        server_id: endpoint.id(),
        state,
        tasks: vec![http, hub],
    })
}
