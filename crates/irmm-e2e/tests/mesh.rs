use std::sync::Arc;

use irmm_e2e::mesh::FakeNetwork;
use irmm_e2e::*;
use serde_json::{Value, json};

async fn mesh_agent(env: &TestEnv, token: &Value, net: &Arc<FakeNetwork>, dir: &tempdir::Dir) -> irmm_agent::AgentHandle {
    let mut cfg = env.agent_config(dir.to_path_buf(), token["token"].as_str());
    cfg.network = net.clone();
    irmm_agent::start(cfg).await.expect("start agent")
}

async fn device(admin: &Api, id: uuid::Uuid) -> Value {
    admin.get(&format!("/api/devices/{id}")).await.expect_ok()
}

#[tokio::test]
async fn enabling_mesh_gives_devices_addresses_and_configures_their_interface() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "owner@m.test").await;
    let (client, _, tok) = make_site(&admin, "Clinic", "Office").await;
    let (net_a, dir_a) = (Arc::new(FakeNetwork::default()), tempdir::Dir::new());
    let agent_a = mesh_agent(&env, &tok, &net_a, &dir_a).await;
    let dev_a = wait_agent_online(&agent_a).await;

    // Off by default: no addresses, nothing configured.
    let clients = admin.get("/api/clients").await.expect_ok();
    assert_eq!(clients[0]["mesh_enabled"], false);
    assert_eq!(device(&admin, dev_a).await["mesh_ipv4"], Value::Null);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(net_a.current(), None);

    let c = admin.put(&format!("/api/clients/{client}/mesh"), json!({"enabled": true})).await.expect_ok();
    assert_eq!(c["mesh_enabled"], true);
    let range = c["mesh_ipv4_range"].as_str().unwrap().to_owned();
    let prefix = c["mesh_ipv6_prefix"].as_str().unwrap().to_owned();
    assert_eq!(range, "100.88.0.0/16");
    assert!(prefix.starts_with("fd") && prefix.ends_with("::/48"), "ULA /48: {prefix}");

    // A device enrolled after enabling gets addresses too.
    let (net_b, dir_b) = (Arc::new(FakeNetwork::default()), tempdir::Dir::new());
    let agent_b = mesh_agent(&env, &tok, &net_b, &dir_b).await;
    let dev_b = wait_agent_online(&agent_b).await;

    let a = device(&admin, dev_a).await;
    let b = device(&admin, dev_b).await;
    let (a4, a6) = (a["mesh_ipv4"].as_str().unwrap(), a["mesh_ipv6"].as_str().unwrap());
    let (b4, b6) = (b["mesh_ipv4"].as_str().unwrap(), b["mesh_ipv6"].as_str().unwrap());
    assert!(a4.starts_with("100.88.") && b4.starts_with("100.88."), "{a4} {b4}");
    assert_ne!(a4, b4);
    assert_ne!(a6, b6);
    let p48 = prefix.trim_end_matches("::/48");
    assert!(a6.starts_with(p48) && b6.starts_with(p48), "{a6} {b6} in {prefix}");
    let listed = admin.get("/api/devices").await.expect_ok();
    assert!(listed.as_array().unwrap().iter().any(|d| d["mesh_ipv4"] == a4));

    // Each agent configured its own addresses plus routes for the whole client network.
    for (net, v4, v6) in [(&net_a, a4, a6), (&net_b, b4, b6)] {
        let iface = eventually("interface configured", || async { net.current() }).await;
        assert_eq!(iface.ipv4.to_string(), format!("{v4}/16"));
        assert_eq!(iface.ipv6.to_string(), format!("{v6}/48"));
        let routes: Vec<String> = iface.routes.iter().map(|r| r.to_string()).collect();
        assert_eq!(routes, vec![range.clone(), prefix.clone()]);
    }

    // Disabling removes the interface again.
    admin.put(&format!("/api/clients/{client}/mesh"), json!({"enabled": false})).await.expect_ok();
    eventually("interface removed", || async { net_a.current().is_none().then_some(()) }).await;
    assert_eq!(device(&admin, dev_a).await["mesh_ipv4"], Value::Null);
}

