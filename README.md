# MiniProxy 抓包代理工具

基于 **Rust（后端）+ React（前端）** 的 HTTP/HTTPS/WebSocket/TCP 抓包调试代理，支持 MITM 解密、内容解压、按域名分组筛选、实时可视化与 JSON/HAR 导出。

## 功能特性

| 功能 | 说明 |
|---|---|
| 多协议捕获 | HTTP 明文代理、HTTPS（TLS MITM 中间人解密）、WebSocket（逐帧捕获消息）、纯 TCP 隧道（流量统计 + 首包 hex 预览） |
| 分组 / 筛选 / 搜索 | **分组方式可切换**（默认不分组）：域名 / 站点（主域名归并）/ 应用（客户端进程）/ 协议 / 文件类型 / 状态码，分组标题支持「只看此组」一键筛选；关键词搜索 URL/域名/站点/应用，并深入**请求头/请求体、响应头/响应体（含解压后内容）、WebSocket 消息文本**，正文命中的记录会标注「内容匹配」；协议、文件类型、方法、状态码、域名、**站点、应用**均支持多选（同组或、跨组且），已选条件以可移除 chips 呈现，支持一键清空 |
| 请求/响应完整记录 | 请求行、请求头、请求体、响应状态、响应头、响应体全部记录并展示 |
| 解包能力 | 自动解压 gzip / deflate(zlib+raw) / brotli / zstd；chunked 分片由 HTTP 层自动还原；WebSocket 文本帧还原（支持续帧合并）；**二进制 / 压缩正文自动识别为乱码并提供原始数据视图（文本 / 十六进制 / Base64 + 下载原始字节）** |
| 可视化界面 | SSE 实时推送、暂停/恢复、明暗主题切换（浅色/深色/跟随系统）、历史记录查看与 JSON/HAR 导出 |
| 一键系统代理 | 界面右上角一键开启/关闭 macOS 系统代理（HTTP+HTTPS+SOCKS）；开启前自动备份原有代理配置，关闭时恢复，不破坏 Clash 等已有配置；**程序退出时（含 Ctrl+C、强杀、崩溃）自动恢复系统代理**，不会留下指向死端口的代理导致断网 |
| 上游级联 | 出站流量经本机其他代理（Clash/Charles/Surge…）转发，HTTPS 仍被解密抓包。**三种配置方式**：界面「🔗 上游级联」一键检测/手填（改完立即生效）、环境变量 `MINIPROXY_UPSTREAM_PROXY`、从未配置时**启动自动探测**本机常见代理端口；设置持久化到 `~/.miniproxy/config.json`，启用前自动做连通性测试 |
| 分流规则 | 界面「🚦 分流规则」配置哪些域名/IP **跳过代理直连源站**、哪些**强制走上游**；支持通配符、IP 通配与 CIDR 网段，内置私网/回环直连段。持久化到 `~/.miniproxy/config.json`，并在系统代理开启时同步进 macOS bypass 列表（浏览器等直接绕过 MiniProxy） |
| 耗时可见 | 列表与详情面板显示每条请求的耗时（从收到请求到响应体读完），≥0.8s 标黄、≥3s 标红，配合 HAR 导出的 `time` 字段一起可用于定位慢请求 |
| HTTP/2 | 客户端侧与出站侧都协商 h2，同一域名可多路复用，避免为每个请求重复握手；如需退回纯 HTTP/1.1 用 `MINIPROXY_NO_H2=1` |
| 出站超时 | 连上游、等上游 CONNECT 响应、与源站做 TLS 握手都有超时（默认 10s / 连接 5s），上游节点丢包时快速失败并在界面标出原因，而不是无限等待把浏览器拖死 |

## 项目结构

