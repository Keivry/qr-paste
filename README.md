<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# qr-paste

跨设备文本输入中继工具：扫码后将手机上的文本写入 PC 剪贴板，并可自动粘贴到当前焦点输入框。

## 工作原理

```
手机浏览器 ──WebSocket──▶ 服务端（公网 Linux）◀──gRPC── PC 客户端（Windows）
```

1. PC 客户端启动，通过 gRPC 连接服务端，服务端分配一次性令牌并返回扫码 URL
2. PC 客户端将 URL 渲染为二维码展示在窗口中
3. 手机扫码打开浏览器页面，输入文本后点击发送
4. 服务端通过 WebSocket 收到文本，经 gRPC 流转发至 PC 客户端
5. PC 客户端将文本写入系统剪贴板，并可选择模拟 `Ctrl+V` 自动粘贴

一次会话令牌扫码即失效（单次使用），有效期 5 分钟（可配置）。

## 组件

| 目录 | 说明 |
|------|------|
| `server/` | 服务端（Rust + Axum + Tonic），部署于公网 Linux |
| `client/` | Windows 桌面客户端（Rust + egui），运行于 PC |
| `common/` | 客户端与服务端共享的 WebSocket 消息类型定义 |
| `proto/` | gRPC 接口定义（`relay.proto`） |

## 快速开始

### 服务端

**环境要求**：Linux，Rust 工具链，公网 IP（必须配合 nginx、HAProxy 等反向代理做 TLS 终止，并避免直接暴露 gRPC 端口）

1. 复制配置文件并按需修改：
   ```bash
   cp server.example.toml server.toml
   vim server.toml  # 必填：public_base_url、grpc_auth_token
   ```
   默认情况下，`http_bind_host` 与 `grpc_bind_host` 都是 `127.0.0.1`，适合放在 nginx / HAProxy 之后运行；只有在你明确需要直接监听所有网卡时，才改成 `0.0.0.0`。

2. 编译并运行：
   ```bash
   cargo build --release -p server
   ./target/release/server
   ```

3. 反向代理参考配置见 `docs/nginx.conf.example` 与 `docs/haproxy.cfg.example`

### PC 客户端（Windows）

**环境要求**：Windows 10+，Rust 工具链（交叉编译目标 `x86_64-pc-windows-msvc`）

1. 复制配置文件：
   ```bash
   cp client.example.toml client.toml
   vim client.toml  # 必填：server_host、grpc_auth_token
   ```
   Windows 打包运行时，客户端会优先读取 `client.exe` 同目录下的 `client.toml`；若不存在，则回退到当前工作目录。
   - 直连内网 / 明文 gRPC：`server_host = "10.0.0.5"`，默认端口 `50051`（也可显式写 `grpc_port = 50051`）
   - 通过 HAProxy / nginx 提供的 TLS gRPC 入口：`server_host = "https://grpc.example.com"`，默认端口 `443`（也可显式写 `grpc_port = 443`）

2. 编译并运行：
   ```bash
   cargo build --release -p client
   ./target/release/client.exe
   ```

    Windows 客户端连接 `https://` gRPC 入口时，默认使用内置的 Mozilla 根证书（`webpki-roots`）进行 TLS 校验，而不是依赖本机 Windows 证书库。这能避免某些机器在加载系统根证书时卡住；对 Let's Encrypt 等公网 CA 证书通常兼容，但如果你使用的是企业内网私有 CA，则需要额外补充受信根证书支持。

启动后窗口显示二维码，手机扫码即可使用。连接成功后，若系统托盘初始化正常，窗口会自动缩小到系统托盘。

如果希望点击窗口右上角关闭按钮时只隐藏到托盘而不退出，可在 `client.toml` 中启用：`minimize_on_close = true`。

## 配置

