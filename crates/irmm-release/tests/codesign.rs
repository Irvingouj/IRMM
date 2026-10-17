//! Trait-level tests for OS code signing. The macOS path is exercised for real
//! with an ad-hoc signature (needs no certificate), so CI proves the signer and
//! its tamper detection end-to-end against the actual `codesign` tool.

use std::path::{Path, PathBuf};

use irmm_release::codesign::{self, Signer};

/// Scratch directory removed on drop (no external crate needed).
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        // Unique per call: parallel tests share this process's pid, so a clock
        // alone can collide and one Dir's Drop would wipe another's files.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("irmm-codesign-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A copy of this test binary — a genuine Mach-O / PE / ELF to sign.
fn sample(dir: &Path) -> PathBuf {
    let dst = dir.join("sample.bin");
    std::fs::copy(std::env::current_exe().unwrap(), &dst).unwrap();
    dst
}

#[cfg(target_os = "macos")]
#[test]
fn macos_adhoc_signature_round_trips_and_detects_tampering() {
    let dir = Dir::new();
    let bin = sample(dir.path());
    let signer = codesign::MacCodesign { identity: "-".into() };

    signer.sign(&bin).unwrap();
    signer.verify(&bin).expect("freshly ad-hoc-signed binary verifies");

    // Flip a byte inside a signed code page; verification must now fail.
    let mut bytes = std::fs::read(&bin).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&bin, &bytes).unwrap();
    assert!(signer.verify(&bin).is_err(), "tampered binary must fail verification");
}

#[test]
fn linux_has_no_os_signing_step() {
    let dir = Dir::new();
    let bin = sample(dir.path());
    let signer = codesign::for_platform("linux-x86_64").unwrap();
    // No-op: integrity is covered by the ed25519 release signature instead.
    signer.sign(&bin).unwrap();
    signer.verify(&bin).unwrap();
}

#[test]
fn missing_windows_credentials_name_the_secret() {
    // The release runner has no signing secrets in its environment; the signer
    // must say which Infisical variable to populate rather than fail obscurely.
    if std::env::var_os("IRMM_WINDOWS_PKCS12").is_some() {
        return; // credentials present (unexpected in CI); nothing to assert.
    }
    let err = match codesign::for_platform("windows-x86_64") {
        Ok(_) => panic!("expected missing-credential error"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("IRMM_WINDOWS_PKCS12"), "{err}");
}
