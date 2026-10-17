//! Script library and batch jobs. A job fans out into one run per device.

use axum::extract::{Path, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use irmm_proto::{ServerMsg, Shell};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::visible_device;
use crate::auth::{AuthUser, Role};
use crate::db::{self, audit};
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<crate::State> {
    Router::new()
        .route("/api/scripts", get(list_scripts).post(create_script))
        .route("/api/scripts/{id}", put(update_script).delete(delete_script))
        .route("/api/jobs", get(list_jobs).post(create_job))
        .route("/api/jobs/{id}", get(get_job))
}

fn parse_shell(s: &str) -> ApiResult<Shell> {
    serde_json::from_value(json!(s)).map_err(|_| ApiError::bad_request("unknown shell"))
}

#[derive(Deserialize)]
struct ScriptReq {
    name: String,
    shell: String,
    body: String,
}

async fn list_scripts(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as("SELECT id, name, shell, body FROM scripts ORDER BY name")
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(Value::Array(
        rows.into_iter().map(|(id, name, shell, body)| json!({"id": id, "name": name, "shell": shell, "body": body})).collect(),
    )))
}

async fn create_script(State(st): State<crate::State>, user: AuthUser, Json(req): Json<ScriptReq>) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    parse_shell(&req.shell)?;
    let id = Uuid::new_v4();
    let mut tx = user.tx(&st).await?;
    sqlx::query("INSERT INTO scripts (id, tenant_id, name, shell, body, created_by) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(user.tenant_id)
        .bind(&req.name)
        .bind(&req.shell)
        .bind(&req.body)
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, user.tenant_id, user.actor(), "script.create", Some(id.to_string()), json!({"name": req.name})).await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id})))
}

async fn update_script(
    State(st): State<crate::State>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(req): Json<ScriptReq>,
) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    parse_shell(&req.shell)?;
    let mut tx = user.tx(&st).await?;
    let n = sqlx::query("UPDATE scripts SET name = $2, shell = $3, body = $4, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(&req.name)
        .bind(&req.shell)
        .bind(&req.body)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::not_found());
    }
    audit(&mut tx, user.tenant_id, user.actor(), "script.update", Some(id.to_string()), json!({"name": req.name})).await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id})))
}

async fn delete_script(State(st): State<crate::State>, user: AuthUser, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require(Role::Admin)?;
    let mut tx = user.tx(&st).await?;
    let n = sqlx::query("DELETE FROM scripts WHERE id = $1").bind(id).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found());
    }
    audit(&mut tx, user.tenant_id, user.actor(), "script.delete", Some(id.to_string()), json!({})).await?;
    tx.commit().await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct JobReq {
    device_ids: Vec<Uuid>,
    script_id: Option<Uuid>,
    shell: Option<String>,
    body: Option<String>,
    timeout_secs: Option<u64>,
}

