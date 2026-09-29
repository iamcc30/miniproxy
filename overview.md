# MiniProxy 交付概览

## 完成内容

基于 **Rust（后端）+ React（前端）** 的代理抓包工具，五项需求全部实现并经真实流量验证：

| 需求 | 实现 |
|---|---|
| 1. 多协议捕获 | HTTP 明文代理、HTTPS TLS MITM 解密（本地 CA 动态签发证书）、WebSocket 逐帧捕获、纯 TCP 隧道记录 |
| 2. 分组/筛选/搜索 | 分组维度可切换（域名/站点/应用/协议/文件类型/状态码，**默认不分组**）、分组标题「只看此组」一键筛选；**关键词搜索覆盖 URL/域名/站点/应用 + 请求头体、响应头体（含解压后内容）、WS 消息**，正文命中标注「内容匹配」；协议、类型、方法、状态码、域名、站点、应用全部多选（同组或、跨组且），chips 可移除 |
| 3. 请求/响应完整记录 | 请求行/头/体 + 响应状态/头/体全部捕获并可视化展示 |
| 4. 解包能力 | gzip / deflate(zlib+raw) / brotli / zstd 自动解压并标记；chunked 自动还原；WS 文本帧还原（含续帧合并、去掩码） |
| 5. 可视化与历史 | SSE 实时推送、暂停/恢复、主题切换（浅色/深色/系统）、JSON + HAR 1.2 导出 |
| 6. 一键系统代理 | 界面一键开启/关闭 macOS 系统代理（默认指向 127.0.0.1:34567）；开启前备份原有代理配置、关闭时自动恢复 |

## 文件结构

- `backend/` — Rust 后端（hyper + rustls + rcgen）；`cargo run` 同时启动 **web 服务**（界面 + API，`127.0.0.1:9000`）与 **代理服务**（抓包口 `0.0.0.0:34567`）。两者只是"开始监听"，**不会改动系统代理**
- `frontend/` — React + Vite + TS，`npm run build` 后由后端托管
- `README.md` — 使用说明（代理配置、CA 信任、环境变量）

## 关键修复（调试过程中发现）

1. TLS 首字节探测后必须回吐，否则 ClientHello 损坏
2. WS 握手转发需保留 `Connection/Upgrade` 头
3. 先取源站 101 头再经 hyper 返回，避免客户端收到两个 101
4. WS 客户端帧 XOR 去掩码后再展示

## 验证结果

明文 HTTP 200 ✓、HTTPS MITM 200 ✓、gzip/brotli 解压 ✓、POST 请求体捕获 ✓、wss 双向消息+close code ✓、TCP 隧道 ✓、HAR/JSON 导出 ✓、SSE 实时更新 ✓

## 最近更新（2026-09-15）

- **默认不分组**：列表默认平铺，「分组」下拉按需切换
- **正文搜索**：关键词深入请求/响应头体（含解压后内容）与 WS 消息；按条目缓存检索索引（查询 <1ms），单侧正文上限 128KB、二进制自动跳过；搜索框 250ms 防抖 + 清空按钮；正文命中显示「内容匹配」徽标

## 最近更新（2026-09-24）：定位「开了代理后 Google 很慢」

排查结论：慢的主因在上游节点（对外部代理不通 Google 系域名），但本项目有多处把「节点慢」放大成「整页卡死」的问题，已一并修掉。

### 性能与稳定性修复

1. **出站全部加超时**（`dial.rs`）——连上游 / 等 CONNECT 响应 / 与源站 TLS 握手以前都是裸 await，上游丢包时无限等待，客户端只能干等。现在默认 10s（连接 5s）快速失败，错误可在界面看到；`MINIPROXY_DIAL_TIMEOUT_MS` 可调。实测：上游吞包时 **10.05s 返回 502 并说明原因**（此前会一直挂到系统 TCP 超时）。
2. **缓存出站根证书与 ClientConfig**（`dial.rs`）——以前每个新出站连接都 `load_native_certs()`（macOS 要读钥匙串）+ 重建 `RootCertStore`（逐张校验 161 张系统证书），改为 `OnceLock` 进程内只做一次。
3. **支持 HTTP/2**（`ca.rs` / `dial.rs`）——MITM 服务端 ALPN 改为 `h2,http/1.1`（hyper 的 `ConnectionMode::Fallback` 自动识别 h2 前言，服务端无需其它改动），出站 rustls 也协商 h2 并在 `OriginStream::connected()` 里回报 `negotiated_h2()`。WebSocket 用的裸 TLS 仍固定 http/1.1（手写升级报文不能跑在 h2 上）。`MINIPROXY_NO_H2=1` 可退回纯 H1。
4. **修 `durationMs` 恒为 0**（`capture.rs`）——`started_at` 建条目后再没更新过；新增 `finished_at` 与统一的 `finish()` 收口所有「完成」标记，详情接口、HAR 的 `time`、列表摘要都用真实耗时。前端列表新增「耗时」列（≥0.8s 黄、≥3s 红），详情面板也显示。

### Bug 修复：MITM 证书链对 OpenSSL 系客户端不可用

`ca.rs` 的 `build_ca_params` 用 `CertificateParams::new(vec!["MiniProxy Root CA"])`，rcgen 会把该字符串当成 **DNS SAN**，生成出含空格的非法名字 `DNS:MiniProxy Root CA`。结果：

