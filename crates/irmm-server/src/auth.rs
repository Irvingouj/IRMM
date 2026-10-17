//! Password + mandatory TOTP login, bearer sessions, roles and client scoping.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::Argon2;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::Json;
use base64::Engine as _;
use rand::RngCore as _;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::db::{self, audit};
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Technician,
    Admin,
    Owner,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "viewer" => Self::Viewer,
            "technician" => Self::Technician,
            "admin" => Self::Admin,
            "owner" => Self::Owner,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Technician => "technician",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub role: Role,
    /// `None`: all clients of the tenant.
    pub clients: Option<Vec<Uuid>>,
    token_hash: String,
}

impl AuthUser {
    pub fn require(&self, min: Role) -> ApiResult<()> {
        if self.role >= min { Ok(()) } else { Err(ApiError::forbidden()) }
    }

    pub fn actor(&self) -> Option<(Uuid, &str)> {
        Some((self.user_id, self.email.as_str()))
    }

    pub async fn tx(&self, st: &crate::AppState) -> ApiResult<db::Tx> {
        Ok(db::tenant_tx(&st.pool, self.tenant_id).await?)
    }
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn hash_token(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))?
        .to_string())
}

fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

fn totp(secret_b32: &str, email: &str) -> anyhow::Result<totp_rs::TOTP> {
    let secret = totp_rs::Secret::Encoded(secret_b32.to_owned())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("totp secret: {e:?}"))?;
    Ok(totp_rs::TOTP::new_unchecked(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        secret,
        Some("IRMM".into()),
        email.to_owned(),
    ))
}

/// Bearer token from `Authorization`, or `?token=` (WebSockets cannot set headers).
fn bearer(parts: &Parts) -> Option<String> {
    if let Some(h) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        return h.to_str().ok()?.strip_prefix("Bearer ").map(str::to_owned);
    }
    parts.uri.query()?.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_owned))
}

impl FromRequestParts<crate::State> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &crate::State) -> ApiResult<Self> {
        let token = bearer(parts).ok_or_else(ApiError::unauthorized)?;
        let token_hash = hash_token(&token);
        let mut tx = db::system_tx(&st.pool).await?;
        let row: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
            "SELECT u.id, u.tenant_id, u.email, u.role FROM auth_sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.kind = 'session' AND s.expires_at > now()",
        )
        .bind(&token_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let (user_id, tenant_id, email, role) = row.ok_or_else(ApiError::unauthorized)?;
        let role = Role::parse(&role).ok_or_else(ApiError::unauthorized)?;
        let scoped: Vec<(Uuid,)> = sqlx::query_as("SELECT client_id FROM user_clients WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let clients = if role >= Role::Admin || scoped.is_empty() {
            None
        } else {
            Some(scoped.into_iter().map(|r| r.0).collect())
        };
        Ok(Self {
            user_id,
            tenant_id,
            email,
            role,
            clients,
            token_hash,
        })
    }
}

#[derive(Deserialize)]
pub struct SignupReq {
    tenant_name: String,
    email: String,
    password: String,
}

pub async fn signup(State(st): State<crate::State>, Json(req): Json<SignupReq>) -> ApiResult<Json<Value>> {
    if req.password.len() < 12 {
        return Err(ApiError::bad_request("password must be at least 12 characters"));
    }
    if req.tenant_name.trim().is_empty() || !req.email.contains('@') {
        return Err(ApiError::bad_request("tenant name and email required"));
    }
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let mut tx = db::system_tx(&st.pool).await?;
    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
        .bind(tenant)
        .bind(req.tenant_name.trim())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO users (id, tenant_id, email, pw_hash, role) VALUES ($1, $2, $3, $4, 'owner')")
        .bind(user)
        .bind(tenant)
        .bind(req.email.trim())
        .bind(hash_password(&req.password)?)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, tenant, Some((user, req.email.trim())), "tenant.create", None, json!({})).await?;
    tx.commit().await?;
    Ok(Json(json!({"tenant_id": tenant, "user_id": user})))
}

#[derive(Deserialize)]
pub struct LoginReq {
    email: String,
    password: String,
}