```
miniproxy/
├── backend/            # Rust 后端 (hyper + rustls + rcgen)
│   └── src/
│       ├── main.rs     # 入口：代理端口 + API 端口
│       ├── ca.rs       # 本地 CA 生成/持久化 + 按域名动态签发证书
│       ├── capture.rs  # 抓包模型/存储/解码/转发捕获
│       ├── proxy.rs    # CONNECT 隧道 / TLS MITM / WebSocket 升级
│       ├── ws.rs       # WebSocket 帧解析（RFC 6455）
│       ├── tcp.rs      # TCP 隧道记录
│       ├── api.rs      # REST/SSE/导出/静态服务
│       ├── peek.rs     # 首字节探测流包装
│       ├── config.rs   # 持久化配置 ~/.miniproxy/config.json（上游级联 + 分流规则）
│       ├── rules.rs    # 出站分流规则：跳过代理/强制走代理（通配符 + CIDR 匹配）
│       ├── dial.rs     # 出站拨号：直连 / 上游 CONNECT 级联 / 本机代理自动探测
│       └── util.rs     # 通用工具
├── frontend/           # React + Vite + TypeScript
│   └── src/            # App.tsx（列表/过滤/详情）、api.ts、theme.ts、styles.css
└── packaging/          # 打包成 macOS .app
    ├── package-macos.sh
    ├── shell/AppShell.m  # 原生外壳：NSWindow + WKWebView，双击即出窗口
    ├── make-icon.py    # 生成 AppIcon.png（纯 Pillow 绘制）
    └── AppIcon.png
```

## 打包成 macOS 应用

```bash
packaging/package-macos.sh            # 产物：dist/MiniProxy.app（约 107MB）
packaging/package-macos.sh --no-ffmpeg   # 不内置 ffmpeg（省 100MB，音视频合并回退为分开下载）
```

脚本会：构建前端 → `cargo build --release` → 组装 Bundle → 编译原生外壳 → 生成图标 → ad-hoc 签名。
装进应用程序目录后**双击图标直接出窗口**（原生 NSWindow + WKWebView，不经过浏览器；
重复双击只会把已有窗口带到前台）：

```bash
cp -R dist/MiniProxy.app /Applications/
open /Applications/MiniProxy.app
```

Bundle 结构与要点：

| 内容 | 说明 |
|---|---|
| `Contents/MacOS/MiniProxy` | 原生外壳（`packaging/shell/AppShell.m`，系统 clang 编译，零额外依赖）：拉起/收养后端、窗口里加载界面、Cmd+Q 优雅退出 |
| `Contents/MacOS/miniproxy-bin` | release 二进制 |
| `Contents/Resources/static/` | 前端产物。`static_dir()` 会**按可执行文件相对路径**找它（`api.rs`），Finder 双击时工作目录是 `/`，CWD 相对路径全部失效 |
| `Contents/Resources/bin/ffmpeg|ffprobe` | arm64 静态构建（osxexperts.net），`find_ffmpeg()` 优先用它，其次 Homebrew/PATH |
| `Contents/Resources/AppIcon.icns` | 由 `packaging/make-icon.py` 生成 |

外壳行为：

- 启动时若 9000 已有 MiniProxy（比如上次强杀剩下的无头后端），直接**收养**并显示它的界面，不再起子进程；
- **退出**：Cmd+Q、关闭窗口、Dock 退出、`kill` 外壳，都会先 `POST /api/quit` 让后端优雅停机
  （恢复系统代理、收尾在途请求），等子进程退出后外壳才退；后端先没了（界面里点「⏻ 退出」）外壳也跟着退，不留僵尸窗口；
- 菜单栏有「在浏览器打开界面 ⌘B」，需要大屏调试时可再开一份网页版。

注意：

- **签名/分发**：默认 ad-hoc（`MINIPROXY_SIGN_ID=-`）本地自用；要发给别人需
  `MINIPROXY_SIGN_ID="Developer ID Application: …" codesign …` + notarytool 公证，否则对方会被 Gatekeeper 拦。
- **CA 信任仍要手动**（App 化解决不了提权问题）：
  `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ~/.miniproxy/ca.crt`
- 静态文件服务会拒绝带 `..` 的路径段（`GET /../../../etc/passwd` → 404）。
- 卸载：`rm -rf /Applications/MiniProxy.app`；配置与 CA 在 `~/.miniproxy/`，想清干净一并删除。

