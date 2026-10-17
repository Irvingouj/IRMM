//! One-command install: the script served by the server fetches the latest signed
//! release for this platform and hands it the server URL and enrollment token.

use irmm_e2e::*;

/// Publishes `binary` as the latest release for this platform.
async fn publish(env: &TestEnv, version: &str, binary: Vec<u8>) {
    use sha2::Digest as _;
    let platform = irmm_proto::current_platform();
    let sha = hex::encode(sha2::Sha256::digest(&binary));
    let sig = irmm_proto::sign_release(&env.release_key, version, &platform, &sha);
    env.operator()
        .post_bytes(&format!("/api/operator/releases?platform={platform}&version={version}&signature={sig}"), binary)
        .await
        .expect_ok();
}

fn stand_in_agent() -> Vec<u8> {
    std::fs::read(env!("CARGO_BIN_EXE_record-args")).unwrap()
}

async fn setup() -> (TestEnv, String, serde_json::Value, tempdir::Dir) {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_, _, tok) = make_site(&admin, "C", "S").await;
    publish(&env, "1.0.0", b"not this one".to_vec()).await;
    publish(&env, "1.0.1", stand_in_agent()).await;
    let token = tok["token"].as_str().unwrap().to_owned();
    (env, token, tok["install"].clone(), tempdir::Dir::new())
}

#[cfg(unix)]
#[tokio::test]
async fn install_sh_runs_latest_release_with_token() {
    use std::io::Write as _;
    let (env, token, install, dir) = setup().await;
    for os in ["linux", "macos"] {
        let cmd = install[os].as_str().unwrap();
        assert!(cmd.contains(&format!("{}/install.sh", env.url())) && cmd.contains(&token), "{cmd}");
    }
    let (status, script) = env.anon().get_bytes("/install.sh").await;
    assert_eq!(status, 200);

    let record = dir.path().join("args");
    let mut child = std::process::Command::new("sh")
        .args(["-s", "--", &token])
        .env("IRMM_TEST_RECORD", &record)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&script).unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output()).await.unwrap().unwrap();
    assert!(out.status.success(), "install.sh failed: {}", String::from_utf8_lossy(&out.stderr));
    let args = std::fs::read_to_string(&record).unwrap();
    assert_eq!(args, format!("install --server {} --token {token}", env.url()));
}

#[cfg(windows)]
#[tokio::test]
async fn install_ps1_runs_latest_release_with_token() {
    let (env, token, install, dir) = setup().await;
    let cmd = install["windows"].as_str().unwrap();
    assert!(cmd.contains(&format!("{}/install.ps1", env.url())) && cmd.contains(&token), "{cmd}");

    // Run exactly the command the console shows.
    let record = dir.path().join("args");
    let out = tokio::process::Command::new("cmd.exe")
        .args(["/C", cmd])
        .env("IRMM_TEST_RECORD", &record)
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "install.ps1 failed: {}", String::from_utf8_lossy(&out.stderr));
    let args = std::fs::read_to_string(&record).unwrap();
    assert_eq!(args, format!("install --server {} --token {token}", env.url()));
}
