use irmm_agent::AgentState;
use irmm_e2e::*;
use serde_json::json;

#[tokio::test]
async fn agent_enrolls_with_site_token_and_shows_up_online_with_inventory() {
    let env = TestEnv::start().await;
    let admin = env.signup("Acme MSP", "owner@acme.test").await;
    let (client_id, site_id, tok) = make_site(&admin, "Contoso", "HQ").await;

    let token = tok["token"].as_str().unwrap();
    for os in ["windows", "macos", "linux"] {
        let cmd = tok["install"][os].as_str().unwrap_or_else(|| panic!("install command for {os}"));
        assert!(cmd.contains(token), "{os} install command embeds the token: {cmd}");
    }

    let dir = tempdir::Dir::new();
    let agent = env.start_agent(dir.to_path_buf(), Some(token)).await;
    let device_id = wait_agent_online(&agent).await;

    let devices = admin.get("/api/devices").await.expect_ok();
    let devices = devices.as_array().unwrap();
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(d["id"], device_id.to_string());
    assert_eq!(d["site_id"], site_id.as_str());
    assert_eq!(d["client_id"], client_id.as_str());
    assert_eq!(d["online"], true);
    assert!(!d["hostname"].as_str().unwrap().is_empty());
    assert_eq!(d["os"], std::env::consts::OS);
    assert!(d["last_seen"].is_string());

    let detail = admin.get(&format!("/api/devices/{device_id}")).await.expect_ok();
    let inv = &detail["inventory"];
    assert!(inv["memory_bytes"].as_u64().unwrap() > 0);
    assert!(inv["cpu_cores"].as_u64().unwrap() > 0);
    assert!(!inv["ips"].as_array().unwrap().is_empty());
    assert!(!inv["disks"].as_array().unwrap().is_empty());
    assert!(inv["logged_in_users"].is_array());

    // Going offline is visible.
    agent.stop().await;
    eventually("device offline", || async {
        let d = admin.get(&format!("/api/devices/{device_id}")).await.expect_ok();
        (d["online"] == false).then_some(())
    })
    .await;

    // Restart from the same state dir, without a token: same identity, no duplicate.
    let agent = env.start_agent(dir.to_path_buf(), None).await;
    assert_eq!(wait_agent_online(&agent).await, device_id);
    assert_eq!(admin.get("/api/devices").await.expect_ok().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn enrollment_token_limits_are_enforced() {
    let env = TestEnv::start().await;
    let admin = env.signup("Acme MSP", "owner@acme.test").await;
    let c = admin.post("/api/clients", json!({"name": "C"})).await.expect_ok();
    let s = admin
        .post(&format!("/api/clients/{}/sites", c["id"].as_str().unwrap()), json!({"name": "S"}))
        .await
        .expect_ok();
    let tok = admin
        .post(&format!("/api/sites/{}/enroll-tokens", s["id"].as_str().unwrap()), json!({"max_uses": 1, "ttl_hours": 1}))
        .await
        .expect_ok();
    let token = tok["token"].as_str().unwrap();

    let d1 = tempdir::Dir::new();
    let a1 = env.start_agent(d1.to_path_buf(), Some(token)).await;
    wait_agent_online(&a1).await;

    let d2 = tempdir::Dir::new();
    let a2 = env.start_agent(d2.to_path_buf(), Some(token)).await;
    eventually("second enrollment rejected", || async {
        matches!(a2.state(), AgentState::Rejected { .. }).then_some(())
    })
    .await;

    let d3 = tempdir::Dir::new();
    let a3 = env.start_agent(d3.to_path_buf(), Some("bogus-token")).await;
    eventually("bogus token rejected", || async {
        matches!(a3.state(), AgentState::Rejected { .. }).then_some(())
    })
    .await;

    assert_eq!(admin.get("/api/devices").await.expect_ok().as_array().unwrap().len(), 1);
}
