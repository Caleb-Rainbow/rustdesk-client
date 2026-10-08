# WSS 传输验证

本测试包直接复用 `libs/hbb_common/src/strict_wss.rs` 的生产代码，不替换 TLS 或 WebSocket 实现，也不需要编译桌面捕获、编解码和 Flutter 依赖。

```powershell
cargo test --locked --manifest-path tests/wss-transport/Cargo.toml
```

测试覆盖真实 TCP 回环上的 WebSocket 握手、16 KiB 帧限制、大消息重组、空消息、Ping/Pong 与 Close 互通，以及自签名证书的持续拒绝和 TLS 连接超时。还使用服务端现有的 tokio-tungstenite 0.17.2 验证注册/中继 protobuf 字节完整及 1 MiB 双向传输。这里验证传输互通，不代替实际 RustDesk 服务端注册和桌面会话验收。受控 Tokio 双工流用于确定性验证发送超时和外部取消后必须重连。

有效公开证书的正向握手测试默认忽略，需显式指定授权的在线端点后运行，并单独记录结果：

```powershell
$env:RUSTDESK_WSS_TEST_URL = 'wss://remote.yingluozhiwei.cn/ws/id'
cargo test --locked --manifest-path tests/wss-transport/Cargo.toml verified_public_wss_handshake -- --ignored
```

Linux 可验证通用 Rustls 路径；Windows 运行同一测试才能验证 Schannel 路径。Android 还需要实际应用完成平台证书校验器初始化并执行设备验收。

帧载荷上限不等同于 TCP 包或 TLS 记录的固定长度，也不构成流量与浏览器不可区分或无法被网络策略拦截的保证。
