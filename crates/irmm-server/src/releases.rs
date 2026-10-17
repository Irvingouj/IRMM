use axum::Router;
use uuid::Uuid;

pub fn routes() -> Router<crate::State> {
    Router::new()
}

pub async fn offer_update(_st: &crate::State, _device: Uuid) {}
