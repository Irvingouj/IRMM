use std::path::Path;

use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use irmm_proto::{ChannelKind, TicketClaims};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{self, AuthUser, Role};
use crate::db::{self, audit};
use crate::error::{ApiError, ApiResult};

pub fn router(st: crate::State, static_dir: Option<&Path>) -> Router {
    let api = Router::new()
        .route("/api/signup", post(auth::signup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/totp", post(auth::totp_verify))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/me", get(auth::me))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/clients", get(list_clients).post(create_client))
        .route("/api/clients/{id}/sites", get(list_sites).post(create_site))
        .route("/api/clients/{id}/mesh", put(set_mesh))
        .route("/api/sites/{id}/enroll-tokens", post(create_enroll_token))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", get(get_device))
        .route("/api/devices/{id}/sessions", post(create_session))
        .route("/api/audit", get(list_audit))
        .route("/api/agent/bootstrap", get(bootstrap))
        .merge(crate::jobs::routes())
        .merge(crate::gateway::routes())
        .merge(crate::releases::routes())
        .merge(crate::install::routes())
        .merge(crate::relay::routes())
        .with_state(st);
    match static_dir {
        Some(dir) => api.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .fallback(tower_http::services::ServeFile::new(dir.join("index.html"))),
        ),
        None => api,
    }
}

#[derive(serde::Deserialize)]
struct BootstrapQuery {
    node: Option<String>,
    token: Option<String>,
}

/// Public server parameters for a new agent. An agent that presents a usable
/// enrollment token may use the relay while it enrolls.
async fn bootstrap(State(st): State<crate::State>, Query(q): Query<BootstrapQuery>) -> ApiResult<Json<irmm_proto::Bootstrap>> {
    if let (Some(node), Some(token)) = (q.node, q.token) {
        if node.parse::<iroh::PublicKey>().is_ok() && crate::relay::enroll_token_usable(&st, &token).await? {
            st.agents.grant_relay(node);
        }
    }
    Ok(Json(irmm_proto::Bootstrap {
        server_id: st.endpoint.id().to_string(),
        direct_addrs: st.iroh_public_addrs.clone(),
        relay_url: st.relay_url.clone(),
        release_key: st.release_key.to_string(),
        ice_servers: st.ice_servers.clone(),
    }))
}

async fn list_users(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    let mut tx = user.tx(&st).await?;
    let rows: Vec<(Uuid, String, String, bool, Vec<Uuid>)> = sqlx::query_as(
        "SELECT u.id, u.email, u.role, u.totp_secret IS NOT NULL,
                coalesce(array_agg(uc.client_id) FILTER (WHERE uc.client_id IS NOT NULL), '{}')
         FROM users u LEFT JOIN user_clients uc ON uc.user_id = u.id GROUP BY u.id ORDER BY u.created_at",
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, email, role, totp, clients)| json!({"id": id, "email": email, "role": role, "totp_enabled": totp, "client_ids": clients}))
            .collect(),
    )))
}

#[derive(Deserialize)]
struct CreateUser {
    email: String,
    password: String,
    role: String,
    #[serde(default)]
    client_ids: Vec<Uuid>,
}

async fn create_user(State(st): State<crate::State>, user: AuthUser, Json(req): Json<CreateUser>) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    let role = Role::parse(&req.role).ok_or_else(|| ApiError::bad_request("bad role"))?;
    if role > user.role || (role == Role::Owner && user.role != Role::Owner) {
        return Err(ApiError::forbidden());
    }
    if req.password.len() < 12 {
        return Err(ApiError::bad_request("password must be at least 12 characters"));
    }
    let id = Uuid::new_v4();
    let mut tx = user.tx(&st).await?;
    sqlx::query("INSERT INTO users (id, tenant_id, email, pw_hash, role) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(user.tenant_id)
        .bind(req.email.trim())
        .bind(auth::hash_password(&req.password)?)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
    for c in &req.client_ids {
        sqlx::query("INSERT INTO user_clients (tenant_id, user_id, client_id) VALUES ($1, $2, $3)")
            .bind(user.tenant_id)
            .bind(id)
            .bind(c)
            .execute(&mut *tx)
            .await?;
    }
    audit(&mut tx, user.tenant_id, user.actor(), "user.create", Some(req.email.clone()), json!({"role": role.as_str()})).await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id})))
}

type ClientRow = (Uuid, String, bool, String, Option<String>);

const CLIENT_SELECT: &str = "SELECT id, name, mesh_enabled, mesh_ipv4_range::text, mesh_ipv6_prefix::text FROM clients
     WHERE ($1::uuid[] IS NULL OR id = ANY($1))";

fn client_json((id, name, mesh_enabled, range, prefix): ClientRow) -> Value {
    json!({"id": id, "name": name, "mesh_enabled": mesh_enabled, "mesh_ipv4_range": range, "mesh_ipv6_prefix": prefix})
}

