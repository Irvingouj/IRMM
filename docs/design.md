# IRMM 设计文档（草案 v0.2）

面向 MSP 的多租户 RMM。远程连接层：**Iroh**（Agent ↔ Server）+ **WebRTC**（浏览器 ↔ Agent 直连）+ **IronRDP**（远程桌面）。控制台只用浏览器。

> 标注约定：**[代码]** 读源码得出、未运行；**[记忆]** 未在本仓库核实；**[待验证]** 必须在 PoC 中确认。

---

## 1. 目标与非目标

**目标**
- MSP 用浏览器管理多个客户的 Windows 设备：远程桌面、远程终端、脚本、文件传输。
- 认证、授权、审计全部由我们自己控制，不依赖 Windows 账号密码。
- Agent 只出站连接，不开任何入站端口。
- 浏览器和 Agent 之间优先走 **WebRTC 直连**，连不上时自动回退到 WSS 网关。

**非目标（P0）**
- macOS / Linux Agent（P2）。
- 原生桌面控制台。
- 自建 TURN 服务器（P0 用 WSS 网关兜底，见第 4 节）。

---

## 2. 整体架构

```
┌──────────── 浏览器（SvelteKit SPA）───────────┐
│  管理界面   iron-remote-desktop(WASM)  xterm.js │
└──┬──────────────┬──────────────┬──────────────┘
   │ HTTPS (REST   │ WSS（兜底：    │ WebRTC DataChannel（首选）
   │  + 信令)      │  RDP/终端/文件）│ DTLS，P2P 直连
   ▼              ▼              │
┌─────────────────────────────┐  │
│ irmm-server（单进程，P0）     │  │
│  ├─ API (axum)：租户/设备/脚本/用户/审计、WebRTC 信令转发
│  ├─ Gateway：WSS ↔ Iroh stream 桥接（兜底路径）  │
│  ├─ Agent 控制通道：Iroh endpoint                  │
│  └─ 静态文件：Svelte 构建产物                       │
│        Postgres                                  │
└──────────────────────┬──────────────────────────┘  │
                       │ Iroh (QUIC；直连或经 relay)  │
          ┌────────────┴───────────┐                 │
          │ irmm-relay (iroh-relay)│  自建，UDP 被封时走 HTTPS
          └────────────┬───────────┘                 │
                       ▼                             ▼
┌─────────────── Windows 设备 ────────────────────┐
│ irmm-agent（SYSTEM 服务，session 0）              │
│  ├─ Iroh endpoint：控制通道、信令、兜底数据通道      │
│  ├─ WebRTC 端点（str0m）：浏览器直连的数据通道       │
│  ├─ 通道分发：RDP / 终端 / 文件（与传输方式无关）     │
│  └─ 拉起 ↓                                        │
│ irmm-session（用户会话中的 SYSTEM 进程）             │
│  └─ ironrdp-server + DXGI 截屏 + 输入注入           │
└─────────────────────────────────────────────────┘
```

### 2.1 为什么还用 Iroh（浏览器方案下的诚实评估）
浏览器方案中，连接的一端是公网 Gateway，Agent 主动出站连接即可，**不需要 NAT 打洞**。
Iroh 在这里的价值是：
- 设备身份就是公钥（NodeId），不需要自己管理 mTLS 证书。
- UDP 被企业防火墙封锁时，可以经 relay 走 HTTPS 兜底 **[记忆]**。纯 QUIC 方案做不到这一点。
- 网络切换（Wi-Fi ↔ 有线、换 IP）后自动重连。
- 以后加原生控制台或 Gateway 多区域部署时，P2P 能力可以直接用上。

浏览器到 Agent 的直连由 WebRTC 负责（第 4 节）；Iroh 负责 Agent 的常驻控制通道、WebRTC 信令，以及兜底路径。

对照：Devolutions Gateway 的 agent tunnel 用的是 **quinn + 双向 TLS**，只走 UDP **[代码]**
（`devolutions-gateway/crates/agent-tunnel/src/lib.rs`、`listener.rs`）。
我们用 Iroh 替换它的位置。

---

## 3. 从参考项目复用什么

