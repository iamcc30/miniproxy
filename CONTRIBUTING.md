# 贡献指南

感谢你想为 MiniProxy 做点东西。这里没有太多规矩，但请先读一下这几条。

## 开发环境

```bash
# 后端（Rust，需 1.70+，edition 2021）
cd backend
cargo run

# 前端（React + Vite + TypeScript）
cd frontend
npm install
npm run dev      # 开发模式，Vite 代理到后端
npm run build    # 产出 frontend/dist，由后端静态托管
```

默认端口：代理抓包口 `0.0.0.0:34567`，界面 / API `127.0.0.1:9000`。端口都可用环境变量覆盖，详见 README。

## 改代码前请注意

- **前端图标**：一律用 `frontend/src/icons.tsx` 里的内联 SVG，不要引入图标库，也不要写 emoji —— 项目要求离线可构建。
- **配置写盘**：`~/.miniproxy/config.json` 必须整体写（上游级联 + 分流规则一起），否则保存其一会冲掉另一部分。
- **出站连接**：所有出站拨号走 `dial.rs` 的 `tcp_dial`，别直接 `TcpStream::connect`，否则会绕过上游级联与分流规则。
- **端口监听**：用 `Server::try_bind` 而不是 `bind`，端口被占用时要报错退出，不能 panic 成幽灵进程。
- **WebSocket**：`tls_wrap()` 必须保持 HTTP/1.1-only。

## 提交 PR

1. Fork 本仓库，从 `main` 切出特性分支。
2. 保证 `cargo build --release` 与 `npm run build` 都通过。
3. 提交信息尽量说明「为什么改」，而不只是「改了什么」。
4. PR 描述里写清复现步骤或验证方式——涉及代理行为的改动，附一条可复现的 curl 命令最好。

## 报 Bug

提 issue 时请附上：操作系统版本、MiniProxy 版本（或提交哈希）、复现步骤、以及期望行为与实际行为。抓包类问题的排查方法 README「已知限制」与诊断部分已有总结，可先对照。

## 许可

向本仓库提交贡献即表示你同意你的贡献以 [MIT License](./LICENSE) 授权。