async fn list_clients(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let rows: Vec<ClientRow> = sqlx::query_as(&format!("{CLIENT_SELECT} ORDER BY name"))
        .bind(&user.clients)
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(Value::Array(rows.into_iter().map(client_json).collect())))
}

#[derive(Deserialize)]
struct MeshReq {
    enabled: bool,
}

/// Turns the client's mesh network on or off and pushes the change to its online devices.
async fn set_mesh(
    State(st): State<crate::State>,
    user: AuthUser,
    UrlPath(client): UrlPath<Uuid>,
    Json(req): Json<MeshReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    visible_client(&mut tx, &user, client).await?;
    user.require(Role::Admin)?;
    sqlx::query("UPDATE clients SET mesh_enabled = $2 WHERE id = $1")
        .bind(client)
        .bind(req.enabled)
        .execute(&mut *tx)
        .await?;
    crate::mesh::ensure_addrs(&mut tx, client).await?;
    audit(&mut tx, user.tenant_id, user.actor(), "client.mesh", Some(client.to_string()), json!({"enabled": req.enabled})).await?;
    let row: ClientRow = sqlx::query_as(&format!("{CLIENT_SELECT} AND id = $2"))
        .bind(&user.clients)
        .bind(client)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Err(e) = crate::mesh::push_client(&st, user.tenant_id, client).await {
        tracing::warn!(error = %format!("{e:#}"), %client, "mesh config push");
    }
    Ok(Json(client_json(row)))
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

async fn create_client(State(st): State<crate::State>, user: AuthUser, Json(req): Json<NameReq>) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    let id = Uuid::new_v4();
    let mut tx = user.tx(&st).await?;
    sqlx::query("INSERT INTO clients (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(user.tenant_id)
        .bind(req.name.trim())
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "client.create", Some(id.to_string()), json!({"name": req.name})).await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id, "name": req.name})))
}

async fn visible_client(tx: &mut db::Tx, user: &AuthUser, client: Uuid) -> ApiResult<()> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM clients WHERE id = $1 AND ($2::uuid[] IS NULL OR id = ANY($2))")
            .bind(client)
            .bind(&user.clients)
            .fetch_optional(&mut **tx)
            .await?;
    found.map(|_| ()).ok_or_else(ApiError::not_found)
}

async fn list_sites(State(st): State<crate::State>, user: AuthUser, UrlPath(client): UrlPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    visible_client(&mut tx, &user, client).await?;
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM sites WHERE client_id = $1 ORDER BY name")
        .bind(client)
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(Value::Array(rows.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect())))
}

async fn create_site(
    State(st): State<crate::State>,
    user: AuthUser,
    UrlPath(client): UrlPath<Uuid>,
    Json(req): Json<NameReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    visible_client(&mut tx, &user, client).await?;
    user.require(Role::Admin)?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO sites (id, tenant_id, client_id, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(user.tenant_id)
        .bind(client)
        .bind(req.name.trim())
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "site.create", Some(id.to_string()), json!({"name": req.name})).await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id, "name": req.name})))
}

#[derive(Deserialize)]
struct TokenReq {
    max_uses: i32,
    ttl_hours: i64,
}

async fn create_enroll_token(
    State(st): State<crate::State>,
    user: AuthUser,
    UrlPath(site): UrlPath<Uuid>,
    Json(req): Json<TokenReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let found: Option<(Uuid,)> = sqlx::query_as("SELECT client_id FROM sites WHERE id = $1")
        .bind(site)
        .fetch_optional(&mut *tx)
        .await?;
    let (client,) = found.ok_or_else(ApiError::not_found)?;
    visible_client(&mut tx, &user, client).await?;
    user.require(Role::Admin)?;
    if !(1..=10_000).contains(&req.max_uses) || !(1..=24 * 365).contains(&req.ttl_hours) {
        return Err(ApiError::bad_request("max_uses 1..10000, ttl_hours 1..8760"));
    }
    let id = Uuid::new_v4();
    let token = auth::random_token();
    let (expires_at,): (chrono::DateTime<chrono::Utc>,) = sqlx::query_as(
        "INSERT INTO enroll_tokens (id, tenant_id, site_id, token_hash, uses_left, expires_at, created_by)
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(hours => $6::int), $7) RETURNING expires_at",
    )
    .bind(id)
    .bind(user.tenant_id)
    .bind(site)
    .bind(auth::hash_token(&token))
    .bind(req.max_uses)
    .bind(req.ttl_hours as i32)
    .bind(user.user_id)
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "enroll_token.create", Some(site.to_string()), json!({"max_uses": req.max_uses})).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "id": id,
        "token": token,
        "expires_at": expires_at,
        "install": crate::install::commands(&st.public_url, &token),
    })))
}

type DeviceRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    String,
    String,
    String,
    String,
    chrono::DateTime<chrono::Utc>,
    String,
    String,
    Value,
    Option<String>,
    Option<String>,
);

const DEVICE_SELECT: &str = "SELECT d.id, d.site_id, s.client_id, s.name, c.name, d.hostname, d.os, d.os_version,
        d.agent_version, d.last_seen, d.arch, d.node_id, d.inventory, host(m.ipv4), host(m.ipv6)
     FROM devices d JOIN sites s ON s.id = d.site_id JOIN clients c ON c.id = s.client_id
     LEFT JOIN mesh_addrs m ON m.device_id = d.id AND c.mesh_enabled
     WHERE ($1::uuid[] IS NULL OR s.client_id = ANY($1))";

fn device_json(st: &crate::AppState, r: DeviceRow, with_inventory: bool) -> Value {
    let (id, site_id, client_id, site_name, client_name, hostname, os, os_version, agent_version, last_seen, arch, _node, inv, mesh_ipv4, mesh_ipv6) = r;
    let mut v = json!({
        "id": id,
        "site_id": site_id,
        "client_id": client_id,
        "site_name": site_name,
        "client_name": client_name,
        "hostname": hostname,
        "os": os,
        "os_version": os_version,
        "arch": arch,
        "agent_version": agent_version,
        "last_seen": last_seen,
        "online": st.agents.is_online(id),
        "mesh_ipv4": mesh_ipv4,
        "mesh_ipv6": mesh_ipv6,
    });
    if with_inventory {
        v["inventory"] = inv;
    }
    v
}

pub async fn visible_device(tx: &mut db::Tx, user: &AuthUser, id: Uuid) -> ApiResult<DeviceRow> {
    let row: Option<DeviceRow> = sqlx::query_as(&format!("{DEVICE_SELECT} AND d.id = $2"))
        .bind(&user.clients)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.ok_or_else(ApiError::not_found)
}

async fn list_devices(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let rows: Vec<DeviceRow> = sqlx::query_as(&format!("{DEVICE_SELECT} ORDER BY c.name, s.name, d.hostname"))
        .bind(&user.clients)
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(Value::Array(rows.into_iter().map(|r| device_json(&st, r, false)).collect())))
}

async fn get_device(State(st): State<crate::State>, user: AuthUser, UrlPath(id): UrlPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let row = visible_device(&mut tx, &user, id).await?;
    Ok(Json(device_json(&st, row, true)))
}

#[derive(Deserialize)]
struct SessionReq {
    kind: ChannelKind,
    #[serde(default)]
    monitor: u32,
}

/// Issues a short-lived, single-use ticket for one channel to the device.
async fn create_session(
    State(st): State<crate::State>,
    user: AuthUser,
    UrlPath(device): UrlPath<Uuid>,
    Json(req): Json<SessionReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    visible_device(&mut tx, &user, device).await?;
    user.require(Role::Technician)?;
    if !st.agents.is_online(device) {
        return Err(ApiError::conflict("device offline"));
    }
    let session_id = Uuid::new_v4();
    sqlx::query("INSERT INTO remote_sessions (id, tenant_id, device_id, user_id, kind) VALUES ($1, $2, $3, $4, $5)")
        .bind(session_id)
        .bind(user.tenant_id)
        .bind(device)
        .bind(user.user_id)
        .bind(serde_json::to_value(req.kind).unwrap().as_str().unwrap())
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "session.create", Some(device.to_string()), json!({"kind": req.kind, "session_id": session_id})).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "session_id": session_id,
        "kind": req.kind,
        "ticket": issue_ticket(&st, &user, device, session_id, req.kind, req.monitor),
        "ice_servers": st.ice_servers,
    })))
}

pub fn issue_ticket(st: &crate::AppState, user: &AuthUser, device: Uuid, session_id: Uuid, kind: ChannelKind, monitor: u32) -> String {
    irmm_proto::sign_ticket(
        &st.secret_key,
        &TicketClaims {
            jti: Uuid::new_v4(),
            session_id,
            tenant_id: user.tenant_id,
            device_id: device,
            user_id: user.user_id,
            kind,
            monitor,
            exp: irmm_proto::now_secs() + 120,
        },
    )
}

async fn list_audit(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    let mut tx = user.tx(&st).await?;
    let rows: Vec<(i64, Option<String>, String, Option<String>, Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, actor_email, action, target, detail, at FROM audit_log ORDER BY id DESC LIMIT 500",
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, actor, action, target, detail, at)| json!({"id": id, "actor_email": actor, "action": action, "target": target, "detail": detail, "at": at}))
            .collect(),
    )))
}
