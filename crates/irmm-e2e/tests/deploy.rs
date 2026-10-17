//! The shipped server and release tool, run as processes the way an operator would.

use std::process::Stdio;

use irmm_e2e::*;
use serde_json::json;
use tokio::process::{Child, Command};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port()
}

/// Binds Iroh on all interfaces like a real deployment; agents learn `localhost:<udp>`
/// from the public URL. Each start gets a new UDP port, as if the server moved.
fn start_server(db_url: &str, port: u16, data: &std::path::Path, release_pub: &str) -> Child {
    Command::new(workspace_binary("irmm-server", "irmm-server"))
        .env("IRMM_DATABASE_URL", db_url)
        .env("IRMM_HTTP_ADDR", format!("127.0.0.1:{port}"))
        .env("IRMM_IROH_ADDR", format!("0.0.0.0:{}", free_udp_port()))
        .env("IRMM_PUBLIC_URL", format!("http://localhost:{port}"))
        .env("IRMM_DATA_DIR", data)
        .env("IRMM_RELEASE_PUBLIC_KEY", release_pub)
        .env("IRMM_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env("IRMM_ICE_SERVERS", "")
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn irmm-server")
}

async fn bootstrap(url: &str) -> serde_json::Value {
    eventually("server up", || async {
        let r = reqwest::get(format!("{url}/api/agent/bootstrap")).await.ok()?;
        r.status().is_success().then_some(())?;
        r.json().await.ok()
    })
    .await
}

#[tokio::test]
async fn operator_deploys_server_publishes_signed_agent_and_agent_enrolls() {
    let dir = tempdir::Dir::new();
    let release_tool = workspace_binary("irmm-release", "irmm-release");

    // Offline release key.
    let key = dir.path().join("release.key");
    let out = std::process::Command::new(&release_tool).arg("keygen").arg("--out").arg(&key).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let release_pub = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    // Refuses to overwrite an existing key.
    assert!(!std::process::Command::new(&release_tool).arg("keygen").arg("--out").arg(&key).output().unwrap().status.success());

    let db = fresh_database().await;
    let port = free_port();
    let url = format!("http://localhost:{port}");
    let data = dir.path().join("data");
    let mut server = start_server(&db, port, &data, &release_pub);
    let boot = bootstrap(&url).await;
    assert_eq!(boot["release_key"], release_pub.as_str());
    assert!(boot["direct_addrs"][0].as_str().unwrap().starts_with("localhost:"), "{boot}");

    // Publish the agent build (same version as the agent, so no update is offered).
    let platform = irmm_proto::current_platform();
    let version = env!("CARGO_PKG_VERSION");
    let out = std::process::Command::new(&release_tool)
        .args(["publish", "--server", &url, "--version", version, "--platform", &platform])
        .arg("--key")
        .arg(&key)
        .arg("--file")
        .arg(agent_binary())
        .env("IRMM_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let served = reqwest::get(format!("{url}/api/agent/releases/{platform}/latest")).await.unwrap().bytes().await.unwrap();
    assert_eq!(served.as_ref(), std::fs::read(agent_binary()).unwrap().as_slice());

    // A tenant enrolls the published agent.
    let anon = Api::new(url.clone(), None);
    anon.post("/api/signup", json!({"tenant_name": "MSP", "email": "o@m.test", "password": PASSWORD})).await.expect_ok();
    let admin = anon.login("o@m.test", PASSWORD).await.unwrap();
    let (_, _, tok) = make_site(&admin, "C", "S").await;
    let state = dir.path().join("agent");
    let _agent = Command::new(agent_binary())
        .args(["run", "--server", &url, "--token", tok["token"].as_str().unwrap()])
        .arg("--state-dir")
        .arg(&state)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let online = || async {
        let list = admin.get("/api/devices").await.expect_ok();
        list.as_array().unwrap().iter().find(|d| d["online"] == true).map(|d| d["id"].clone())
    };
    let id = eventually("agent online", online).await;

    // Server restart keeps its identity, so the agent reconnects by itself.
    server.kill().await.unwrap();
    let mut _server = start_server(&db, port, &data, &release_pub);
    let boot2 = bootstrap(&url).await;
    assert_eq!(boot2["server_id"], boot["server_id"]);
    // Noticing the dead connection takes up to the 15 s QUIC idle timeout.
    let again = eventually_for(std::time::Duration::from_secs(60), "agent back online", || async {
        let list = admin.get("/api/devices").await.expect_ok();
        list.as_array().unwrap().iter().find(|d| d["online"] == true).map(|d| d["id"].clone())
    })
    .await;
    assert_eq!(again, id);
}