| 来源 | 内容 | 用法 |
|---|---|---|
| `IronRDP/crates/ironrdp-server` | RDP 服务端；`run_connection<S: AsyncRead+AsyncWrite+Send+Sync+Unpin>` (`server.rs:383`) | Agent 端直接用，stream 传 Iroh 双向流 **[待验证: Sync 约束]** |
| `IronRDP/crates/ironrdp-server` 的 traits | `RdpServerDisplay` (`display.rs:288`)、`RdpServerInputHandler` (`handler.rs:73`) | 实现 DXGI 截屏和 SendInput |
| `IronRDP/crates/ironrdp-egfx` + `egfx` feature | 发送 AVC420/AVC444 帧，编码器自带 | P0 先用 RemoteFX，H.264 放到 P1 |
| `IronRDP/web-client/iron-remote-desktop*` | Svelte 写的远程桌面 Web Component + WASM | 浏览器端直接嵌入 |
| `IronRDP/crates/ironrdp-rdcleanpath` | RDCleanPath PDU：`destination`、`proxy_auth`、`server_cert_chain` 等 | Gateway 端协议 |
| `devolutions-gateway/src/rd_clean_path.rs` | 浏览器 RDP 代理的完整流程：读 PDU → 校验 token → 连接上游 → X.224 → TLS → 返回证书链 → 双向转发 | 参考实现，**上游换成 Iroh stream**；跳过凭据注入、VMConnect、KDC、录像 |
| `devolutions-gateway/src/token.rs` | provisioner 公钥签发、Gateway 校验的 JWT 模型（`provisioner_public_key`，`AssociationTokenClaims`） | 同样的授权模型 |
| `IronRDP/crates/ironrdp-web` | WASM 客户端；传输写死为 gloo-net `WebSocket`（`session.rs:300` `WebSocket::open`，`:405`/`:932` 的 `ReadHalf/WriteHalf<WebSocket>`）**[代码]** | **需要 fork**：泛化成 `AsyncRead + AsyncWrite`，再加 DataChannel 适配器；争取提 PR 回馈上游 |
| `str0m`（候选） | sans-IO 的 Rust WebRTC 实现 **[记忆]** | Agent 端 WebRTC。M0 和 `webrtc-rs` 对比后再定 |
| `iroh-relay` 1.2.0 | 自带 `iroh-relay` 可执行文件（`server` feature） | 直接部署 |

许可证：IronRDP 和 iroh-relay 都是 `MIT OR Apache-2.0` **[代码: Cargo.toml]**；Devolutions Gateway 仓库根目录有 LICENSE-MIT 和 LICENSE-APACHE。

---

## 4. 浏览器 ↔ Agent 传输：WebRTC 优先，WSS 网关兜底

### 4.1 统一的"通道"抽象
RDP、终端、文件都是**通道**：一条可靠、有序的字节流。第一帧是 `ChannelOpen{ticket, kind, params}`，之后是原始字节。
Agent 端的分发逻辑**不关心底层传输**，两种来源都会被包装成 `AsyncRead + AsyncWrite`：
- WebRTC DataChannel（首选）
- Iroh bi-stream，由 Gateway 转接浏览器的 WSS（兜底）

### 4.2 建立连接
1. 浏览器调用 `POST /api/devices/{id}/sessions` 拿到会话票据（第 5.3 节）。
2. 浏览器创建 `RTCPeerConnection`（STUN 用公共服务器或自建服务器），生成 offer，通过 `POST /api/sessions/{sid}/signal` 发给 Server。
3. Server 校验票据，通过 Iroh 控制通道把 offer 转给 Agent，再把 Agent 的 answer 和 ICE candidate 转回给浏览器（P0 用轮询或 SSE）。
4. ICE 打通后，浏览器为每个通道打开一个 DataChannel（`ordered: true`，可靠模式），第一帧发送 `ChannelOpen`。
5. **3 秒内**连不通，或者 ICE 失败，浏览器就改用 `wss://…/gw/{kind}`，走第 4.4 节的兜底路径。

### 4.3 安全
- **DTLS 指纹防篡改**：信令只经过已认证的 Server 转发。Agent 的 answer 中含有 DTLS 证书指纹，Server 用自己的密钥签名后再交给浏览器，浏览器比对 `RTCPeerConnection` 实际收到的远端指纹。
- Agent 只接受**指纹与 offer 一致**、并且首帧带有效票据的 DataChannel。
- 票据一次性使用（`jti`），每个通道都要单独验证。