## 快速开始

### 1. 启动 MiniProxy（web 服务 + 代理服务）

```bash
cd backend
cargo run            # 默认代理端口 34567，界面端口 9000
```

这一步只让**两个服务开始监听**：web 服务（界面 + API，`127.0.0.1:9000`）与代理服务
（抓包口 `0.0.0.0:34567`）。它**不会改动系统代理**——把系统流量交给 MiniProxy 是另一个
动作，需要你手动触发，见「4. 开始抓包」里的「开启系统代理」。

可用环境变量：
- `MINIPROXY_PORT`（默认 34567）：代理监听端口
- `MINIPROXY_API_PORT`（默认 9000）：界面/API 端口
- `MINIPROXY_STATIC`：前端静态文件目录
- `MINIPROXY_API_HOST`：界面/API 监听地址（默认 `127.0.0.1`；手机抓包设为 `0.0.0.0` 以便设备下载 CA 证书）
- `MINIPROXY_UPSTREAM_PROXY`：上游级联代理（如 `http://127.0.0.1:7890`；优先级最高，会覆盖界面设置）
- `MINIPROXY_DIAL_TIMEOUT_MS`（默认 10000）：出站「等待上游 CONNECT 响应」与「与源站 TLS 握手」的超时；连不上上游时 5s 内即失败
- `MINIPROXY_NO_H2=1`：关闭 MITM 的 HTTP/2 协商，退回纯 HTTP/1.1（排查兼容性问题时用）

其中「上游级联」无需改环境变量：界面右上角「🔗 上游级联」可随时开关/更换，运行期立即生效。

### 上游级联（抓包 + 科学上网）

默认情况下 MiniProxy **直连**目标网站。如果目标站点被墙（如 chatgpt.com），
直连会在 TLS 握手阶段被中断，界面显示 `tls handshake eof` 错误。
此时可开启「上游级联」，让出站流量经本机其他代理（Clash 等）转发：

1. **界面开启（推荐）**：打开 http://127.0.0.1:9000，点击右上角「🔗 上游级联」
   → 「🔍 自动检测本机代理」，会并发探测本机常见代理端口
   （7890 / 7897 / 7891 / 9090 / 8888 / 10809 / 6152 / 1087 / 2080 / 2081 / 3128 / 8889 / 8080 / 20171，
   覆盖 Clash / Clash Verge / Charles / Surge / v2ray 等），命中一个 CONNECT 探针通过才算可用；
   点候选地址即可启用，也可手动填 `127.0.0.1:7890`。
2. **启动时就生效**：环境变量

```bash
MINIPROXY_UPSTREAM_PROXY=http://127.0.0.1:7890 cargo run
```

3. **完全不用管**：从未配置过上游时，MiniProxy 会在启动后**后台自动探测**本机代理，
   探测到就直接启用（终端会打印「自动检测到本机代理 … 并已启用」），不阻塞启动、不需要重启。

设置会持久化到 `~/.miniproxy/config.json`，下次启动沿用；一旦在界面里显式关闭，
就不再自动探测，尊重你的选择。取值优先级：**环境变量 > 界面保存的配置 > 自动探测**。

界面里修改**立即对新连接生效**（无需重启），设置前会先做连通性测试（TCP 握手 + CONNECT 探针），
地址不通会被拒绝并给出原因，不会让你带着坏配置继续抓包。

开启后：
- 到所有源站的连接（HTTP/HTTPS/WS/TCP）先经上游代理（HTTP CONNECT 隧道）出站
- HTTPS 依然被 MiniProxy 解密抓包（级联只影响出站路径，不影响 MITM）
- 一键系统代理 + 上游级联组合：浏览器正常访问被墙站点，同时流量全部被抓包记录

### 分流规则（哪些域名/IP 跳过代理、哪些走代理）

界面右上角「🚦 分流规则」，两个列表每行一条，保存后**持久化到 `~/.miniproxy/config.json`**（重启沿用），
改完立即对新连接生效：