pub async fn login(State(st): State<crate::State>, Json(req): Json<LoginReq>) -> ApiResult<Json<Value>> {
    let mut tx = db::system_tx(&st.pool).await?;
    let row: Option<(Uuid, Uuid, String, Option<String>)> =
        sqlx::query_as("SELECT id, tenant_id, pw_hash, totp_secret FROM users WHERE lower(email) = lower($1)")
            .bind(&req.email)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((user_id, tenant_id, pw_hash, totp_secret)) = row else {
        return Err(ApiError::unauthorized());
    };
    if !verify_password(&req.password, &pw_hash) {
        audit(&mut tx, tenant_id, Some((user_id, &req.email)), "auth.login_failed", None, json!({})).await?;
        tx.commit().await?;
        return Err(ApiError::unauthorized());
    }
    let pending = random_token();
    let new_secret = totp_secret.is_none().then(|| totp_rs::Secret::generate_secret().to_encoded().to_string());
    sqlx::query(
        "INSERT INTO auth_sessions (token_hash, tenant_id, user_id, kind, pending_secret, expires_at)
         VALUES ($1, $2, $3, 'pending', $4, now() + interval '10 minutes')",
    )
    .bind(hash_token(&pending))
    .bind(tenant_id)
    .bind(user_id)
    .bind(&new_secret)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(match new_secret {
        Some(secret) => json!({
            "step": "totp_setup",
            "pending_token": pending,
            "otpauth_url": totp(&secret, &req.email)?.get_url(),
            "totp_secret": secret,
        }),
        None => json!({"step": "totp", "pending_token": pending}),
    }))
}

#[derive(Deserialize)]
pub struct TotpReq {
    pending_token: String,
    code: String,
}

pub async fn totp_verify(State(st): State<crate::State>, Json(req): Json<TotpReq>) -> ApiResult<Json<Value>> {
    let mut tx = db::system_tx(&st.pool).await?;
    let row: Option<(Uuid, Uuid, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT u.id, u.tenant_id, u.email, u.totp_secret, s.pending_secret FROM auth_sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.kind = 'pending' AND s.expires_at > now()",
    )
    .bind(hash_token(&req.pending_token))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, tenant_id, email, current, pending_secret)) = row else {
        return Err(ApiError::unauthorized());
    };
    let secret = pending_secret.clone().or(current).ok_or_else(ApiError::unauthorized)?;
    if !totp(&secret, &email)?.check_current(req.code.trim()).unwrap_or(false) {
        audit(&mut tx, tenant_id, Some((user_id, &email)), "auth.totp_failed", None, json!({})).await?;
        tx.commit().await?;
        return Err(ApiError::unauthorized());
    }
    if let Some(s) = pending_secret {
        sqlx::query("UPDATE users SET totp_secret = $1 WHERE id = $2").bind(s).bind(user_id).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM auth_sessions WHERE token_hash = $1")
        .bind(hash_token(&req.pending_token))
        .execute(&mut *tx)
        .await?;
    let token = random_token();
    sqlx::query(
        "INSERT INTO auth_sessions (token_hash, tenant_id, user_id, kind, expires_at)
         VALUES ($1, $2, $3, 'session', now() + interval '12 hours')",
    )
    .bind(hash_token(&token))
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, tenant_id, Some((user_id, &email)), "auth.login", None, json!({})).await?;
    tx.commit().await?;
    Ok(Json(json!({"token": token})))
}

pub async fn logout(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    sqlx::query("DELETE FROM auth_sessions WHERE token_hash = $1")
        .bind(&user.token_hash)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "auth.logout", None, json!({})).await?;
    tx.commit().await?;
    Ok(Json(json!({})))
}

pub async fn me(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let (tenant_name,): (String,) = sqlx::query_as("SELECT name FROM tenants WHERE id = $1")
        .bind(user.tenant_id)
        .fetch_one(&mut *tx)
        .await?;
    Ok(Json(json!({
        "id": user.user_id,
        "email": user.email,
        "role": user.role.as_str(),
        "tenant_id": user.tenant_id,
        "tenant_name": tenant_name,
        "client_ids": user.clients,
    })))
}
