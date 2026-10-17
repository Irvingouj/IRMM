//! The shipped `irmm-agent` executable, driven as a separate process.

use std::process::Stdio;

use irmm_e2e::*;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

fn run_agent(args: &[&str]) -> Child {
    Command::new(agent_binary())
        .args(args)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn irmm-agent")
}

async fn online_device(admin: &Api) -> Value {
    eventually("device online", || async {
        let list = admin.get("/api/devices").await.expect_ok();
        list.as_array().unwrap().iter().find(|d| d["online"] == true).cloned()
    })
    .await
}

#[tokio::test]
async fn agent_process_enrolls_runs_scripts_and_survives_restart() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_, _, tok) = make_site(&admin, "C", "S").await;
    let state = tempdir::Dir::new();
    let state_dir = state.path().to_str().unwrap().to_owned();

    let mut agent = run_agent(&["run", "--server", &env.url(), "--token", tok["token"].as_str().unwrap(), "--state-dir", &state_dir]);
    let dev = online_device(&admin).await;
    let id = dev["id"].as_str().unwrap().to_owned();
    assert_eq!(dev["agent_version"], env!("CARGO_PKG_VERSION"));

    let body = if cfg!(windows) { "Write-Output ok-from-binary" } else { "echo ok-from-binary" };
    let job = admin.post("/api/jobs", json!({"device_ids": [id], "shell": native_shell(), "body": body})).await.expect_ok();
    let run = eventually("job finished", || async {
        let j = admin.get(&format!("/api/jobs/{}", job["id"].as_str().unwrap())).await.expect_ok();
        let r = j["runs"][0].clone();
        (r["status"] == "succeeded" || r["status"] == "failed").then_some(r)
    })
    .await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert!(run["stdout"].as_str().unwrap().contains("ok-from-binary"));

    // Restart without server URL or token: the enrolled identity is reused.
    agent.kill().await.unwrap();
    eventually("device offline", || async {
        let d = admin.get(&format!("/api/devices/{id}")).await.expect_ok();
        (d["online"] == false).then_some(())
    })
    .await;
    let _agent = run_agent(&["run", "--state-dir", &state_dir]);
    let again = online_device(&admin).await;
    assert_eq!(again["id"], id.as_str());
    assert_eq!(admin.get("/api/devices").await.expect_ok().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn bad_enrollment_token_fails_with_message() {
    let env = TestEnv::start().await;
    let state = tempdir::Dir::new();
    let out = Command::new(agent_binary())
        .args(["run", "--server", &env.url(), "--token", "nope", "--state-dir", state.path().to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("enrollment token"), "{err}");
}
