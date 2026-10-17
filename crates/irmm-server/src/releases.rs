//! Agent releases: published by the operator (signed with the offline release key),
//! served to agents, and offered to every online agent that runs another version.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use irmm_proto::{ReleaseInfo, ServerMsg};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::db;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<crate::State> {
    Router::new()
        .route("/api/operator/releases", post(publish).layer(DefaultBodyLimit::max(512 * 1024 * 1024)))
        .route("/api/agent/releases/{platform}/{version}", get(download))
        .route("/api/agent/releases/{platform}/latest", get(latest_binary))
}

fn check_operator(st: &crate::AppState, headers: &HeaderMap) -> ApiResult<()> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    match (&st.operator_token, token) {
        (Some(want), Some(got)) if !want.is_empty() && crate::auth::hash_token(want) == crate::auth::hash_token(got) => Ok(()),
        _ => Err(ApiError::unauthorized()),
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[derive(Deserialize)]
struct PublishQuery {
    platform: String,
    version: String,
    signature: String,
}

async fn publish(
    State(st): State<crate::State>,
    headers: HeaderMap,
    Query(q): Query<PublishQuery>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    check_operator(&st, &headers)?;
    if !valid_segment(&q.platform) || !valid_segment(&q.version) {
        return Err(ApiError::bad_request("bad platform or version"));
    }
    let sha256 = hex::encode(Sha256::digest(&body));
    let info = ReleaseInfo {
        version: q.version.clone(),
        platform: q.platform.clone(),
        url: String::new(),
        sha256: sha256.clone(),
        signature: q.signature.clone(),
    };
    info.verify(&st.release_key).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let dir = st.data_dir.join("releases").join(&q.platform);
    tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;
    tokio::fs::write(dir.join(&q.version), &body).await.map_err(anyhow::Error::from)?;
    let mut tx = db::system_tx(&st.pool).await?;
    sqlx::query(
        "INSERT INTO releases (platform, version, sha256, signature) VALUES ($1, $2, $3, $4)
         ON CONFLICT (platform, version) DO UPDATE SET sha256 = $3, signature = $4, created_at = now()",
    )
    .bind(&q.platform)
    .bind(&q.version)
    .bind(&sha256)
    .bind(&q.signature)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    for device in st.agents.online_devices() {
        offer_update(&st, device).await;
    }
    Ok(Json(json!({"sha256": sha256})))
}

pub async fn latest(st: &crate::AppState, platform: &str) -> anyhow::Result<Option<ReleaseInfo>> {
    let mut tx = db::system_tx(&st.pool).await?;
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT version, sha256, signature FROM releases WHERE platform = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(platform)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row.map(|(version, sha256, signature)| ReleaseInfo {
        url: format!("/api/agent/releases/{platform}/{version}"),
        version,
        platform: platform.to_owned(),
        sha256,
        signature,
    }))
}

/// Sends the newest release for the device's platform if it differs from what it runs.
pub async fn offer_update(st: &crate::State, device: Uuid) {
    let Some(link) = st.agents.get(device) else { return };
    let res = async {
        let mut tx = db::tenant_tx(&st.pool, link.tenant_id).await?;
        let row: Option<(String, String, String)> = sqlx::query_as("SELECT os, arch, agent_version FROM devices WHERE id = $1")
            .bind(device)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        let Some((os, arch, version)) = row else { return anyhow::Ok(()) };
        if let Some(rel) = latest(st, &format!("{os}-{arch}")).await? {
            if rel.version != version {
                st.agents.send(device, ServerMsg::Update(rel));
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %format!("{e:#}"), %device, "offer update");
    }
}

async fn serve_file(st: &crate::AppState, platform: &str, version: &str) -> ApiResult<Response> {
    if !valid_segment(platform) || !valid_segment(version) {
        return Err(ApiError::not_found());
    }
    let path = st.data_dir.join("releases").join(platform).join(version);
    let file = tokio::fs::File::open(&path).await.map_err(|_| ApiError::not_found())?;
    let len = file.metadata().await.map_err(anyhow::Error::from)?.len();
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream".to_owned()), (header::CONTENT_LENGTH, len.to_string())],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

async fn download(State(st): State<crate::State>, Path((platform, version)): Path<(String, String)>) -> ApiResult<Response> {
    serve_file(&st, &platform, &version).await
}

/// Used by the install scripts.
async fn latest_binary(State(st): State<crate::State>, Path(platform): Path<String>) -> ApiResult<Response> {
    let rel = latest(&st, &platform).await?.ok_or_else(ApiError::not_found)?;
    serve_file(&st, &platform, &rel.version).await
}
