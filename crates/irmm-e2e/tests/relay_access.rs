//! The self-hosted iroh-relay asks the server who may relay (iroh-relay `access.http`):
//! the server itself, enrolled devices, and agents that are enrolling with a valid token.

use irmm_e2e::*;

async fn check(env: &TestEnv, node: &str, bearer: Option<&str>) -> (u16, String) {
    let mut req = reqwest::Client::new().post(format!("{}/api/relay/access", env.url())).header("X-Iroh-Endpoint-Id", node);
    if let Some(b) = bearer {
        req = req.bearer_auth(b);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test]
async fn relay_admits_only_the_server_enrolling_and_enrolled_agents() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let stranger = iroh::SecretKey::generate().public().to_string();

    // The relay authenticates to the server.
    assert_eq!(check(&env, &stranger, None).await.0, 401);
    assert_eq!(check(&env, &stranger, Some("wrong")).await.0, 401);
    assert_eq!(check(&env, &stranger, Some(RELAY_ACCESS_TOKEN)).await.1, "false");
    assert_eq!(check(&env, &env.server.server_id.to_string(), Some(RELAY_ACCESS_TOKEN)).await.1, "true");

    // Bootstrapping with a bad token grants nothing.
    let r = reqwest::get(format!("{}/api/agent/bootstrap?node={stranger}&token=nope", env.url())).await.unwrap();
    assert!(r.status().is_success());
    assert_eq!(check(&env, &stranger, Some(RELAY_ACCESS_TOKEN)).await.1, "false");

    // An agent enrolling with a valid token may relay before it is enrolled, and after.
    let (_, _, tok) = make_site(&admin, "C", "S").await;
    let dir = tempdir::Dir::new();
    let agent = env.start_agent(dir.to_path_buf(), tok["token"].as_str()).await;
    let node = agent.endpoint().id().to_string();
    assert_eq!(check(&env, &node, Some(RELAY_ACCESS_TOKEN)).await.1, "true");
    wait_agent_online(&agent).await;
    assert_eq!(check(&env, &node, Some(RELAY_ACCESS_TOKEN)).await.1, "true");
}
