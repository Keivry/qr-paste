<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# qr-paste

跨设备文本输入中继工具：扫码后将手机上的文本写入 PC 剪贴板，并可自动粘贴到当前焦点输入框。

## 工作原理

```text
手机浏览器 ──WebSocket──▶ 服务端（公网 Linux）◀──gRPC── PC 客户端（Windows）
```

1. PC 客户端通过 gRPC 连接服务端，获取一次性扫码 URL 并生成二维码。
2. 手机扫码打开浏览器，完成鉴权（bootstrap → ws-ticket → WebSocket 握手）。
3. 手机输入文本发送，服务端经 gRPC 转发至 PC 客户端。
4. PC 客户端写入系统剪贴板（可选自动粘贴）。

## 快速开始

### 服务端 (Linux)

需配合反向代理提供 TLS 终止。代理配置示例见 `docs/` 目录下的 `nginx.conf.example`、`haproxy.cfg.example`、`Caddyfile.example`。

```bash
cp server.example.toml server.toml
vim server.toml  # 修改 public_base_url, grpc_auth_token, upload_dir
cargo build --release -p server
./target/release/server
```

### 客户端 (Windows)

支持 Windows 10+，也可在 Linux 上交叉编译。

```bash
cp client.example.toml client.toml
vim client.toml  # 修改 server_host, grpc_auth_token
cargo build --release -p client
.\target\release\client.exe
```

**`server_host` 写法说明：**
- **生产推荐**：`"https://grpc.example.com"`（使用 TLS，默认 443 端口）
- **受控内网**：`"10.0.0.5"`（明文传输，禁止用于公网）

## 配置参考

部分关键配置说明如下（完整说明见各自的 `*.example.toml`）：

| 字段名 | 默认值 | 说明 |
|--------|--------|------|
| `public_base_url` | 无 (必填) | 手机访问的公网地址 (例: `https://relay.example.com`) |
| `grpc_auth_token` | 无 (必填) | C/S 认证密钥。**明文存储，请限制文件权限且勿提交版本库** |
| `upload_dir` | 无 (必填) | 服务端文件上传暂存目录 |
| `auto_paste` | `false` | 自动将文本注入当前焦点窗口 (请仅在受信场景开启) |
| `simulate_key_after_paste` | 无 | 粘贴后模拟按键 (如 `"Return"`, `"ctrl+Return"`) |
| `minimize_on_close` | `false` | 点击关闭按钮时隐藏到托盘而非退出 |
| `start_minimized` | `false` | 客户端启动时直接隐藏到托盘 |

## 文件传输

支持手机浏览器向 PC 传输文件，所有路径均经过 SHA-256 校验。
- **PC 在线 (HTTP_STREAMING)**：边传边下，耗时约为 `max(上传, 下载)`。需在反向代理关闭响应缓冲 (如 Nginx 的 `proxy_buffering off`)。
- **PC 离线 (RELAY)**：先上传暂存服务端磁盘，PC 上线后再下载。

## 网络要求

服务端本身不处理 TLS，生产环境**必须**通过反向代理终止 HTTPS/WSS。

| 端口 | 协议 | 用途 | 访问要求 |
|------|------|------|----------|
| 8080 | HTTP/WS | 供手机浏览器访问，处理文件与 WebSocket | 需公网可达 |
| 50051| gRPC | 供 PC 客户端连接 | 仅受信网络或经 TLS 反代 |

## 安全机制

- **短效令牌**：扫码后立即失效，WebSocket Ticket 仅 15 秒有效。
- **严格鉴权**：连接需完成 `bootstrap` → `ws-ticket` → `WebSocket` 三步握手。
- **Origin 校验**：严格校验请求来源与 `public_base_url` 匹配。
- **会话撤销 (Epoch)**：PC 可主动撤销所有旧会话，立即断开现有连接。
- **资源限制**：内置 IP 限流与 WebSocket 并发连接数上限限制。

## 许可证

MIT OR Apache-2.0。详见 LICENSE-MIT 与 LICENSE-APACHE。