| 列表 | 作用 |
|---|---|
| **跳过代理（直连源站）** | 命中者不经上游级联，直接连源站；默认同时**不解密**（记录为隧道，Mode 标注「直连（分流规则：跳过代理）」），适内网/自签证书站点 |
| **强制走代理** | 命中者强制经上游转发，**优先级最高**（覆盖「跳过代理」与内置直连段） |

条目写法（大小写不敏感）：`example.com`（本域及子域）、`*.foo.com`、`api-*.foo.com`（`*` 通配）、
`1.2.3.4`、`192.168.1.*`、`10.0.0.0/8`（CIDR，IPv4/IPv6 均可）。每个列表上限 500 条。

**内置直连**（无需配置，可用「强制走代理」覆盖）：`localhost`、`*.local`、`127.0.0.0/8`、`::1`、
`fe80::/10`、`fc00::/7`、`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`、`169.254.0.0/16`、`100.64.0.0/10`。

「跳过代理」的条目还会**同步进 macOS 系统代理的 bypass 列表**（`networksetup -setproxybypassdomains`），
让浏览器 / curl / Electron 这类走系统网络栈的客户端**真的不把请求发给 MiniProxy**；
同步时保留你原有的 bypass 条目，关闭系统代理时按原样恢复。自带网络栈的程序（Codex、Go/Rust 程序等）
不读系统代理，但它们在规则命中时同样会被 MiniProxy 直连转发。

API：`GET /api/rules`、`POST /api/rules`（`{"direct":[...],"proxied":[...],"directNoMitm":true}`）。
注意：已建立的连接（含 hyper 连接池里的空闲连接）不受影响，与上游切换行为一致。

### MITM 白名单（TLS 直通域名）

部分客户端会拒绝 MITM 证书（系统级证书固定、不信任用户 CA 等），例如
macOS 的 iCloud 服务（`gateway.icloud.com` 等）。这类连接无法解密，
MiniProxy 默认对以下域名后缀**不做 MITM、原样直通**（保证客户端正常工作，
流量以 TCP 隧道形式记录）：

```
icloud.com / icloud.com.cn / icloud-content.com / apple.com / mzstatic.com
```

可用环境变量追加：

```bash
MINIPROXY_NO_MITM=pinning.example.com,other.example.org cargo run
```

如需恢复对某域名解密抓包（需客户端信任 MiniProxy CA），可修改
`backend/src/proxy.rs` 中的 `DEFAULT_BYPASS` 列表。

**自动直通（证书固定 App）**：静态白名单之外，若某域名 TLS 握手失败累计达到 3 次
（`AUTO_BYPASS_THRESHOLD`），MiniProxy 会自动对该域名停止 MITM、改为直通隧道，
客户端随即恢复正常联网（记录里 Mode 标注「TLS 直通（客户端证书固定，自动跳过）」）。
任意一次握手成功会清零计数，避免偶发中断被误判。无需配置，进程内生效。

**自动直通（源站只支持旧式加密套件）**：MiniProxy 出站用 rustls，只实现 AEAD 套件
（GCM / ChaCha20），**没有 CBC 系列**。少数老旧站点（如 `www.bootstrapmb.com`，只提供
`ECDHE-RSA-AES256-SHA384`）因此根本协商不上，表现为 502 且错误为
`error trying to connect: tls handshake eof`。这类域名累计 2 次
（`OUTBOUND_BYPASS_THRESHOLD`）后同样自动改为**隧道直通**：站点恢复正常访问，
但该域名无法解密抓包。直通带 30 分钟有效期（`OUTBOUND_BYPASS_TTL_MS`），
过期后重试 MITM，上游偶发掉线导致的误判会自愈。想立刻恢复解密可在界面点
工具栏「🛡 自动直通 → 清空名单」（对应 `POST /api/bypass/clear`，
`GET /api/bypass` 查看当前名单与原因）。

### 分组与「按组筛选」（默认不分组）

工具栏右侧的「分组」下拉可切换列表的分组维度：

