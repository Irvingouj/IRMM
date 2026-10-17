//! One-command agent install: `install.sh` (Linux, macOS) and `install.ps1` (Windows)
//! download the latest release for the machine's platform and run `irmm-agent install`.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::{Value, json};

pub fn routes() -> Router<crate::State> {
    Router::new().route("/install.sh", get(install_sh)).route("/install.ps1", get(install_ps1))
}

/// One-line install commands for each OS.
pub fn commands(public_url: &str, token: &str) -> Value {
    json!({
        "linux": format!("curl -fsSL {public_url}/install.sh | sudo sh -s -- {token}"),
        "macos": format!("curl -fsSL {public_url}/install.sh | sudo sh -s -- {token}"),
        "windows": format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"& ([scriptblock]::Create((irm {public_url}/install.ps1))) -Token {token}\""),
    })
}

fn quoted(url: &str) -> String {
    url.replace('\'', "")
}

async fn install_sh(State(st): State<crate::State>) -> impl IntoResponse {
    let script = format!(
        r#"#!/bin/sh
# IRMM agent installer. Usage: install.sh <enrollment-token>
set -eu
URL='{url}'
TOKEN="${{1:?usage: install.sh <enrollment-token>}}"
case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "irmm: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "irmm: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -fsSL "$URL/api/agent/releases/$os-$arch/latest" -o "$tmp"
chmod +x "$tmp"
"$tmp" install --server "$URL" --token "$TOKEN"
"#,
        url = quoted(&st.public_url)
    );
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script)
}

async fn install_ps1(State(st): State<crate::State>) -> impl IntoResponse {
    let script = format!(
        r#"# IRMM agent installer. Run as Administrator.
param([Parameter(Mandatory = $true)][string]$Token)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Url = '{url}'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {{ 'aarch64' }} else {{ 'x86_64' }}
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("irmm-agent-" + [guid]::NewGuid() + ".exe")
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -UseBasicParsing -Uri "$Url/api/agent/releases/windows-$arch/latest" -OutFile $tmp
try {{
    & $tmp install --server $Url --token $Token
    if ($LASTEXITCODE -ne 0) {{ throw "irmm-agent install failed with exit code $LASTEXITCODE" }}
}} finally {{
    Remove-Item -Force $tmp -ErrorAction SilentlyContinue
}}
"#,
        url = quoted(&st.public_url)
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], script)
}
