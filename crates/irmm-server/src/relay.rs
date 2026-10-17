//! Access checks for the self-hosted iroh-relay (`access.http` in its config): only the
//! server, enrolled devices and agents enrolling with a valid token may relay.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;

use crate::auth::hash_token;
use crate::db;
use crate::error::ApiResult;

pub fn routes() -> Router<crate::State> {
    Router::new().route("/api/relay/access", post(access))
}

async fn access(State(st): State<crate::State>, headers: HeaderMap) -> ApiResult<Response> {
    let bearer = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer "));
    match (&st.relay_access_token, bearer) {
        (Some(want), Some(got)) if hash_token(want) == hash_token(got) => {}
        _ => return Ok(StatusCode::UNAUTHORIZED.into_response()),
    }
    let Some(node) = headers.get("x-iroh-endpoint-id").and_then(|h| h.to_str().ok()).and_then(|s| s.parse::<iroh::PublicKey>().ok())
    else {
        return Ok((StatusCode::FORBIDDEN, "false").into_response());
    };
    let node = node.to_string();
    let allowed = node == st.endpoint.id().to_string() || st.agents.relay_granted(&node) || {
        let mut tx = db::system_tx(&st.pool).await?;
        let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM devices WHERE node_id = $1").bind(&node).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        found.is_some()
    };
    Ok(if allowed { (StatusCode::OK, "true") } else { (StatusCode::FORBIDDEN, "false") }.into_response())
}

pub async fn enroll_token_usable(st: &crate::AppState, token: &str) -> ApiResult<bool> {
    let mut tx = db::system_tx(&st.pool).await?;
    let found: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM enroll_tokens WHERE token_hash = $1 AND uses_left > 0 AND expires_at > now()")
            .bind(hash_token(token))
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(found.is_some())
}