| 维度 | 归组规则 | 「只看此组」写入的筛选字段 |
|---|---|---|
| 按域名 | 每个目标主机一组 | 域名 |
| 按站点 | 多级子域归并到主域名（`api.example.com`、`cdn.example.com` → `example.com`，内置常见双段后缀如 `com.cn`/`co.uk`） | 站点 |
| 按应用 | 发起请求的**客户端进程**（Chrome / curl / 微信…，通过 `lsof` 按客户端端口反查） | 应用 |
| 按协议 | HTTP/HTTPS、WebSocket、TCP | 协议 |
| 按文件类型 | 按 Content-Type 与 URL 扩展名归类 | 类型 |
| 按状态码 | 2xx / 3xx / 4xx / 5xx / 失败 / 进行中 | 状态 |

- 悬停/查看任意分组标题，右侧有「**只看此组**」按钮，点击即把该分组加入对应筛选维度（再点取消）；
- 「全部站点」「全部应用」两个下拉与域名下拉一样支持多选 + 搜索 + 实时计数；
- 各维度候选值与计数来自 `GET /api/facets`，每 5 秒刷新。

进程归因说明：仅对本机发起的连接有效（`lsof -iTCP:<客户端端口>`），每个新连接约 60ms
（已按端口缓存）。若不需要，可用 `MINIPROXY_NO_APP=1` 关闭；远端机器通过本代理转发
的流量无法识别其原始进程。

### 关键词搜索（含正文）

搜索框匹配范围：**URL、域名、站点、应用、方法、状态码、请求头与请求体、响应头与响应体（优先使用解压后内容）、WebSocket 消息文本、错误信息**，大小写不敏感。

- 正文命中的记录在列表中标注「**内容匹配**」（URL 未命中、仅正文命中时显示），一眼看出这条为什么被搜出来；
- 关键词在服务端匹配（`GET /api/entries?q=...`），前端 250ms 防抖；搜索期间新到记录若 URL 未命中，会防抖重查由服务端确认正文；
- 每条记录的检索索引按需构建并缓存（内容变化自动重建），单侧正文参与搜索的上限 128KB，非 UTF-8 二进制（图片/字体等）自动跳过。

### 2. 构建前端（首次）

```bash
cd frontend
npm install && npm run build
```

构建产物默认从 `./static` 或 `./frontend/dist` 提供。

### 3. 信任 CA 证书（解密 HTTPS 必需）

首次启动后，CA 证书位于 `~/.miniproxy/ca.crt`，也可在界面右上角点击「CA 证书」下载。

macOS 安装并信任：

```bash
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain ~/.miniproxy/ca.crt
```

浏览器（Chrome/Safari）跟随系统钥匙串即可。**不安装 CA 时 HTTPS 流量将以 TCP 隧道形式记录（无法解密内容）。**

> **如果启动日志提示「检测到旧证书带有非法的 SubjectAlternativeName 扩展，已重建」**：旧版本的 CA 误把 CN 写成了 DNS SAN（`DNS:MiniProxy Root CA`，含空格、非法），
> 导致 OpenSSL 系客户端（curl / Node / Go / Python requests / git）以 `unsupported or invalid name syntax` 拒绝整条链。
> 新版本会自动备份旧文件（`ca.crt.legacy-san.bak` / `ca.key.legacy-san.bak`）并重建 CA，**需要重新执行上面的信任命令**（旧证书在钥匙串里的信任项对它无效）。
> 验证是否修好：`openssl verify -CAfile ~/.miniproxy/ca.crt <某条抓包导出的叶子证书>` 应输出 OK；或直接
> `curl --cacert ~/.miniproxy/ca.crt -x http://127.0.0.1:34567 https://www.bing.com/` 能拿到状态码。

### 4. 开始抓包

> **先分清两件事**：「启动服务」（第 1 步）只是让 web 服务与代理服务开始监听；
> 「开启系统代理」（本节第一条）才把系统流量交给 MiniProxy。前者自动，后者手动。

