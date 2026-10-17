use axum::Router;
use serde_json::{Value, json};

pub fn routes() -> Router<crate::State> {
    Router::new()
}

/// One-line install commands for each OS.
pub fn commands(public_url: &str, token: &str) -> Value {
    json!({
        "linux": format!("curl -fsSL {public_url}/install.sh | sudo sh -s -- {token}"),
        "macos": format!("curl -fsSL {public_url}/install.sh | sudo sh -s -- {token}"),
        "windows": format!("powershell -ExecutionPolicy Bypass -Command \"& ([scriptblock]::Create((irm {public_url}/install.ps1))) -Token {token}\""),
    })
}
