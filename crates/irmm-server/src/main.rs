//! `irmm-server`, configured from the environment:
//!
//! | Variable | Default | |
//! |---|---|---|
//! | `IRMM_DATABASE_URL` | required | Postgres URL |
//! | `IRMM_RELEASE_PUBLIC_KEY` | required | from `irmm-release keygen` |
//! | `IRMM_HTTP_ADDR` | `0.0.0.0:8080` | API, console, gateway |
//! | `IRMM_IROH_ADDR` | `0.0.0.0:7843` | UDP, agents |
//! | `IRMM_IROH_PUBLIC_ADDRS` | public URL host + Iroh port | comma-separated `host:port` agents dial |
//! | `IRMM_PUBLIC_URL` | `http://<http addr>` | URL agents and browsers use |
//! | `IRMM_DATA_DIR` | `./data` | server key, releases |
//! | `IRMM_STATIC_DIR` | unset | built web console |
//! | `IRMM_RELAY_URL` | unset | self-hosted iroh-relay |
//! | `IRMM_ICE_SERVERS` | `stun:stun.l.google.com:19302` | comma-separated STUN/TURN URLs |
//! | `IRMM_OPERATOR_TOKEN` | unset | enables release publishing |

use std::path::{Path, PathBuf};

use anyhow::Context as _;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn required(name: &str) -> anyhow::Result<String> {
    var(name).filter(|v| !v.is_empty()).with_context(|| format!("{name} is required"))
}

fn list(v: String) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// The server identity: agents pin its public half, so it must survive restarts.
fn load_or_create_key(path: &Path) -> anyhow::Result<iroh::SecretKey> {
    if let Ok(s) = std::fs::read_to_string(path) {
        return s.trim().parse().with_context(|| format!("parse {}", path.display()));
    }
    let key = iroh::SecretKey::generate();
    std::fs::write(path, hex::encode(key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=warn".into()))
        .init();
    let data_dir = PathBuf::from(var("IRMM_DATA_DIR").unwrap_or_else(|| "data".into()));
    std::fs::create_dir_all(&data_dir)?;
    let config = irmm_server::Config {
        database_url: required("IRMM_DATABASE_URL")?,
        http_addr: var("IRMM_HTTP_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().context("IRMM_HTTP_ADDR")?,
        iroh_addr: var("IRMM_IROH_ADDR").unwrap_or_else(|| "0.0.0.0:7843".into()).parse().context("IRMM_IROH_ADDR")?,
        iroh_public_addrs: list(var("IRMM_IROH_PUBLIC_ADDRS").unwrap_or_default()),
        public_url: var("IRMM_PUBLIC_URL").filter(|v| !v.is_empty()).map(|u| u.trim_end_matches('/').to_owned()),
        secret_key: load_or_create_key(&data_dir.join("server.key"))?,
        relay_url: var("IRMM_RELAY_URL").filter(|v| !v.is_empty()),
        release_key: required("IRMM_RELEASE_PUBLIC_KEY")?.parse().context("IRMM_RELEASE_PUBLIC_KEY")?,
        static_dir: var("IRMM_STATIC_DIR").filter(|v| !v.is_empty()).map(PathBuf::from),
        ice_servers: list(var("IRMM_ICE_SERVERS").unwrap_or_else(|| "stun:stun.l.google.com:19302".into())),
        operator_token: var("IRMM_OPERATOR_TOKEN").filter(|v| !v.is_empty()),
        data_dir,
    };
    let server = irmm_server::start(config).await?;
    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}
