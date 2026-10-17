use irmm_e2e::*;
use serde_json::json;

#[tokio::test]
async fn login_requires_password_and_mandatory_totp() {
    let env = TestEnv::start().await;
    let anon = env.anon();
    anon.post("/api/signup", json!({"tenant_name": "T", "email": "a@t.test", "password": PASSWORD}))
        .await
        .expect_ok();

    anon.get("/api/devices").await.expect_status(401);
    anon.post("/api/auth/login", json!({"email": "a@t.test", "password": "wrong"}))
        .await
        .expect_status(401);

    // First login forces TOTP enrollment; a pending token is not a session.
    let r = anon
        .post("/api/auth/login", json!({"email": "a@t.test", "password": PASSWORD}))
        .await
        .expect_ok();
    assert_eq!(r["step"], "totp_setup");
    assert!(r["otpauth_url"].as_str().unwrap().starts_with("otpauth://totp/"));
    let pending = r["pending_token"].as_str().unwrap();
    Api::new(env.url(), Some(pending.to_owned())).get("/api/devices").await.expect_status(401);
    anon.post("/api/auth/totp", json!({"pending_token": pending, "code": "000000"}))
        .await
        .expect_status(401);
    let secret = r["totp_secret"].as_str().unwrap();
    let s = anon
        .post("/api/auth/totp", json!({"pending_token": pending, "code": totp_now(secret)}))
        .await
        .expect_ok();
    let user = Api::new(env.url(), Some(s["token"].as_str().unwrap().to_owned()));
    let me = user.get("/api/me").await.expect_ok();
    assert_eq!(me["email"], "a@t.test");
    assert_eq!(me["role"], "owner");

    // Later logins ask for a code, not a new secret.
    let r = anon
        .post("/api/auth/login", json!({"email": "a@t.test", "password": PASSWORD}))
        .await
        .expect_ok();
    assert_eq!(r["step"], "totp");
    assert!(r.get("totp_secret").is_none());
    anon.login_with("a@t.test", PASSWORD, Some(secret)).await.unwrap();

    user.post("/api/auth/logout", json!({})).await.expect_ok();
    user.get("/api/me").await.expect_status(401);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let env = TestEnv::start().await;
    let a = env.signup("A MSP", "owner@a.test").await;
    let b = env.signup("B MSP", "owner@b.test").await;
    let (a_client, a_site, _) = make_site(&a, "A-Client", "A-Site").await;
    let (_agent, a_dev, _dir) = enrolled_agent(&env, &a).await;

    assert!(b.get("/api/devices").await.expect_ok().as_array().unwrap().is_empty());
    assert!(b.get("/api/clients").await.expect_ok().as_array().unwrap().is_empty());
    b.get(&format!("/api/devices/{a_dev}")).await.expect_status(404);
    b.post(&format!("/api/clients/{a_client}/sites"), json!({"name": "x"})).await.expect_status(404);
    b.post(&format!("/api/sites/{a_site}/enroll-tokens"), json!({"max_uses": 1, "ttl_hours": 1}))
        .await
        .expect_status(404);
    b.post(&format!("/api/devices/{a_dev}/sessions"), json!({"kind": "shell"})).await.expect_status(404);
    assert!(b.get("/api/audit").await.expect_ok().as_array().unwrap().iter().all(|e| e["actor_email"] != "owner@a.test"));

    // Row-level security holds even for raw SQL issued as the application role.
    let mut db = <sqlx::PgConnection as sqlx::Connection>::connect(&env.db_url).await.unwrap();
    let tenants: Vec<(uuid::Uuid,)> = sqlx::query_as("SELECT id FROM tenants ORDER BY created_at").fetch_all(&mut db).await.unwrap();
    let b_tenant = tenants[1].0;
    sqlx::query("BEGIN").execute(&mut db).await.unwrap();
    sqlx::query("SET LOCAL ROLE irmm_app").execute(&mut db).await.unwrap();
    sqlx::query(&format!("SET LOCAL app.tenant_id = '{b_tenant}'")).execute(&mut db).await.unwrap();
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM devices").fetch_one(&mut db).await.unwrap();
    assert_eq!(n.0, 0, "tenant B sees no devices through RLS");
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM clients").fetch_one(&mut db).await.unwrap();
    assert_eq!(n.0, 0);
    sqlx::query("ROLLBACK").execute(&mut db).await.unwrap();
}

