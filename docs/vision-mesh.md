# IRMM 愿景：一个 Agent = RMM + Mesh VPN + DNS + 反向代理（草案 v0.1）

2026-10-18 设计讨论（grilling）的结论。本文只记录**已达成的决定**和理由，实现细节留给各阶段的实施计划。

> 标注约定：**[代码]** 读源码得出、未运行；**[记忆]** 未在本仓库核实；**[待验证]** 必须在 PoC 中确认。

---

## 1. 一句话

IT 在每台机器上**只装一个 agent**。它同时是 RMM 端点、mesh VPN 客户端、DNS 客户端和内部应用的 HTTPS 代理。员工打开 `https://erp.clinic-a.int.msp.com` 就能用，不管人在办公室还是在家。

今天要实现同样的效果，MSP 需要 RMM + Tailscale/NetBird + Cloudflare Access/Tunnel + Umbrella/DNSFilter：四个 agent，四个控制台，四套身份模型。

## 2. 用户

- **MSP 客户公司的员工**：使用者。MSP 付费并负责管理。
- **企业自管 IT**：一个 tenant 下只有一个 client，复用同一套模型。
- MSP 技术员：继续使用现有的 RMM 功能（远程桌面、终端、脚本）。

---

## 3. 身份与访问控制

### 3.1 两个维度

```
User（来自 SSO）  ──<  DeviceSession  >──  Device（RMM 注册）
Device ── tags：自动（tenant / client / site）+ 自定义（finance-laptop …）
Device ── assigned_user（可选，IT 指定，适用于 1 人 1 机）
```

和 Tailscale 不同：**tag 和用户可以同时存在，不互斥**。所有设备都由 IT 注册，"受管设备"这个信号是天然得到的。

### 3.2 分层

| 层 | 依据 | 说明 |
|---|---|---|
| L3 mesh | 设备 tag + `assigned_user` | 共享设备上不区分具体是哪个人 |
| L7 HTTPS 代理 | **每个请求**的用户 SSO | 共享电脑上 Bob 和 Carol 也能区分 |

- 服务一旦标记为"需要用户认证"，L3 上**任何设备都不能直连它的后端**，只有代理能访问。
- 不做按连接的用户识别（WFP / eBPF），代价太高。

### 3.3 员工身份来源

- 默认：每个 client 对接自己的 **Entra ID / Google Workspace**（OIDC）。组信息在登录时读取，SCIM 以后再做。
- 兜底：没有 IdP 的小客户用 IRMM 本地账号。
- 服务器作为 OIDC 客户端，签发带用户和组信息的短期签名 token；agent 上的代理只需验签，复用现有的签名 ticket 模型。

### 3.4 ACL

- 同一个 client 内**默认全通**（和今天的办公室 LAN 一样），控制台提供"收紧"模板。
- **GUI 和代码同等重要**：底层是同一份结构化数据，两边无损互转（注释作为规则的描述字段保存）。
- 支持 **tenant 级模板**，一套策略应用到多个 client。
- 每次变更都写入 append-only 审计日志。

---

## 4. 网络

### 4.1 数据平面

- **TUN + IP over iroh QUIC**，不用 WireGuard。直接做 TUN，不先经过"TCP 转发"阶段。
  - 理由：一套网络栈；iroh 已经提供 NAT 穿透和 relay；不用再造一套打洞和中继。
  - TUN：Windows 用 wintun，macOS 用 utun（root 守护进程，不用 Network Extension **[待验证]**），Linux 用 tun。
- 现状 **[代码]**：`iroh = "1.3"`（`Cargo.toml:26`）。agent 只连服务器（`crates/irmm-agent/src/lib.rs:199`）；没配 relay 时 relay 是关闭的（`lib.rs:110-112`）。mesh 需要新增 agent 之间的连接，并且 relay 会变成**必须**。
- 抗防火墙靠的是 iroh relay（HTTPS/TCP 443）**[记忆]**，不是 QUIC 本身：很多企业防火墙会封 UDP 443。严格网络下流量走 relay，所以 relay 的带宽很关键。

### 4.2 网络边界与地址

- **每个 client 一张网**。客户之间在网络层没有路径，不靠 ACL 来隔离。
- 双栈：IPv4 从 `100.64.0.0/10` 分配子段（可按 client 修改，检测到冲突时报警）；IPv6 用每个 client 随机生成的 ULA `/48`。
- 地址可以在不同 client 之间重复使用。

### 4.3 路径

- **永远走 mesh**：DNS 永远返回 mesh IP。在办公室时 iroh 会走局域网直连。行为一致、ACL 永远生效、好调试。

### 4.4 没有 agent 的设备

- 第一步：**按设备发布（1:1）**。connector（同 LAN 里的一台 agent）为设备分配 mesh IP 和名字，做 1:1 NAT。
- 第二步：**子网路由**，保留原 IP，照顾写死 IP 的老软件和映射盘。和其他站点网段冲突时**直接拒绝并报错**，不做 4via6。
- mDNS / WSD 自动发现跨 mesh 不生效，打印机需要手动添加。

