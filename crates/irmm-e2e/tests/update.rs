use std::sync::{Arc, Mutex};

use irmm_e2e::*;
use irmm_proto::ReleaseInfo;
use serde_json::json;

#[derive(Default, Clone)]
struct RecordingUpdater(Arc<Mutex<Vec<(String, Vec<u8>)>>>);

#[async_trait::async_trait]
impl irmm_agent::update::Updater for RecordingUpdater {
    async fn install(&self, release: &ReleaseInfo, binary: Vec<u8>) -> anyhow::Result<()> {
        self.0.lock().unwrap().push((release.version.clone(), binary));
        Ok(())
    }
}

async fn agent_with_updater(env: &TestEnv, admin: &Api, updater: RecordingUpdater) -> (irmm_agent::AgentHandle, uuid::Uuid, tempdir::Dir) {
    let (_, _, tok) = make_site(admin, "C", "S").await;
    let dir = tempdir::Dir::new();
    let mut cfg = env.agent_config(dir.to_path_buf(), tok["token"].as_str());
    cfg.updater = Arc::new(updater);
    let agent = irmm_agent::start(cfg).await.unwrap();
    let id = wait_agent_online(&agent).await;
    (agent, id, dir)
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(b))
}

#[tokio::test]
async fn signed_release_is_rolled_out_to_online_agents() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let updater = RecordingUpdater::default();
    let (_agent, dev, _dir) = agent_with_updater(&env, &admin, updater.clone()).await;

    let platform = irmm_proto::current_platform();
    let binary = b"#!new agent build".repeat(1000);
    let sig = irmm_proto::sign_release(&env.release_key, "9.9.9", &platform, &sha256_hex(&binary));
    let op = env.operator();
    // Operator API requires the operator token.
    env.anon()
        .post_bytes(&format!("/api/operator/releases?platform={platform}&version=9.9.9&signature={sig}"), binary.clone())
        .await
        .expect_status(401);
    op.post_bytes(&format!("/api/operator/releases?platform={platform}&version=9.9.9&signature={sig}"), binary.clone())
        .await
        .expect_ok();

    let installed = eventually("update installed", || async { updater.0.lock().unwrap().first().cloned() }).await;
    assert_eq!(installed.0, "9.9.9");
    assert_eq!(installed.1, binary);
    eventually("update reported", || async {
        let log = admin.get("/api/audit").await.expect_ok();
        log.as_array().unwrap().iter().find(|e| e["action"] == "agent.update" && e["target"] == dev.to_string()).cloned()
    })
    .await;

    // A release signed with the wrong key is refused by the server.
    let bad_sig = irmm_proto::sign_release(&iroh::SecretKey::generate(), "9.9.10", &platform, &sha256_hex(&binary));
    op.post_bytes(&format!("/api/operator/releases?platform={platform}&version=9.9.10&signature={bad_sig}"), binary)
        .await
        .expect_status(400);
}

#[tokio::test]
async fn agent_refuses_releases_not_signed_by_the_pinned_key() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let updater = RecordingUpdater::default();
    let (agent, dev, dir) = agent_with_updater(&env, &admin, updater.clone()).await;
    agent.stop().await;

    // Simulate a compromised server: a release row whose signature does not verify.
    let platform = irmm_proto::current_platform();
    let binary = b"evil".to_vec();
    let forged = irmm_proto::sign_release(&iroh::SecretKey::generate(), "6.6.6", &platform, &sha256_hex(&binary));
    let releases = env.server.state.data_dir.join("releases").join(&platform);
    std::fs::create_dir_all(&releases).unwrap();
    std::fs::write(releases.join("6.6.6"), &binary).unwrap();
    let mut db = <sqlx::PgConnection as sqlx::Connection>::connect(&env.db_url).await.unwrap();
    sqlx::query("INSERT INTO releases (platform, version, sha256, signature) VALUES ($1, '6.6.6', $2, $3)")
        .bind(&platform)
        .bind(sha256_hex(&binary))
        .bind(forged)
        .execute(&mut db)
        .await
        .unwrap();

    let mut cfg = env.agent_config(dir.to_path_buf(), None);
    cfg.updater = Arc::new(updater.clone());
    let _agent = irmm_agent::start(cfg).await.unwrap();
    let entry = eventually("update failure reported", || async {
        let log = admin.get("/api/audit").await.expect_ok();
        log.as_array().unwrap().iter().find(|e| e["action"] == "agent.update" && e["target"] == dev.to_string()).cloned()
    })
    .await;
    assert!(entry["detail"]["error"].as_str().unwrap().contains("signature"), "{entry}");
    assert!(updater.0.lock().unwrap().is_empty());
    let _ = json!({});
}
