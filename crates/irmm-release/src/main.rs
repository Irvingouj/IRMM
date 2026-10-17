//! Operator tool for agent releases: generate the offline release key, sign a build,
//! publish it. Agents pin the release public key at enrollment and refuse anything
//! not signed with it. The operator token is read from `IRMM_OPERATOR_TOKEN`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use sha2::{Digest as _, Sha256};

#[derive(Parser)]
#[command(name = "irmm-release", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a release signing key; prints its public key (IRMM_RELEASE_PUBLIC_KEY).
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the signature for a build.
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Sign a build and upload it; online agents on that platform update to it.
    Publish {
        #[arg(long)]
        server: String,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        version: String,
        /// e.g. linux-x86_64, macos-aarch64, windows-x86_64
        #[arg(long)]
        platform: String,
        #[arg(long)]
        file: PathBuf,
    },
}

fn read_key(path: &Path) -> anyhow::Result<iroh::SecretKey> {
    std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .parse()
        .context("parse release key")
}

fn sign(key: &Path, version: &str, platform: &str, file: &Path) -> anyhow::Result<(Vec<u8>, String)> {
    let key = read_key(key)?;
    let binary = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let sha = hex::encode(Sha256::digest(&binary));
    Ok((binary, irmm_proto::sign_release(&key, version, platform, &sha)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::Keygen { out } => {
            if out.exists() {
                bail!("{} already exists; refusing to overwrite a release key", out.display());
            }
            let key = iroh::SecretKey::generate();
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            std::io::Write::write_all(&mut f, hex::encode(key.to_bytes()).as_bytes())?;
            println!("{}", key.public());
        }
        Cmd::Sign { key, version, platform, file } => {
            let (_, sig) = sign(&key, &version, &platform, &file)?;
            println!("{sig}");
        }
        Cmd::Publish { server, key, version, platform, file } => {
            let token = std::env::var("IRMM_OPERATOR_TOKEN").context("IRMM_OPERATOR_TOKEN is required")?;
            let (binary, sig) = sign(&key, &version, &platform, &file)?;
            let url = format!("{}/api/operator/releases", server.trim_end_matches('/'));
            let r = reqwest::Client::new()
                .post(url)
                .query(&[("platform", &platform), ("version", &version), ("signature", &sig)])
                .bearer_auth(token)
                .body(binary)
                .send()
                .await?;
            if !r.status().is_success() {
                bail!("publish failed: {} {}", r.status(), r.text().await.unwrap_or_default());
            }
            println!("published {platform} {version}");
        }
    }
    Ok(())
}
