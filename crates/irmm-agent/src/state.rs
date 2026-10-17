//! Persistent agent identity: Iroh secret key, pinned server id and release key.

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateFile {
    pub server_url: String,
    secret_key: String,
    pub server_id: String,
    pub direct_addrs: Vec<String>,
    pub relay_url: Option<String>,
    pub release_key: String,
    #[serde(default)]
    pub ice_servers: Vec<String>,
    pub device_id: Option<Uuid>,
}

const FILE: &str = "agent.json";

impl AgentStateFile {
    pub fn secret_key(&self) -> anyhow::Result<iroh::SecretKey> {
        let bytes: [u8; 32] = hex::decode(&self.secret_key)?.try_into().map_err(|_| anyhow::anyhow!("key length"))?;
        Ok(iroh::SecretKey::from_bytes(&bytes))
    }

    pub fn load(dir: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read(dir.join(FILE)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).context("parse agent.json")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Loads the saved identity, or fetches the server bootstrap and creates one.
    pub async fn load_or_bootstrap(dir: &Path, server_url: &str) -> anyhow::Result<Self> {
        if let Some(s) = Self::load(dir)? {
            return Ok(s);
        }
        anyhow::ensure!(!server_url.is_empty(), "no saved state and no server URL");
        let b = fetch_bootstrap(server_url).await?;
        let s = Self {
            server_url: server_url.trim_end_matches('/').to_owned(),
            secret_key: hex::encode(iroh::SecretKey::generate().to_bytes()),
            server_id: b.server_id,
            direct_addrs: b.direct_addrs,
            relay_url: b.relay_url,
            release_key: b.release_key,
            ice_servers: b.ice_servers,
            device_id: None,
        };
        s.save(dir)?;
        Ok(s)
    }

    /// Re-reads the server's addresses from its bootstrap endpoint. The server and
    /// release keys pinned at enrollment must not change.
    pub async fn refresh_addrs(&mut self, dir: &Path, server_url: &str) -> anyhow::Result<()> {
        let b = fetch_bootstrap(server_url).await?;
        anyhow::ensure!(b.server_id == self.server_id && b.release_key == self.release_key, "server identity changed");
        if (&b.direct_addrs, &b.relay_url, &b.ice_servers) != (&self.direct_addrs, &self.relay_url, &self.ice_servers) {
            self.direct_addrs = b.direct_addrs;
            self.relay_url = b.relay_url;
            self.ice_servers = b.ice_servers;
            self.save(dir)?;
        }
        Ok(())
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE);
        let tmp = dir.join("agent.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        restrict_permissions(&tmp);
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_permissions(p: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_p: &Path) {
    // The state directory under %ProgramData% is ACL'd to SYSTEM/Administrators by the installer.
}

async fn fetch_bootstrap(server_url: &str) -> anyhow::Result<irmm_proto::Bootstrap> {
    let url = format!("{}/api/agent/bootstrap", server_url.trim_end_matches('/'));
    reqwest::get(&url).await?.error_for_status()?.json().await.context("bootstrap")
}