- 服务端配置：`server.toml`，参考 `server.example.toml`（每项均有注释）
- 客户端配置：`client.toml`，参考 `client.example.toml`（每项均有注释）
- `server.toml` 中的 `http_bind_host` / `grpc_bind_host` 默认都是 `127.0.0.1`，用于把明文 HTTP / gRPC 限制在本机或内网；若你确实要让后端直接监听全部网卡，可显式改为 `0.0.0.0`
- `behind_trusted_proxy = true` 仅应在服务端确实部署在可信反向代理之后时开启。开启后限流会信任 `X-Forwarded-For` / `X-Real-IP`；若直接暴露服务端或代理不受控，请保持默认 `false`
- `client.toml` 中的 `server_host` 支持两种写法：
  - 仅主机名 / IP（客户端会按 `http://<host>:<grpc_port>` 连接，默认端口 `50051`）
  - 完整 `http://` / `https://` URL（适合挂在 HAProxy / nginx 后的 gRPC 入口；若未另写 `grpc_port`，`https://` 默认走 `443`）
- 连接中界面会同时显示当前连接目标，以及最近一次连接/订阅/流失败原因，便于在 Windows release 下直接排障
- `start_minimized = true`：启动时直接隐藏窗口，仅显示托盘图标
- `minimize_on_close = true`：点击窗口关闭按钮时隐藏到托盘；若要真正退出，请使用托盘菜单中的“退出”

## 网络要求

| 端口 | 用途 | 方向 |
|------|------|------|
| `http_port`（默认 8080） | HTTP + WebSocket（手机浏览器） | 公网入 |
| `grpc_port`（默认 50051） | gRPC（PC 客户端） | 仅受信网络 / TLS 反代后暴露 |

服务端不处理 TLS，生产环境必须由 nginx、HAProxy 等反向代理终止 HTTPS/WSS，并为 gRPC 提供 TLS 入口或至少限制在受信网络中。不要直接将裸 `grpc_port` 暴露到公网。

如果 PC 客户端通过 HAProxy / nginx 的 443 端口接入 gRPC，请把 `client.toml` 写成类似：

```toml
server_host = "https://grpc.example.com"
grpc_port = 443
```

这样客户端会以 TLS + HTTP/2 方式连接反向代理；若 `server_host` 写成 `https://grpc.example.com` 且未额外填写 `grpc_port`，客户端也会默认使用 `443`。若仍写成裸主机名，则客户端会按明文 `http://` 连接，不适用于 HTTPS 反代入口。

## 安全机制

- 令牌单次使用（扫码后立即失效，防止重放）
- gRPC 订阅需提供共享鉴权令牌（`grpc_auth_token`）
- 令牌有效期可配置（默认 5 分钟）
- HTTP / WebSocket 按 IP 限流
- WebSocket 全局并发连接数上限
- WebSocket 握手阶段设置消息 / frame 大小上限（默认 64 KB）
- WebSocket 空闲超时断连（默认 90 秒）
- 过期令牌定时清理

## 安全注意事项

- `grpc_auth_token` 必须使用高熵随机值，并且不要提交到仓库。
- `client.toml` 与 `server.toml` 中的 `grpc_auth_token` 以明文形式保存在本地文件中，请限制文件权限并避免进入版本控制。
- `auto_paste` 默认关闭；开启后，来自手机端的文本会自动注入到当前焦点窗口，请仅在受信场景下使用。
- `enter_after_paste` 默认关闭；仅在 `auto_paste = true` 时生效，开启后粘贴完成会自动按下回车键，适用于需要直接提交的场景。
- 客户端现在会串行处理收到的剪贴板/自动粘贴任务，避免连续消息触发并发线程导致顺序错乱或剪贴板被后写覆盖。
- 若部署在反向代理后，建议仅让代理对外暴露 HTTP / gRPC TLS 入口，后端服务保持监听在内网或 `127.0.0.1`。

## 许可证

本项目采用 **MIT OR Apache-2.0** 双许可证发布。详见仓库根目录下的 `LICENSE-MIT` 与 `LICENSE-APACHE`。

## 依赖

- **服务端**：`axum` · `tonic` · `prost` · `tokio` · `dashmap` · `tower` · `tower-http`
- **客户端**：`eframe` · `egui` · `tonic` · `prost` · `arboard` · `enigo` · `tray-icon` · `qrcode`