```
openssl verify -CAfile ~/.miniproxy/ca.crt leaf.pem
→ error 53 at 1 depth lookup: unsupported or invalid name syntax
```

即 **curl / Node / Go / Python requests / git 等一律拒绝 MITM 证书**（Chrome 不检查 CA 的 SAN 所以照常），并且每次握手失败都累加直通计数，满 3 次该域名被永久自动直通。修复：改用 `CertificateParams::default()` 只设 DN；启动时用 base64 解 PEM、扫 SAN OID (`06 03 55 1D 11`) 识别旧证书，备份为 `*.legacy-san.bak` 后自动重建（备份失败则中止重建，绝不静默覆盖）；备份与重建逻辑已在隔离环境验证。

**升级注意**：CA 重建后必须在钥匙串里重新信任新的 `~/.miniproxy/ca.crt`，否则浏览器解密会报证书错误。

### 本次验证

明文 HTTP ✓ / HTTPS MITM（curl 用本地 CA 校验通过）✓ / ALPN 协商 h2 ✓ / WebSocket 101+帧记录 ✓ / 上游吞包 10s 快速失败 ✓ / durationMs 有真实数值 ✓ / CA 旧证书自动迁移（隔离测试）✓

## 「上游请求失败: error trying to connect: tls handshake eof」

**原因**：出站用 rustls，只实现 AEAD 套件（GCM / ChaCha20），**不含 CBC 系列**。少数老旧站点只提供
CBC 套件（实测 `www.bootstrapmb.com` 只给 `ECDHE-RSA-AES256-SHA384`，且无 TLS 1.3；三个 GCM/ChaCha20
套件全部协商失败 → `Cipher is (NONE)`），对端直接关连接 → `tls handshake eof` → MiniProxy 返回 502。
直连和只经上游 7890 都正常（curl 用 OpenSSL，支持 CBC），**只有 MiniProxy 这一跳失败**，故不是节点问题。

**处置**（2026-09-28）：出站握手失败也纳入自动直通（阈值 `OUTBOUND_BYPASS_THRESHOLD = 2`，
TTL `OUTBOUND_BYPASS_TTL_MS = 30min`，过期重试 MITM 以免上游偶发掉线造成永久误判）。
命中后该域名以**隧道方式**照常访问（不解密），502 文案会附上说明。`GET /api/bypass` 返回
`{host,count,reason: client|outbound}`，界面工具栏「🛡 自动直通」可查看与清空。
彻底解密这类站点需要换用支持 CBC 的 TLS 后端（`native-tls`/`openssl`），但本地 registry 未缓存
这些 crate、离线装不了，留待联网后再评估。

## 分流规则（2026-09-28 新增）

界面「🚦 分流规则」两个列表，持久化到 `~/.miniproxy/config.json` 的 `direct` / `proxied` /
`direct_no_mitm` 字段，重启沿用：

- **跳过代理（`direct`）**：命中者 `upstream_for()` 返回 None → 直连源站；默认同时跳过 MITM
  （`MitmDecision::BypassDirect`，隧道条目 Mode 标注「直连（分流规则：跳过代理）」）。
- **强制走代理（`proxied`）**：优先级最高，覆盖 `direct` 与内置直连段。
- 匹配：域名（含子域）、`*` glob、IP、IPv4 通配、CIDR（v4/v6）；内置直连 = 回环 / `*.local` /
  私网段 / link-local / CGNAT。
- 生效点：`dial::ProxyConnector::call()`（HTTP 客户端每次拨号读规则）、`proxy.rs` 的 TCP 隧道与
  WS 路径（改用 `app.upstream_for(host)`）、api.rs 的 SABR 补拉。
- **系统代理 bypass 同步**：开启系统代理或保存规则时，把 `direct` 条目追加进
  `networksetup -setproxybypassdomains`（保留用户原有条目，基准是开启时的备份，避免重复累积）；
  关闭系统代理时按备份原样恢复。自带网络栈的程序不读系统代理，但仍受 MiniProxy 内部分流控制。
- API：`GET/POST /api/rules`。
- 已知限制：已建立的连接（含 hyper 连接池空闲连接）不受规则变更影响，与上游切换行为一致。

## 使用注意

- HTTPS 解密需信任 CA：`~/.miniproxy/ca.crt`（界面右上角可下载）
- 默认端口：代理 34567、界面 9000（可用 `MINIPROXY_PORT` / `MINIPROXY_API_PORT` 调整）
- 术语：「**启动服务**」= 启动 web 服务（:9000）+ 代理服务（:34567），随进程启动自动完成；
  「**开启系统代理**」= 把系统流量指向 :34567，只在点击界面右上角按钮时发生（`POST /api/system-proxy/enable`
  是全代码唯一的开启入口）。**启动服务不会开启系统代理**；反向只有一种情况：启动自检发现上次
  残留的备份且系统代理仍指向已停端口时，会按备份**恢复原值**（只会关，不会开）。
- 已知限制：正文单条最多存 4MB（搜索索引单侧 128KB）；permessage-deflate WS 压缩帧不解压；应用归因仅对本机发起的连接有效
- 新环境变量：`MINIPROXY_DIAL_TIMEOUT_MS`（出站超时，默认 10000）、`MINIPROXY_NO_H2=1`（关闭 h2）
