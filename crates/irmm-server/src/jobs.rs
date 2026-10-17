use axum::Router;
use uuid::Uuid;

pub fn routes() -> Router<crate::State> {
    Router::new()
}

#[allow(clippy::too_many_arguments)]
pub async fn record_result(
    _st: &crate::State,
    _tenant: Uuid,
    _device: Uuid,
    _run_id: Uuid,
    _exit_code: Option<i32>,
    _stdout: String,
    _stderr: String,
    _error: Option<String>,
) -> anyhow::Result<()> {
    Ok(())
}
