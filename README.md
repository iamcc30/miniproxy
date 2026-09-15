# MiniProxy 抓包代理工具

基于 **Rust（后端）+ React（前端）** 的 HTTP/HTTPS/WebSocket/TCP 抓包调试代理，支持 MITM 解密、内容解压、按域名分组筛选、实时可视化与 JSON/HAR 导出。

## 功能特性

| 功能 | 说明 |
|---|---|
| 多协议捕获 | HTTP 明文代理、HTTPS（TLS MITM 中间人解密）、WebSocket（逐帧捕获消息）、纯 TCP 隧道（流量统计 + 首包 hex 预览） |
| 分组 / 筛选 / 搜索 | **分组方式可切换**（默认不分组）：域名 / 站点（主域名归并）/ 应用（客户端进程）/ 协议 / 文件类型 / 状态码，分组标题支持「只看此组」一键筛选；关键词搜索 URL/域名/站点/应用，并深入**请求头/请求体、响应头/响应体（含解压后内容）、WebSocket 消息文本**，正文命中的记录会标注「内容匹配」；协议、文件类型、方法、状态码、域名、**站点、应用**均支持多选（同组或、跨组且），已选条件以可移除 chips 呈现，支持一键清空 |
| 请求/响应完整记录 | 请求行、请求头、请求体、响应状态、响应头、响应体全部记录并展示 |
| 解包能力 | 自动解压 gzip / deflate(zlib+raw) / brotli / zstd；chunked 分片由 HTTP 层自动还原；WebSocket 文本帧还原（支持续帧合并） |
| 可视化界面 | SSE 实时推送、暂停/恢复、明暗主题切换（浅色/深色/跟随系统）、历史记录查看与 JSON/HAR 导出 |
| 一键系统代理 | 界面右上角一键开启/关闭 macOS 系统代理（HTTP+HTTPS+SOCKS）；开启前自动备份原有代理配置，关闭时恢复，不破坏 Clash 等已有配置 |

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
│       └── util.rs     # 通用工具
└── frontend/           # React + Vite + TypeScript
    └── src/            # App.tsx（列表/过滤/详情）、api.ts、theme.ts、styles.css
```

## 快速开始

### 1. 启动后端

```bash
cd backend
cargo run            # 默认代理端口 34567，界面端口 9000
```

可用环境变量：
- `MINIPROXY_PORT`（默认 34567）：代理监听端口
- `MINIPROXY_API_PORT`（默认 9000）：界面/API 端口
- `MINIPROXY_STATIC`：前端静态文件目录
- `MINIPROXY_UPSTREAM_PROXY`：上游级联代理（如 `http://127.0.0.1:7890`）

### 上游级联（抓包 + 科学上网）

默认情况下 MiniProxy **直连**目标网站。如果目标站点被墙（如 chatgpt.com），
直连会在 TLS 握手阶段被中断，界面显示 `tls handshake eof` 错误。
此时可开启「上游级联」，让出站流量经本机其他代理（Clash 等）转发：

```bash
MINIPROXY_UPSTREAM_PROXY=http://127.0.0.1:7890 cargo run
```

开启后：
- 到所有源站的连接（HTTP/HTTPS/WS/TCP）先经上游代理（HTTP CONNECT 隧道）出站
- HTTPS 依然被 MiniProxy 解密抓包（级联只影响出站路径，不影响 MITM）
- 一键系统代理 + 上游级联组合：浏览器正常访问被墙站点，同时流量全部被抓包记录

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

### 3. 构建前端（首次）

```bash
cd frontend
npm install && npm run build
```

构建产物默认从 `./static` 或 `./frontend/dist` 提供。

### 4. 信任 CA 证书（解密 HTTPS 必需）

首次启动后，CA 证书位于 `~/.miniproxy/ca.crt`，也可在界面右上角点击「CA 证书」下载。

macOS 安装并信任：

```bash
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain ~/.miniproxy/ca.crt
```

浏览器（Chrome/Safari）跟随系统钥匙串即可。**不安装 CA 时 HTTPS 流量将以 TCP 隧道形式记录（无法解密内容）。**

### 5. 开始抓包

- **一键系统代理（推荐）**：打开界面 http://127.0.0.1:9000，点击右上角「🌐 系统代理」即可将系统 HTTP/HTTPS 代理指向 MiniProxy；再次点击恢复原状（自动备份/还原原有代理配置）
- **手动设置**：设置 HTTP/HTTPS 代理为 `127.0.0.1:34567`
- **curl**：`curl -x http://127.0.0.1:34567 https://httpbin.org/get`
- **浏览器**：`chrome --proxy-server=http://127.0.0.1:34567`

打开界面：**http://127.0.0.1:9000**

## 已知限制

- 响应体采用「流式转发 + 边收边存」，单条记录正文最多保留 4 MB（超出部分截断并标记）。
- WebSocket 压缩扩展（permessage-deflate）暂不解压，记录原始帧。
- 「按应用」分组通过 `lsof` 识别**本机客户端进程**（约 60ms/连接，已缓存；`MINIPROXY_NO_APP=1` 可关闭）。经远程机器转发进来的流量无法识别原始进程，会显示为「未知应用」。
- HTTP/2 上游以 HTTP/1.1 对接（ALPN 不向上游协商 h2），绝大多数站点兼容。
- HTTP/1.1 Keep-Alive 多路复用场景下，一条 UI 记录对应一次「请求-响应」往返。