### 4.4 RDP 走 DataChannel：Agent 自己扮演 RDCleanPath 网关
`ironrdp-web` 的连接流程依赖 RDCleanPath：由代理完成 TLS，再把证书链交给浏览器。直连时由 **Agent 自己充当这个代理**：
1. 浏览器在 DataChannel 上发 RDCleanPath 请求（X.224 + `proxy_auth` = 票据）。
2. Agent 内的 RDCleanPath 响应端校验票据，在本机通过命名管道连接 `irmm-session` 里的 `ironrdp-server`，完成 X.224 和 TLS，再返回证书链。
3. 之后双向转发。

兜底路径完全一样，只是这个响应端运行在 Gateway 上，上游换成 Iroh stream。**两条路径共用同一份 RDCleanPath 响应端代码**。浏览器端只需要切换传输方式。

### 4.5 不做 TURN（P0）
对称 NAT 或封锁 UDP 的网络，直接回退到 WSS 网关；Gateway 到 Agent 这一段由 Iroh relay 兜底。这样 P0 少部署一个 coturn。连通率和延迟数据收集够了，再决定是否加 TURN。

---

## 5. 认证与授权

### 5.1 身份
| 主体 | 身份 | 说明 |
|---|---|---|
| 技术员 | 账号 + 密码 + **强制 TOTP** | 属于某个 MSP 租户 |
| Agent | Iroh NodeId（Ed25519 公钥） | 注册时绑定到 设备 → 站点 → 客户 → 租户 |
| Server / Gateway | Iroh NodeId + 签名密钥 | Agent 安装时内置 Server 的 NodeId 和签名公钥 |

### 5.2 设备注册
1. 技术员在控制台为某个站点生成**注册令牌**（可设有效期和次数）。
2. 安装命令：`irmm-agent.exe install --server <url> --token <enroll-token>`。
3. Agent 生成 Iroh 密钥（存放在 DPAPI 保护的文件里），连接 Server 并提交令牌、NodeId 和设备信息。
4. Server 校验令牌后登记设备，此后只认这个 NodeId。

### 5.3 会话授权（远程桌面、终端、文件）
1. 浏览器请求 `POST /api/devices/{id}/sessions {kind: rdp|shell|files}`。
2. Server 检查 RBAC，写入审计记录，签发**会话票据**（JWT，有效期 60 秒，一次性 `jti`），内容包括：
   租户、设备 ID、技术员 ID、会话类型、权限、过期时间。
3. 浏览器建立通道（第 4 节）。RDP 通道把票据放进 RDCleanPath 的 `proxy_auth` 字段，`destination` 填设备 ID。
4. 走兜底路径时，Gateway 校验票据（签名、`jti` 只能用一次、过期时间），通过 Iroh 打开到该设备 NodeId 的 stream，**把票据转交给 Agent**。
5. Agent 始终自己再验一次：签名、`jti`、票据里的设备 ID 是否就是自己；走兜底路径时还要检查对端 NodeId 是否为 Server。

### 5.4 RDP 层安全 **[代码，待验证]**
在 RDCleanPath 流程里，由代理（Agent 内的响应端或 Gateway）和服务端完成 TLS 握手，再把证书链交给浏览器（`rd_clean_path.rs:658-680`）。因此：
- `ironrdp-server` 用 `with_tls(自签证书)`，不用 `with_hybrid`，因为我们不需要 CredSSP 和 Windows 凭据。
- 浏览器在 ClientInfo 里传占位用户名；Agent 忽略它，以第 4.3 节的票据为准。
- 需要验证：`ironrdp-web` 在服务端只支持 SSL、不支持 HYBRID 时能否顺利完成连接。
- **浏览器端不做 TLS** **[代码]**：`ironrdp-web` 收到 RDCleanPath 响应后，只从 `server_cert_chain` 的第一张证书里取出服务端公钥（供 CredSSP 使用），不进行 TLS 握手（`ironrdp-web/src/session.rs` 的 `connect_rdcleanpath`）。TLS 只存在于代理和 RDP 服务端之间，代理负责解密和转发（Devolutions Gateway 的 `rd_clean_path.rs` 也是把客户端的 WSS 流和服务端的 TLS 流直接对接转发）。
  - 兜底路径：浏览器 →（WSS）→ Gateway →（Iroh 上的 TLS）→ Agent，线路上各段都只加密一次。
  - 直连路径：浏览器 →（DataChannel/DTLS）→ Agent 内的响应端 →（本机命名管道上的 TLS）→ `ironrdp-server`。线路上只有 DTLS 一层；TLS 只在本机。以后可以优化掉本机这一跳（响应端返回一张占位证书，`ironrdp-server` 改用 `with_no_security`），P0 不做。**[待验证]**