#[tokio::test]
async fn mesh_addresses_survive_agent_restart() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "owner@m.test").await;
    let (client, _, tok) = make_site(&admin, "Clinic", "Office").await;
    admin.put(&format!("/api/clients/{client}/mesh"), json!({"enabled": true})).await.expect_ok();
    // Another device first, so the restarted one is not trivially the lowest address.
    let (net_x, dir_x) = (Arc::new(FakeNetwork::default()), tempdir::Dir::new());
    let _x = mesh_agent(&env, &tok, &net_x, &dir_x).await;
    wait_agent_online(&_x).await;

    let dir = tempdir::Dir::new();
    let net1 = Arc::new(FakeNetwork::default());
    let agent = mesh_agent(&env, &tok, &net1, &dir).await;
    let dev = wait_agent_online(&agent).await;
    let before = eventually("interface configured", || async { net1.current() }).await;
    let api_before = device(&admin, dev).await;
    agent.stop().await;

    let net2 = Arc::new(FakeNetwork::default());
    let agent = mesh_agent(&env, &tok, &net2, &dir).await;
    assert_eq!(wait_agent_online(&agent).await, dev);
    let after = eventually("interface configured again", || async { net2.current() }).await;
    assert_eq!(after, before);
    let api_after = device(&admin, dev).await;
    assert_eq!(api_after["mesh_ipv4"], api_before["mesh_ipv4"]);
    assert_eq!(api_after["mesh_ipv6"], api_before["mesh_ipv6"]);
    assert_eq!(api_after["mesh_ipv4"].as_str().unwrap().to_owned() + "/16", after.ipv4.to_string());
}

#[tokio::test]
async fn enabling_mesh_leaves_other_clients_and_tenants_alone() {
    let env = TestEnv::start().await;
    let a = env.signup("A MSP", "owner@a.test").await;
    let b = env.signup("B MSP", "owner@b.test").await;
    let (on, _, on_tok) = make_site(&a, "On", "S").await;
    let (_off, _, off_tok) = make_site(&a, "Off", "S").await;
    let (b_client, _, b_tok) = make_site(&b, "B-Client", "S").await;
    let mut agents = Vec::new();
    for tok in [&on_tok, &off_tok, &b_tok] {
        let (net, dir) = (Arc::new(FakeNetwork::default()), tempdir::Dir::new());
        let agent = mesh_agent(&env, tok, &net, &dir).await;
        let id = wait_agent_online(&agent).await;
        agents.push((agent, id, net, dir));
    }
    let (on_dev, off_dev, b_dev) = (agents[0].1, agents[1].1, agents[2].1);

    // Other tenants cannot see or toggle the client; viewers cannot toggle.
    b.put(&format!("/api/clients/{on}/mesh"), json!({"enabled": true})).await.expect_status(404);
    a.post("/api/users", json!({"email": "view@a.test", "password": PASSWORD, "role": "viewer", "client_ids": []}))
        .await
        .expect_ok();
    let viewer = env.anon().login("view@a.test", PASSWORD).await.unwrap();
    viewer.put(&format!("/api/clients/{on}/mesh"), json!({"enabled": true})).await.expect_status(403);

    a.put(&format!("/api/clients/{on}/mesh"), json!({"enabled": true})).await.expect_ok();
    eventually("mesh client configured", || async { agents[0].2.current() }).await;
    assert!(device(&a, on_dev).await["mesh_ipv4"].is_string());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(device(&a, off_dev).await["mesh_ipv4"], Value::Null);
    assert_eq!(device(&b, b_dev).await["mesh_ipv4"], Value::Null);
    assert_eq!(agents[1].2.current(), None);
    assert_eq!(agents[2].2.current(), None);
    let b_clients = b.get("/api/clients").await.expect_ok();
    assert_eq!(b_clients[0]["id"], b_client.as_str());
    assert_eq!(b_clients[0]["mesh_enabled"], false);

    // Row-level security covers the mesh addresses.
    let mut db = <sqlx::PgConnection as sqlx::Connection>::connect(&env.db_url).await.unwrap();
    let tenants: Vec<(uuid::Uuid,)> = sqlx::query_as("SELECT id FROM tenants ORDER BY created_at").fetch_all(&mut db).await.unwrap();
    sqlx::query("BEGIN").execute(&mut db).await.unwrap();
    sqlx::query("SET LOCAL ROLE irmm_app").execute(&mut db).await.unwrap();
    sqlx::query(&format!("SET LOCAL app.tenant_id = '{}'", tenants[1].0)).execute(&mut db).await.unwrap();
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_addrs").fetch_one(&mut db).await.unwrap();
    assert_eq!(n.0, 0, "tenant B sees no mesh addresses through RLS");
    sqlx::query(&format!("SET LOCAL app.tenant_id = '{}'", tenants[0].0)).execute(&mut db).await.unwrap();
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM mesh_addrs").fetch_one(&mut db).await.unwrap();
    assert_eq!(n.0, 1, "tenant A sees its one mesh device");
    sqlx::query("ROLLBACK").execute(&mut db).await.unwrap();
}