#[tokio::test]
async fn roles_and_client_scoping_are_enforced() {
    let env = TestEnv::start().await;
    let owner = env.signup("MSP", "owner@m.test").await;
    let (c1, _, t1) = make_site(&owner, "C1", "S1").await;
    let (_c2, _, t2) = make_site(&owner, "C2", "S2").await;
    let d1 = tempdir::Dir::new();
    let a1 = env.start_agent(d1.to_path_buf(), t1["token"].as_str()).await;
    let dev1 = wait_agent_online(&a1).await;
    let d2 = tempdir::Dir::new();
    let a2 = env.start_agent(d2.to_path_buf(), t2["token"].as_str()).await;
    let dev2 = wait_agent_online(&a2).await;

    owner
        .post("/api/users", json!({"email": "tech@m.test", "password": PASSWORD, "role": "technician", "client_ids": [c1]}))
        .await
        .expect_ok();
    owner
        .post("/api/users", json!({"email": "view@m.test", "password": PASSWORD, "role": "viewer", "client_ids": []}))
        .await
        .expect_ok();
    let tech = env.anon().login("tech@m.test", PASSWORD).await.unwrap();
    let viewer = env.anon().login("view@m.test", PASSWORD).await.unwrap();

    // Technician: only devices of the allowed client; can open sessions there; cannot administer.
    let devs = tech.get("/api/devices").await.expect_ok();
    let ids: Vec<_> = devs.as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, vec![dev1.to_string()]);
    tech.get(&format!("/api/devices/{dev2}")).await.expect_status(404);
    tech.post(&format!("/api/devices/{dev1}/sessions"), json!({"kind": "shell"})).await.expect_ok();
    tech.post(&format!("/api/devices/{dev2}/sessions"), json!({"kind": "shell"})).await.expect_status(404);
    tech.post("/api/clients", json!({"name": "nope"})).await.expect_status(403);
    tech.post("/api/users", json!({"email": "x@m.test", "password": PASSWORD, "role": "admin", "client_ids": []}))
        .await
        .expect_status(403);

    // Viewer: sees everything (no client restriction), cannot act.
    assert_eq!(viewer.get("/api/devices").await.expect_ok().as_array().unwrap().len(), 2);
    viewer.post(&format!("/api/devices/{dev1}/sessions"), json!({"kind": "shell"})).await.expect_status(403);
    viewer.post("/api/jobs", json!({"device_ids": [dev1], "shell": "sh", "body": "echo hi"})).await.expect_status(403);

    // Admins cannot mint owners.
    owner
        .post("/api/users", json!({"email": "adm@m.test", "password": PASSWORD, "role": "admin", "client_ids": []}))
        .await
        .expect_ok();
    let adm = env.anon().login("adm@m.test", PASSWORD).await.unwrap();
    adm.post("/api/users", json!({"email": "o2@m.test", "password": PASSWORD, "role": "owner", "client_ids": []}))
        .await
        .expect_status(403);
}

#[tokio::test]
async fn audit_log_records_actions_and_is_append_only() {
    let env = TestEnv::start().await;
    let owner = env.signup("MSP", "owner@m.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &owner).await;
    owner.post(&format!("/api/devices/{dev}/sessions"), json!({"kind": "shell"})).await.expect_ok();

    let log = owner.get("/api/audit").await.expect_ok();
    let actions: Vec<_> = log.as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap().to_owned()).collect();
    for want in ["auth.login", "client.create", "site.create", "enroll_token.create", "device.enroll", "session.create"] {
        assert!(actions.iter().any(|a| a == want), "audit has {want}: {actions:?}");
    }
    owner.delete("/api/audit").await.expect_status(405);

    let mut db = <sqlx::PgConnection as sqlx::Connection>::connect(&env.db_url).await.unwrap();
    // Not even the database owner can rewrite history.
    assert!(sqlx::query("UPDATE audit_log SET action = 'x'").execute(&mut db).await.is_err());
    assert!(sqlx::query("DELETE FROM audit_log").execute(&mut db).await.is_err());
}