### 5.5 多租户隔离
- 所有业务表都带 `tenant_id`，Postgres 开启 **RLS**，每个请求执行 `SET app.tenant_id`。
- 审计日志只能追加，不提供删除接口。

---

## 6. Agent 设计（Windows）

### 6.1 进程模型
- **irmm-agent**（服务，SYSTEM，session 0）：Iroh 连接、心跳、设备清单、脚本、终端、文件、自我更新、管理会话辅助进程。
- **irmm-session**（SYSTEM token，放到活动控制台会话 `winsta0`）：
  - 由服务通过 `WTSGetActiveConsoleSessionId` + `DuplicateTokenEx` + `SetTokenInformation(TokenSessionId)` + `CreateProcessAsUser` 拉起。
  - 跑 `ironrdp-server`；服务和它之间用本地命名管道转发 RDP 字节流（Iroh stream ↔ 命名管道）。
  - 监听 `WTS_SESSION_*` 事件；会话切换、用户注销时重新拉起。

### 6.2 屏幕与输入（难点）
- 截屏：DXGI Desktop Duplication。桌面切换（UAC、锁屏、登录界面）时会报 `DXGI_ERROR_ACCESS_LOST`，需要 `OpenInputDesktop` → `SetThreadDesktop` → 重建。
- 输入：`SendInput`，同样必须在当前输入桌面上执行。Ctrl+Alt+Del 用 `SendSAS`（需要相应策略）。
- 多显示器：P0 支持切换显示器，不做拼接。
- 编码：P0 用 RemoteFX（crate 自带）；P1 加 H.264（Media Foundation 硬件编码 → EGFX）。

### 6.3 自我更新与签名
- Server 发布签名过的版本清单，Agent 下载、校验签名，交给独立的 updater 进程替换并重启。
- 安装包和所有二进制文件必须 Authenticode 签名（远程控制软件不签名极易被杀软拦截 **[记忆]**）。

---

## 7. Agent ↔ Server 协议
- 一个 Iroh 连接，ALPN `irmm/1`。
- **控制流**（双向 stream #0）：长度前缀 + `postcard`/`serde` 消息：`Hello`、`Heartbeat`、`Inventory`、`RunScript`、`ScriptResult`、`RtcOffer{sid, sdp}`、`RtcAnswer{sid, sdp, fingerprint}`、`RtcCandidate`……
- **兜底数据通道**：每个通道单独开一个 bi-stream，格式和 DataChannel 相同（第 4.1 节）。
- Server → Agent 的请求通过控制流下发；数据通道由 Server/Gateway 发起。

---

## 8. 数据模型（P0）
```
tenants(id, name, plan, created_at)
users(id, tenant_id, email, pw_hash, totp_secret, role, disabled)
clients(id, tenant_id, name)
sites(id, tenant_id, client_id, name)
user_client_access(user_id, client_id)          -- 为空表示可访问全部客户
enroll_tokens(id, tenant_id, site_id, hash, expires_at, uses_left)
devices(id, tenant_id, site_id, node_id UNIQUE, hostname, os, agent_version,
        last_seen, inventory JSONB)
scripts(id, tenant_id, name, shell, body, created_by)
jobs(id, tenant_id, script_id, created_by, created_at)
job_runs(id, tenant_id, job_id, device_id, status, exit_code, stdout, stderr, finished_at)
sessions(id, tenant_id, device_id, user_id, kind, started_at, ended_at, jti UNIQUE)
audit_log(id, tenant_id, user_id, action, target, detail JSONB, at)   -- 只追加
```

