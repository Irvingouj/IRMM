//! Signed self-update. The release key is pinned at enrollment, so a compromised
//! server cannot push a binary the operator did not sign.

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use iroh::PublicKey;
use irmm_proto::ReleaseInfo;
use sha2::{Digest as _, Sha256};

#[async_trait]
pub trait Updater: Send + Sync {
    /// Installs a verified binary and arranges for the new version to run.
    async fn install(&self, release: &ReleaseInfo, binary: Vec<u8>) -> anyhow::Result<()>;
}

/// Exit code that tells the service manager to restart us into the new binary.
pub const RESTART_EXIT_CODE: i32 = 75;

/// Replaces the running executable, then exits so the service manager restarts it.
pub struct SelfReplace;

#[async_trait]
impl Updater for SelfReplace {
    async fn install(&self, release: &ReleaseInfo, binary: Vec<u8>) -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let staged = exe.with_file_name(format!("irmm-agent-{}.new", release.version));
        tokio::fs::write(&staged, &binary).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).await?;
        }
        self_replace::self_replace(&staged).context("replace executable")?;
        let _ = tokio::fs::remove_file(&staged).await;
        tracing::info!(version = %release.version, "agent updated; restarting");
        tokio::spawn(async {
            // Let the UpdateStatus frame reach the server first.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            std::process::exit(RESTART_EXIT_CODE);
        });
        Ok(())
    }
}

pub async fn apply(updater: &dyn Updater, server_url: &str, release: &ReleaseInfo, key: &PublicKey) -> anyhow::Result<()> {
    release.verify(key)?;
    if release.platform != irmm_proto::current_platform() {
        bail!("release is for {}, this agent is {}", release.platform, irmm_proto::current_platform());
    }
    let url = format!("{}{}", server_url.trim_end_matches('/'), release.url);
    let binary = reqwest::get(&url).await?.error_for_status()?.bytes().await?.to_vec();
    let digest = hex::encode(Sha256::digest(&binary));
    if digest != release.sha256 {
        bail!("release digest mismatch");
    }
    updater.install(release, binary).await
}
