use irmm_e2e::*;
use serde_json::{Value, json};

async fn wait_job(admin: &Api, job: &str) -> Value {
    eventually("job finished", || async {
        let j = admin.get(&format!("/api/jobs/{job}")).await.expect_ok();
        j["runs"].as_array().unwrap().iter().all(|r| r["status"] == "succeeded" || r["status"] == "failed").then_some(j)
    })
    .await
}

#[tokio::test]
async fn script_library_and_batch_execution_with_output() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_a1, d1, _x1) = enrolled_agent(&env, &admin).await;
    let (_a2, d2, _x2) = enrolled_agent(&env, &admin).await;

    // Library CRUD.
    let body = if cfg!(windows) { "Write-Output \"hello $env:COMPUTERNAME\"" } else { "echo \"hello $(hostname)\"" };
    let s = admin
        .post("/api/scripts", json!({"name": "Hello", "shell": native_shell(), "body": body}))
        .await
        .expect_ok();
    let sid = s["id"].as_str().unwrap().to_owned();
    admin.put(&format!("/api/scripts/{sid}"), json!({"name": "Hello world", "shell": native_shell(), "body": body})).await.expect_ok();
    let list = admin.get("/api/scripts").await.expect_ok();
    assert_eq!(list[0]["name"], "Hello world");

    // Batch run on two devices.
    let job = admin.post("/api/jobs", json!({"device_ids": [d1, d2], "script_id": sid})).await.expect_ok();
    let j = wait_job(&admin, job["id"].as_str().unwrap()).await;
    let runs = j["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    for r in runs {
        assert_eq!(r["status"], "succeeded", "{r}");
        assert_eq!(r["exit_code"], 0);
        assert!(r["stdout"].as_str().unwrap().contains("hello"), "{r}");
    }

    // Ad-hoc script: non-zero exit and stderr are captured.
    let body = if cfg!(windows) { "[Console]::Error.WriteLine('boom'); exit 3" } else { "echo boom >&2; exit 3" };
    let job = admin.post("/api/jobs", json!({"device_ids": [d1], "shell": native_shell(), "body": body})).await.expect_ok();
    let j = wait_job(&admin, job["id"].as_str().unwrap()).await;
    let r = &j["runs"][0];
    assert_eq!(r["status"], "failed");
    assert_eq!(r["exit_code"], 3);
    assert!(r["stderr"].as_str().unwrap().contains("boom"));

    // Timeouts are enforced.
    let body = if cfg!(windows) { "Start-Sleep -Seconds 30" } else { "sleep 30" };
    let job = admin
        .post("/api/jobs", json!({"device_ids": [d1], "shell": native_shell(), "body": body, "timeout_secs": 1}))
        .await
        .expect_ok();
    let j = wait_job(&admin, job["id"].as_str().unwrap()).await;
    assert_eq!(j["runs"][0]["status"], "failed");
    assert!(j["runs"][0]["error"].as_str().unwrap().contains("timed out"));

    admin.delete(&format!("/api/scripts/{sid}")).await.expect_ok();
    assert!(admin.get("/api/scripts").await.expect_ok().as_array().unwrap().is_empty());
    let jobs = admin.get("/api/jobs").await.expect_ok();
    assert_eq!(jobs.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn jobs_on_offline_devices_fail_fast() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (agent, dev, _dir) = enrolled_agent(&env, &admin).await;
    agent.stop().await;
    eventually("offline", || async {
        (admin.get(&format!("/api/devices/{dev}")).await.expect_ok()["online"] == false).then_some(())
    })
    .await;
    let job = admin.post("/api/jobs", json!({"device_ids": [dev], "shell": native_shell(), "body": "echo hi"})).await.expect_ok();
    let j = wait_job(&admin, job["id"].as_str().unwrap()).await;
    assert_eq!(j["runs"][0]["status"], "failed");
    assert!(j["runs"][0]["error"].as_str().unwrap().contains("offline"));
}
