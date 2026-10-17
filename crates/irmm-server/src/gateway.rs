//! WSS gateway (fallback transport) and HTTP file transfer, both bridged to agent channels over Iroh.

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt as _, StreamExt as _};
use irmm_proto::{ChannelKind, FilesReply, FilesRequest, read_frame, write_frame};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use crate::api::{issue_ticket, visible_device};
use crate::auth::{AuthUser, Role};
use crate::db::audit;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<crate::State> {
    Router::new()
        .route("/api/gw/{device}", get(ws_channel))
        .route("/api/devices/{id}/files", get(list_files))
        .route("/api/devices/{id}/files/download", get(download))
        .route("/api/devices/{id}/files/upload", post(upload).layer(DefaultBodyLimit::disable()))
        .route("/api/sessions/{id}/rtc", post(rtc_offer))
}

#[derive(Deserialize)]
struct TicketQuery {
    ticket: String,
}

/// Validates the ticket and opens the agent channel *before* upgrading, so a bad
/// ticket or an offline agent is an HTTP error the browser can act on.
async fn ws_channel(
    State(st): State<crate::State>,
    Path(device): Path<Uuid>,
    Query(q): Query<TicketQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match irmm_proto::verify_ticket(&st.endpoint.id(), &q.ticket) {
        Ok(c) if c.device_id == device => c,
        _ => return (StatusCode::FORBIDDEN, "bad ticket").into_response(),
    };
    if !st.agents.consume_jti(claims.jti, claims.exp) {
        return (StatusCode::FORBIDDEN, "ticket already used").into_response();
    }
    let (send, recv) = match st.agents.open_channel(device, &q.ticket).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    ws.on_upgrade(move |socket| pump(socket, send, recv))
}

async fn pump(socket: WebSocket, mut send: iroh::endpoint::SendStream, mut recv: iroh::endpoint::RecvStream) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let up = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(b) => {
                    if send.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = send.finish();
    };
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };
    tokio::join!(up, down);
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

/// Opens a `Files` channel on behalf of the user and sends one request.
async fn files_channel(
    st: &crate::State,
    user: &AuthUser,
    device: Uuid,
    req: &FilesRequest,
) -> ApiResult<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
    let mut tx = user.tx(st).await?;
    visible_device(&mut tx, user, device).await?;
    user.require(Role::Technician)?;
    let action = match req {
        FilesRequest::List { .. } => "file.list",
        FilesRequest::Download { .. } => "file.download",
        FilesRequest::Upload { .. } => "file.upload",
    };
    audit(&mut tx, user.tenant_id, user.actor(), action, Some(device.to_string()), json!({"request": req})).await?;
    tx.commit().await?;
    let ticket = issue_ticket(st, user, device, Uuid::new_v4(), ChannelKind::Files);
    let (mut send, recv) = st
        .agents
        .open_channel(device, &ticket)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    write_frame(&mut send, req).await?;
    Ok((send, recv))
}

async fn reply(recv: &mut iroh::endpoint::RecvStream) -> ApiResult<FilesReply> {
    match read_frame::<_, FilesReply>(recv).await? {
        Some(FilesReply::Error { message }) => Err(ApiError::bad_request(message)),
        Some(r) => Ok(r),
        None => Err(ApiError(StatusCode::BAD_GATEWAY, "agent closed channel".into())),
    }
}

async fn list_files(
    State(st): State<crate::State>,
    user: AuthUser,
    Path(device): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    let (_send, mut recv) = files_channel(&st, &user, device, &FilesRequest::List { path: q.path }).await?;
    match reply(&mut recv).await? {
        FilesReply::List { entries } => Ok(Json(serde_json::to_value(entries).unwrap())),
        _ => Err(ApiError::bad_request("unexpected reply")),
    }
}

async fn download(
    State(st): State<crate::State>,
    user: AuthUser,
    Path(device): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Response> {
    let name = q.path.rsplit(['/', '\\']).next().unwrap_or("download").to_owned();
    let (_send, mut recv) = files_channel(&st, &user, device, &FilesRequest::Download { path: q.path }).await?;
    let FilesReply::Download { size } = reply(&mut recv).await? else {
        return Err(ApiError::bad_request("unexpected reply"));
    };
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(recv.take(size)));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", name.replace('"', ""))),
        ],
        body,
    )
        .into_response())
}

async fn upload(
    State(st): State<crate::State>,
    user: AuthUser,
    Path(device): Path<Uuid>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    let size: u64 = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .ok_or_else(|| ApiError::bad_request("Content-Length required"))?;
    let (mut send, mut recv) = files_channel(&st, &user, device, &FilesRequest::Upload { path: q.path, size }).await?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(e.to_string()))?;
        send.write_all(&chunk).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e.to_string()))?;
    }
    send.flush().await.ok();
    match reply(&mut recv).await? {
        FilesReply::Done => Ok(Json(json!({"size": size}))),
        _ => Err(ApiError::bad_request("unexpected reply")),
    }
}

#[derive(Deserialize)]
struct RtcReq {
    sdp: String,
}

/// WebRTC signaling: relays the browser's (non-trickle) offer to the agent.
async fn rtc_offer(
    State(st): State<crate::State>,
    user: AuthUser,
    Path(session): Path<Uuid>,
    Json(req): Json<RtcReq>,
) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as("SELECT device_id, user_id FROM remote_sessions WHERE id = $1")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let (device, owner) = row.ok_or_else(ApiError::not_found)?;
    if owner != user.user_id {
        return Err(ApiError::not_found());
    }
    visible_device(&mut tx, &user, device).await?;
    tx.commit().await?;
    let answer = st
        .agents
        .rtc_offer(device, req.sdp)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({"sdp": answer})))
}