- **一键系统代理（推荐）**：打开界面 http://127.0.0.1:9000，点击右上角「🌐 系统代理」即可将系统 HTTP/HTTPS 代理指向 MiniProxy；再次点击恢复原状（自动备份/还原原有代理配置）
- **退出自动恢复**：开启系统代理后，无论程序是 Ctrl+C 正常退出、被 `kill` 强杀还是崩溃，
  都会自动把系统代理恢复为开启前的状态，不会出现「程序已退出、代理仍指向 34567 死端口」导致断网。
  实现为两层：正常退出由信号处理（SIGINT/SIGTERM/SIGHUP）恢复；强杀/崩溃则由一个看门狗
  子进程（`miniproxy --sysproxy-watchdog <父PID>`）检测到父进程消失后按备份恢复。
  存活判定用**父子间的 socketpair**：父进程无论正常退出、`kill -9` 还是崩溃，其持有的那端都会关闭，
  子进程读到 EOF 即恢复——不依赖 `ps` 轮询（受限终端 / 沙箱里 `ps` 会执行失败，
  旧实现会把它误判成「父进程已死」，导致刚开启的系统代理在 1 秒内被自动关掉）。
- **极端兜底**：若整个进程组被一起杀掉（连看门狗也没能执行），手动执行
  `networksetup -setwebproxystate Wi-Fi off && networksetup -setsecurewebproxystate Wi-Fi off`
  即可恢复直连。
- **手动设置**：设置 HTTP/HTTPS 代理为 `127.0.0.1:34567`
- **curl**：`curl -x http://127.0.0.1:34567 https://httpbin.org/get`
- **浏览器**：`chrome --proxy-server=http://127.0.0.1:34567`
- **手机 / 局域网设备**：代理设为 `<本机局域网IP>:34567`（如 `192.168.1.224:34567`），
  并在设备上安装信任 CA 证书（见下节）；界面检测到局域网设备接入时会自动弹出安装引导（含证书下载二维码）
- 手机抓包需让 API 对局域网开放以供下载证书：`MINIPROXY_API_HOST=0.0.0.0 cargo run`

### 5. 手机抓包（TLS 握手失败排查）

手机上所有请求报「TLS 握手失败（客户端拒绝 MITM 证书或提前断开）」，
原因是**设备尚未安装信任 MiniProxy CA 证书**——代理本身连通正常（否则连记录都不会有）：

1. **下载证书**：手机浏览器打开 `http://<本机局域网IP>:9000/api/ca.crt`
   （界面横幅会显示完整地址，也可直接扫二维码；或用 AirDrop/微信传 `~/.miniproxy/ca.crt`）
2. **iOS 信任（两步缺一不可）**：
   设置 → 通用 → VPN与设备管理 → 安装描述文件；
   再到 设置 → 通用 → 关于本机 → **证书信任设置**，对 MiniProxy CA 开启完全信任。
   漏掉第二步依然全部握手失败。
3. **Android**：设置 → 安全 → 更多安全设置 → 加密与凭据 → 安装 CA 证书。
   注意 Android 7+ 多数 App 默认不信任用户证书，只有浏览器等部分应用可用。
4. **仍有个别失败很正常**：证书固定（pinning）的 App（微博、穿山甲/头条广告 SDK、
   球米宝、银行类、裸 IP 直连等）即使装了证书也无法解密。MiniProxy 会在同域名失败 3 次后
   **自动改为直通**（不再解密但 App 恢复可用），也可用 `MINIPROXY_NO_MITM=域名` 手动加入白名单。

**怎么判断证书真的生效了**：在界面按「应用/域名」找一条普通 HTTPS 请求（如 App 内的
业务接口、或手机浏览器打开 `https://httpbin.org/get`），若能看到 200 与请求/响应正文，
说明解密已生效；此时仍报「TLS 握手失败」的只是那些做了证书固定的域名。

局域网 IP 自动探测（`ifconfig` 解析，排除 Clash TUN fake-IP 网段），也可通过
`MINIPROXY_API_HOST=0.0.0.0` 让界面/API 对局域网开放后由手机直接访问。