async fn create_job(State(st): State<crate::State>, user: AuthUser, Json(req): Json<JobReq>) -> ApiResult<Json<Value>> {
    user.require(Role::Technician)?;
    let mut tx = user.tx(&st).await?;
    let (shell, body) = match (req.script_id, req.shell, req.body) {
        (Some(sid), _, _) => {
            let row: Option<(String, String)> = sqlx::query_as("SELECT shell, body FROM scripts WHERE id = $1")
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await?;
            row.ok_or_else(ApiError::not_found)?
        }
        (None, Some(shell), Some(body)) => (shell, body),
        _ => return Err(ApiError::bad_request("script_id or shell+body required")),
    };
    let shell_enum = parse_shell(&shell)?;
    if req.device_ids.is_empty() {
        return Err(ApiError::bad_request("no devices"));
    }
    for d in &req.device_ids {
        visible_device(&mut tx, &user, *d).await?;
    }
    let timeout = req.timeout_secs.unwrap_or(300).clamp(1, 24 * 3600);
    let job = Uuid::new_v4();
    sqlx::query("INSERT INTO jobs (id, tenant_id, script_id, shell, body, timeout_secs, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(job)
        .bind(user.tenant_id)
        .bind(req.script_id)
        .bind(&shell)
        .bind(&body)
        .bind(timeout as i32)
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
    let mut runs = Vec::new();
    for d in &req.device_ids {
        let run = Uuid::new_v4();
        let online = st.agents.is_online(*d);
        sqlx::query(
            "INSERT INTO job_runs (id, tenant_id, job_id, device_id, status, error, started_at, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, now(), CASE WHEN $5 = 'failed' THEN now() END)",
        )
        .bind(run)
        .bind(user.tenant_id)
        .bind(job)
        .bind(d)
        .bind(if online { "running" } else { "failed" })
        .bind((!online).then_some("device offline"))
        .execute(&mut *tx)
        .await?;
        if online {
            runs.push((*d, run));
        }
    }
    audit(&mut tx, user.tenant_id, user.actor(), "job.create", Some(job.to_string()), json!({"devices": req.device_ids, "script_id": req.script_id})).await?;
    tx.commit().await?;
    // Dispatch after commit so results can never race the run rows.
    for (device, run_id) in runs {
        if !st.agents.send(device, ServerMsg::RunScript { run_id, shell: shell_enum, body: body.clone(), timeout_secs: timeout }) {
            let mut tx = user.tx(&st).await?;
            sqlx::query("UPDATE job_runs SET status = 'failed', error = 'device offline', finished_at = now() WHERE id = $1")
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    Ok(Json(json!({"id": job})))
}

async fn list_jobs(State(st): State<crate::State>, user: AuthUser) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let rows: Vec<(Uuid, String, String, chrono::DateTime<chrono::Utc>, i64, i64)> = sqlx::query_as(
        "SELECT j.id, j.shell, left(j.body, 200), j.created_at,
                count(r.id), count(r.id) FILTER (WHERE r.status IN ('succeeded', 'failed'))
         FROM jobs j JOIN job_runs r ON r.job_id = j.id
         JOIN devices d ON d.id = r.device_id JOIN sites s ON s.id = d.site_id
         WHERE ($1::uuid[] IS NULL OR s.client_id = ANY($1))
         GROUP BY j.id ORDER BY j.created_at DESC LIMIT 200",
    )
    .bind(&user.clients)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, shell, body, at, total, done)| json!({"id": id, "shell": shell, "body": body, "created_at": at, "runs": total, "finished": done}))
            .collect(),
    )))
}

type RunRow = (Uuid, Uuid, String, String, Option<i32>, Option<String>, Option<String>, Option<String>);

async fn get_job(State(st): State<crate::State>, user: AuthUser, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = user.tx(&st).await?;
    let job: Option<(String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as("SELECT shell, body, created_at FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let (shell, body, at) = job.ok_or_else(ApiError::not_found)?;
    let runs: Vec<RunRow> = sqlx::query_as(
        "SELECT r.id, r.device_id, d.hostname, r.status, r.exit_code, r.stdout, r.stderr, r.error
         FROM job_runs r JOIN devices d ON d.id = r.device_id JOIN sites s ON s.id = d.site_id
         WHERE r.job_id = $1 AND ($2::uuid[] IS NULL OR s.client_id = ANY($2)) ORDER BY d.hostname",
    )
    .bind(id)
    .bind(&user.clients)
    .fetch_all(&mut *tx)
    .await?;
    if runs.is_empty() {
        return Err(ApiError::not_found());
    }
    Ok(Json(json!({
        "id": id,
        "shell": shell,
        "body": body,
        "created_at": at,
        "runs": runs.into_iter().map(|(rid, dev, host, status, code, out, err, error)| json!({
            "id": rid, "device_id": dev, "hostname": host, "status": status, "exit_code": code,
            "stdout": out, "stderr": err, "error": error,
        })).collect::<Vec<_>>(),
    })))
}

#[allow(clippy::too_many_arguments)]
pub async fn record_result(
    st: &crate::State,
    tenant: Uuid,
    device: Uuid,
    run_id: Uuid,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
) -> anyhow::Result<()> {
    let status = if error.is_none() && exit_code == Some(0) { "succeeded" } else { "failed" };
    let mut tx = db::tenant_tx(&st.pool, tenant).await?;
    sqlx::query(
        "UPDATE job_runs SET status = $3, exit_code = $4, stdout = $5, stderr = $6, error = $7, finished_at = now()
         WHERE id = $1 AND device_id = $2 AND status = 'running'",
    )
    .bind(run_id)
    .bind(device)
    .bind(status)
    .bind(exit_code)
    .bind(stdout)
    .bind(stderr)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