### 4.5 出口

- 第一版只做 split tunnel。
- 第二阶段做**按目标路由**的出口（解决银行、医保系统的 IP 白名单问题），在子网路由之后实现；全隧道是它的特例。

---

## 5. 证书

- 默认用**按 client 签发的私有 CA**（AD + ADCS + GPO 模型）。理由：好理解、好调试、离线可用、不依赖外部注册商或 CA。
  - 根证书带 **X.509 Name Constraints**，只能签这个 client 的内部后缀 **[待验证：各浏览器和系统对本地根证书的行为]**。
  - agent 负责把它装进系统信任库、Firefox（企业策略）和各个 Java `cacerts`；Python、Node、容器属于长尾，可以接受。
- **命名空间**：默认 `svc.<client>.int.<msp-domain>`，client 可以改成自己的域名（`svc.int.clinic.com`）。内部名字永远挂在真实域名下面。
- air-gapped 环境同样适用（私有 CA 本身就不依赖外网）。

## 6. DNS

两种角色，不要混淆：

| 角色 | 位置 | 职责 |
|---|---|---|
| 内部名字 + 过滤 + 日志 | **agent 本地的 DNS stub** | 解析内部名字到 mesh IP，执行封禁策略，记录日志，其余查询转发上游 |
| 可选上游 | 系统默认 / Quad9 / Cloudflare / IRMM 服务器的解析器 | 可配置 |

- 执行点在设备上，漫游时同样生效，服务器宕机也不影响上网。
- 防绕过：用浏览器策略关闭 DoH，防火墙封锁发往其他解析器的 53/853 流量，屏蔽已知 DoH 端点。做不到 100%。
- **日志**：默认只记录被拦截的查询和聚合统计；MSP 可以按 client 开启全量记录并设定保留期限。全量日志不写进 Postgres 主库（量级太大）。GDPR 以后再处理。

## 7. L7 代理与外网发布

```
内网： 员工浏览器 ─mesh(P2P)─► 目标机 agent [TLS 终止 + SSO] ─► localhost:8080
外网： 浏览器 ─► 公网 DNS ─► IRMM 服务器（按 SNI 透传，不解密）─iroh─► 同一个 agent 代理
```

- 代理运行在**目标机的 agent** 里，内网和外网走同一个执行点、同一套逻辑。
- 外网发布单独配置公网名字；公网证书由 agent 通过 ACME **TLS-ALPN-01** 申请，客户只需加一条指向服务器的 A 记录。
- 外网发布**默认必须经过 SSO**；可以对单个服务关闭（比如对外网站），此时控制台给出醒目警告并写入审计。匿名流量会经过服务器，有 DDoS 风险。

---

## 8. 客户端体验

- 托盘程序只做最小功能：连接状态（直连 / relay / 断开）、设备名或 ID、错误码，方便员工截图发给 IT。
- 暂不做手机客户端；手机通过外网发布 + SSO 访问。只有 VPN 功能的手机 App 等有真实需求时再做。
- 技术员先用现有的 RMM 功能；以后再做**限时、可审计地加入客户网络**（复用一次性 ticket），永远不做"永久加入所有客户网络"。

## 9. 安全模型

- 服务器视为**完全可信**（和主流 RMM 相同）：它本来就能以 SYSTEM 身份执行命令，mesh 没有显著扩大它的权限。
- 不做 Tailnet Lock 式的"只保护网络层"。
- 长期方向：**端到端授权**。高危操作（终端、脚本、ACL）必须由技术员自己的密钥签名，agent 校验签名。这影响整个 RMM，作为独立项目讨论。
- 已有的保护措施不变：二进制更新使用离线 ed25519 签名。

---

## 10. 阶段

| 阶段 | 内容 |
|---|---|
| **1. mesh 核心** | agent 之间的 iroh 连接、节点列表下发、TUN + IP over iroh（三平台）、双栈地址、默认全通 ACL、DNS stub 解析内部名字、最小托盘程序 |
| 2. 内部应用 | 私有 CA + 信任库安装、agent L7 代理、OIDC（Entra / Google / 本地账号）、服务发布 |
| 3. 管理能力 | DNS 过滤和日志（只依赖 DNS stub，可以和阶段 1 后半段并行）、connector 1:1 发布、ACL GUI + 代码 + 模板 |
| 4. 扩展 | 外网发布、子网路由、技术员限时加入、按目标路由的出口 |
| 以后 | 手机 App、端到端授权 |

**阶段 1 验收**（网络层 e2e）：两个 agent 互相 ping 通 mesh IP、建立 TCP 连接、通过内部名字解析对方，并且在直连和强制 relay 两种情况下都通过。
