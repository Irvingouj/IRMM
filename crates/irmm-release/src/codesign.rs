//! OS-level code signing so the *initial* install is trusted by the operating
//! system (macOS Gatekeeper, Windows SmartScreen). This is separate from the
//! ed25519 release signature in [`crate::sign`], which is what the agent itself
//! verifies on auto-update: the two protect different trust boundaries.
//!
//! Signing material is never hard-coded; it is read from the environment, which
//! the operator populates from Infisical. When a required secret is absent the
//! signer says exactly which variable to set rather than silently shipping an
//! unsigned binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

/// Signs a built binary for one target platform.
pub trait Signer {
    /// Signs `file` in place.
    fn sign(&self, file: &Path) -> anyhow::Result<()>;
    /// Confirms `file` carries an intact signature.
    fn verify(&self, file: &Path) -> anyhow::Result<()>;
}

/// Selects the signer for a release platform string (`macos-aarch64`,
/// `windows-x86_64`, `linux-x86_64`, …), reading its credentials from the
/// environment. Returns an error naming the missing Infisical secret when the
/// platform needs one and it is not set.
pub fn for_platform(platform: &str) -> anyhow::Result<Box<dyn Signer>> {
    let os = platform.split('-').next().unwrap_or(platform);
    match os {
        "macos" => {
            // "-" is an ad-hoc signature (dev/CI without a Developer ID cert);
            // a real release sets the Developer ID identity from Infisical.
            let identity = env("IRMM_MACOS_SIGN_IDENTITY")?;
            Ok(Box::new(MacCodesign { identity }))
        }
        "windows" => {
            let pkcs12 = PathBuf::from(env("IRMM_WINDOWS_PKCS12")?);
            let password = env("IRMM_WINDOWS_PKCS12_PASSWORD")?;
            Ok(Box::new(WindowsAuthenticode { pkcs12, password }))
        }
        // ELF has no OS signing standard; integrity is covered by the ed25519
        // release signature the agent verifies on update.
        "linux" => Ok(Box::new(NoSigning)),
        other => bail!("no code-signing support for platform {other:?}"),
    }
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is required (populate it from Infisical)"))
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output().with_context(|| format!("run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            cmd.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// macOS `codesign`. `identity` is a keychain identity (e.g. `Developer ID
/// Application: Acme Inc (TEAMID)`) or `-` for an ad-hoc signature.
pub struct MacCodesign {
    pub identity: String,
}

impl Signer for MacCodesign {
    fn sign(&self, file: &Path) -> anyhow::Result<()> {
        let mut cmd = Command::new("codesign");
        cmd.arg("--force").arg("--sign").arg(&self.identity);
        if self.identity != "-" {
            // A secure timestamp and the hardened runtime are required for
            // notarization; ad-hoc signatures support neither.
            cmd.arg("--timestamp").arg("--options").arg("runtime");
        }
        run(cmd.arg(file))
    }

    fn verify(&self, file: &Path) -> anyhow::Result<()> {
        run(Command::new("codesign").arg("--verify").arg("--strict").arg(file))
    }
}

/// Windows Authenticode via `osslsigncode`, which signs PE binaries on any OS.
pub struct WindowsAuthenticode {
    pub pkcs12: PathBuf,
    pub password: String,
}

impl Signer for WindowsAuthenticode {
    fn sign(&self, file: &Path) -> anyhow::Result<()> {
        // osslsigncode cannot sign in place, so sign to a sibling and swap.
        let signed = file.with_extension("signed");
        run(Command::new("osslsigncode")
            .arg("sign")
            .arg("-pkcs12")
            .arg(&self.pkcs12)
            .arg("-pass")
            .arg(&self.password)
            .arg("-n")
            .arg("IRMM Agent")
            .arg("-ts")
            .arg("http://timestamp.digicert.com")
            .arg("-in")
            .arg(file)
            .arg("-out")
            .arg(&signed))?;
        std::fs::rename(&signed, file).context("replace with signed binary")?;
        Ok(())
    }

    fn verify(&self, file: &Path) -> anyhow::Result<()> {
        run(Command::new("osslsigncode").arg("verify").arg(file))
    }
}

/// Platforms with no OS signing step.
pub struct NoSigning;

impl Signer for NoSigning {
    fn sign(&self, _file: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    fn verify(&self, _file: &Path) -> anyhow::Result<()> {
        Ok(())
    }
}