打开界面：**http://127.0.0.1:9000**

### 6. 抓不到某个 App 的包？（不使用 macOS 系统代理的客户端）

界面里的「系统代理」只对**走系统网络栈**的客户端生效——浏览器（Chromium/WebKit）、
微信/企业微信、`curl`、Electron 应用等；而自带 HTTP 栈、不读 macOS 系统代理配置的程序**看不见**：

| 客户端 | 是否读 macOS 系统代理 | 表现 |
|---|---|---|
| 浏览器 / Electron / curl | ✅ | 一键系统代理后立刻能抓到 |
| Rust（reqwest+rustls）/ Go / Java / Node(undici) | ❌ 默认不读 | 界面里只有它的其他流量（或什么都没有） |

**典型案例：Codex（ChatGPT.app 内置的 `codex` 二进制）**

- 验证方法：`codex doctor` → `Connectivity` 段。若出现
  `system proxy: manual` + `respect system proxy: disabled`，说明它**完全无视**系统代理
  （对应的特性开关 `respect_system_proxy` 仍是 under development）。
  此时界面能抓到的只有 App 前端（标注为 `Codex (Service)`）的遥测/RUM 请求，
  真正的对话请求一条都没有——这不是代理坏了。
- **正确做法**：这类客户端只认 `HTTP_PROXY / HTTPS_PROXY / ALL_PROXY` 环境变量。
  Codex 会在启动时读取 `~/.codex/.env`，把它指向 MiniProxy 即可：

  ```bash
  # ~/.codex/.env
  HTTP_PROXY="http://127.0.0.1:34567"
  HTTPS_PROXY="http://127.0.0.1:34567"
  NO_PROXY="localhost,127.0.0.1,::1"
  ```

  改完**重启 Codex**（环境变量只在进程启动时读一次，已运行的实例仍走旧配置）。
  出站照常经「上游级联」转发，所以被墙站点照样能访问，HTTPS 依旧被解密抓包；
  CA 无需额外配置（codex 的 rustls 走系统原生证书库，信任钥匙串里的 MiniProxy Root CA）。
- **Codex 的对话不是普通 POST，而是 WebSocket**：
  `wss://chatgpt.com/backend-api/codex/responses`。在界面里按「协议 → WebSocket」
  筛选，或搜索 `codex/responses`，提示词与流式回复会以帧为单位记录在详情里。
- 其他同类客户端同理，例如：给 `openai-python` / `requests` 设 `HTTPS_PROXY`，
  或给 Gradle/Maven（Java）加 `-Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=34567`。

## 已知限制

- 响应体采用「流式转发 + 边收边存」，单条记录正文最多保留 4 MB（超出部分截断并标记）。
- 详情面板的原始数据视图（十六进制 / Base64 / 下载）单侧最多回传 256 KB（原始大小照实显示，超出部分标注「视图已截断」）。
- WebSocket 不再协商 permessage-deflate 压缩扩展（MITM 转发升级请求时剥掉
  `Sec-WebSocket-Extensions`），因此抓到的帧始终是明文，无需解压；
  代价是 WS 流量失去压缩（对抓包场景无所谓）。
- WebSocket 文本帧**按连接**保留，文本总额度 8 MB（`ws::WS_TEXT_BUDGET`）：额度内
  逐条完整入库（几十 KB 的 `response.create` 也能整段看），超出后该条只留前缀并标记
  「内容已截断」。消息头同时显示 **payload 字节数**与**文本字符数**——中文一个字 3 字节，
  两个数字不同是正常的。
- 「按应用」分组通过 `lsof` 识别**本机客户端进程**（约 60ms/连接，已缓存；`MINIPROXY_NO_APP=1` 可关闭）。经远程机器转发进来的流量无法识别原始进程，会显示为「未知应用」。
- HTTP/2 上游以 HTTP/1.1 对接（ALPN 不向上游协商 h2），绝大多数站点兼容。
- HTTP/1.1 Keep-Alive 多路复用场景下，一条 UI 记录对应一次「请求-响应」往返。