---

## 9. P0 功能范围
1. 设备注册（注册令牌、安装命令）
2. 设备列表、在线状态、基础清单（OS、硬件、IP、登录用户、最后在线时间）
3. 远程桌面（查看和控制、切换显示器、文本剪贴板、Ctrl+Alt+Del、支持 UAC 和登录界面）
4. 远程终端（以 SYSTEM 身份运行 PowerShell / cmd）
5. 脚本执行（单台或批量、收集输出、脚本库）
6. 文件传输（上传和下载）
7. 多租户、RBAC、强制 TOTP、审计
8. Agent 自动更新
9. 代码签名

P1：监控和告警、策略和计划任务、补丁管理、进程/服务/事件日志工具、会话录像、用户同意弹窗、H.264。

---

## 10. 仓库结构
```
IRMM/
  Cargo.toml                 # workspace
  crates/
    irmm-proto/              # Agent ↔ Server 消息、票据类型
    irmm-cleanpath/          # RDCleanPath 响应端（Agent 和 Gateway 共用）
    irmm-server/             # API + 信令 + Gateway + Iroh endpoint（二进制）
    irmm-agent/              # Windows 服务：Iroh + WebRTC + 通道分发（二进制）
    irmm-session/            # 用户会话辅助进程：ironrdp-server + 截屏
  web/                       # SvelteKit (adapter-static)
  vendor/ironrdp-web/        # fork：可替换传输层（DataChannel / WebSocket）
  deploy/                    # docker-compose：server + postgres + iroh-relay
  docs/
```
IronRDP 先用 git 依赖固定到某个 commit：Gateway 和 Server 用的部分（egfx server）可能还没发布到 crates.io **[待验证]**。

---

## 11. 开发里程碑

| M | 内容 | 完成标准 |
|---|---|---|
| **M0a PoC 兜底路径** | `ironrdp-server`（先用 GDI/DXGI 截屏）→ Iroh stream → 最小 Gateway（`irmm-cleanpath`）→ 浏览器 `iron-remote-desktop` | 在浏览器里控制一台 Windows 桌面。验证 `Sync` 约束和只用 SSL 的握手 |
| **M0b PoC WebRTC** | fork `ironrdp-web` 的传输层；Agent 端接入 str0m（或 webrtc-rs）；手动交换信令 | 同一台桌面走 DataChannel 连上；和 M0a 对比延迟和帧率；确定 WebRTC 库 |
| **M1 骨架** | Server：租户、用户、TOTP、设备注册、心跳；Agent：服务安装和注册 | 控制台能看到设备在线和离线 |
| **M2 终端/脚本/文件** | 控制通道加数据通道；xterm.js | P0 第 4、5、6 项 |
| **M3 远程桌面产品化** | 自动信令、3 秒回退、指纹签名、票据授权、session 0 / UAC / 登录界面、多显示器、剪贴板 | P0 第 3 项；断网和封 UDP 场景下自动回退 |
| **M4 上线准备** | 自动更新、签名、RLS 审查、relay 部署、审计页面 | P0 第 7、8、9 项，可以内测 |

---

## 12. 待定问题
1. Agent 端 WebRTC 库选 str0m 还是 webrtc-rs：DataChannel 吞吐量、维护活跃度、Windows 支持情况。**[待验证，M0b]**
2. `ironrdp-web` 传输层泛化能否作为 PR 被上游接受？不被接受的话，就要长期维护 fork。
3. 不部署 TURN 时，有多大比例的连接需要回退到网关？需要在真实 MSP 环境里统计。
4. `ironrdp-web` 在服务端只支持 SSL 时能否连接成功（第 4.4 节）。**[待验证，M0]**
5. Iroh 的 `RecvStream`/`SendStream` 用 `tokio::io::join` 合并后是否满足 `Sync`。**[待验证，M0]**
6. 签名证书：用 EV 证书还是 Azure Trusted Signing？成本和时间线由你决定。
7. 对外部署形态：只提供 SaaS，还是也允许 MSP 自行部署？影响 relay 和 Gateway 的部署方式。
