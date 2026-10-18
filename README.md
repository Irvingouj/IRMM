# IRMM

Open-source, multi-tenant remote monitoring and management (RMM) for MSPs.
A small Rust agent runs on each machine and connects to the server over
[Iroh](https://www.iroh.computer/) (QUIC). Technicians work entirely in the browser:
remote desktop runs on [IronRDP](https://github.com/Devolutions/IronRDP) inside the
page, and terminals, scripts and file transfer run over WebRTC or a WebSocket gateway.

> **Status: early MVP.** Every feature below works end to end and is covered by
> network-level tests. It has not been security-audited or run at scale yet.
> Expect breaking changes.

## Features

| | |
|---|---|
| **Enrollment** | Per-site enrollment tokens; one command installs the agent as a system service (systemd / launchd / Windows service) |
| **Devices** | Online status, OS, hardware, IPs, logged-in user, last seen, monitors |
| **Remote desktop** | View and control in the browser, monitor switching, text clipboard, Ctrl+Alt+Del. On Windows it follows the input desktop, so it keeps working on UAC prompts and the lock and logon screens |
| **Terminal** | PowerShell / cmd as SYSTEM on Windows, a root shell on macOS and Linux |
| **Scripts** | Run on one device or many, collect output, keep a script library |
| **Files** | Upload and download |
| **Multi-tenancy** | Tenant → client → site → device; roles (viewer, technician, admin, owner); mandatory TOTP; append-only audit log; Postgres row-level security |
| **Auto-update** | Releases are signed with an offline ed25519 key that agents pin at enrollment, so a compromised server cannot push a binary you didn't sign |
| **Code signing** | `irmm-release codesign` signs releases with macOS codesign or Windows Authenticode so Gatekeeper and SmartScreen trust the installer |

Agent platforms: **Windows**, **macOS**, **Linux** (for remote desktop, X11 only; Wayland is not supported yet).

## Architecture

```
 browser (Svelte console)
   │  HTTPS API ─────────────┐
   │  WebRTC data channels ──┼──► agent         (preferred: direct, peer-to-peer)
   │  WSS gateway ───────────┤                  (fallback: relayed by the server)
   ▼                         │
 irmm-server ◄── Iroh/QUIC ──┘  agents keep one QUIC connection open to the server;
   │                            optional self-hosted iroh-relay for NAT traversal
 Postgres (RLS per tenant)
```

- Every session (desktop, shell, files) starts with a short-lived ticket that the server signs and the agent verifies, and each ticket can be used only once.
- Remote desktop: the agent embeds `ironrdp-server` and acts as an RDCleanPath endpoint. The browser runs the `iron-remote-desktop` web component.
- More detail (in Chinese): [`docs/design.md`](docs/design.md).

| Crate | |
|---|---|
| `irmm-server` | API, web console host, agent hub, WSS gateway |
| `irmm-agent` | Device agent and its installer (`run` / `install` / `uninstall`) |
| `irmm-desktop` | Screen capture and input for each platform, and the RDP host |
| `irmm-proto` | Wire protocol shared by server and agent |
| `irmm-release` | Operator tool: release keys, signing, publishing |
| `irmm-e2e` | Network-level end-to-end tests |
| `web/` | Svelte browser console |

## Self-hosting

You need a Linux host with Docker, a domain whose DNS points at it, and open ports
80/443 (TCP) and 7843 (UDP).

### 1. Create the release key (keep it offline)

The release key signs agent builds. Agents pin its public half when they enroll, so
treat the private key like a CA key and keep it off the server.

```sh
cargo run --release -p irmm-release -- keygen --out release.key
# prints the public key → IRMM_RELEASE_PUBLIC_KEY
```

### 2. Configure and start

`deploy/docker-compose.yml` reads its settings from the environment. Use a secrets
manager or a `deploy/.env` file (already git-ignored):

```sh
IRMM_DOMAIN=rmm.example.com
IRMM_DB_PASSWORD=...            # random
IRMM_RELEASE_PUBLIC_KEY=...     # from step 1
IRMM_OPERATOR_TOKEN=...         # random; lets irmm-release publish builds
```

```sh
cd deploy
docker compose up -d
```

This runs Postgres, `irmm-server` (API, console and agent hub) and Caddy (automatic
HTTPS). Open `https://rmm.example.com` and sign up. The first user owns the tenant and
must set up TOTP.

Optional: run a self-hosted iroh-relay for agents behind strict NATs. It needs its own
hostname for ports 80 and 443. Set `IRMM_RELAY_HOSTNAME`, `IRMM_ACME_EMAIL`,
`IRMM_RELAY_ACCESS_TOKEN` and `IRMM_RELAY_URL`, then run
`docker compose --profile relay up -d`. The relay asks the server whether each node is
allowed to connect.

### 3. Build, sign and publish the agent

The server only hands out agent binaries that you have published, so build one for
each platform you manage:

```sh
# on each target platform (here: Windows)
cargo build --release -p irmm-agent -p irmm-release
target/release/irmm-release codesign --platform windows-x86_64 --file target/release/irmm-agent.exe
IRMM_OPERATOR_TOKEN=... target/release/irmm-release publish --server https://rmm.example.com \
  --key release.key --version 0.1.0 --platform windows-x86_64 --file target/release/irmm-agent.exe
```

Platform names are `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `macos-x86_64`,
`windows-x86_64` and so on. Agents that are online update to a newly published version
on their own.

Code signing needs your own certificates, supplied through the environment:
`IRMM_MACOS_SIGN_IDENTITY` (a Developer ID Application identity; `-` makes an ad-hoc
signature for testing), or `IRMM_WINDOWS_PKCS12` and `IRMM_WINDOWS_PKCS12_PASSWORD`
(Windows signing uses [`osslsigncode`](https://github.com/mtrojnar/osslsigncode)).
Without them, macOS Gatekeeper and Windows SmartScreen and antivirus will likely flag
the agent.

### 4. Enroll devices

In the console, create a client and a site and copy the site's install command:

```sh
# Linux / macOS
curl -fsSL https://rmm.example.com/install.sh | sudo sh -s -- <site-token>
```
```powershell
# Windows (elevated)
powershell -NoProfile -ExecutionPolicy Bypass -Command "& ([scriptblock]::Create((irm https://rmm.example.com/install.ps1))) -Token <site-token>"
```

### Server configuration

| Variable | Default | |
|---|---|---|
| `IRMM_DATABASE_URL` | required | Postgres URL |
| `IRMM_RELEASE_PUBLIC_KEY` | required | from `irmm-release keygen` |
| `IRMM_PUBLIC_URL` | `http://<http addr>` | URL that agents and browsers use |
| `IRMM_HTTP_ADDR` | `0.0.0.0:8080` | API, console, gateway |
| `IRMM_IROH_ADDR` | `0.0.0.0:7843` | UDP port agents connect to |
| `IRMM_IROH_PUBLIC_ADDRS` | public URL host + Iroh port | comma-separated `host:port` addresses agents dial |
| `IRMM_DATA_DIR` | `./data` | server key and published releases |
| `IRMM_STATIC_DIR` | unset | built web console |
| `IRMM_ICE_SERVERS` | `stun:stun.l.google.com:19302` | comma-separated STUN/TURN URLs |
| `IRMM_RELAY_URL` | unset | self-hosted iroh-relay |
| `IRMM_RELAY_ACCESS_TOKEN` | unset | enables `/api/relay/access` for iroh-relay |
| `IRMM_OPERATOR_TOKEN` | unset | enables release publishing |

## Managed IRMM

Rather not run servers, relays and signing certificates yourself? A hosted version is
**coming soon**: the same open-source code with signed agents, relays, backups and
updates handled for you. Watch this repo to hear when it launches.

## Development

Tests run at the network level: a real server, real agents and a real Postgres. The
suite has no function-level unit tests.

```sh
# Postgres for the tests (override with IRMM_TEST_PG=postgres://...)
docker run -d --name irmm-test-pg -e POSTGRES_PASSWORD=irmm -p 55432:5432 postgres:17

cargo test --workspace --tests

# Browser console, end to end (Playwright drives a real server + agent)
cd web && npm ci && npx playwright install chromium && npx playwright test
```

`crates/irmm-e2e/tests/native_desktop_x11.rs` runs only on Linux and needs an X server,
for example `xvfb-run -a cargo test --workspace --tests`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in this project, as defined in the Apache-2.0 license, will be dual licensed
as above, without any additional terms or conditions.
