# DERP 一致性测试

本项目不把“Rust 单元测试通过”当作 DERP 兼容性的充分证据。`scripts/official-conformance.sh`
会启动真实的 release 版 `derper-rs` 进程，再使用固定版本的 Tailscale 官方 Go 客户端
（`tailscale.com v1.100.0`）从进程外进行黑盒测试。

## 上游测试来源

Tailscale 公布了 DERP 测试，但它们大多把 Go `derpserver.Server` 直接嵌入同一个测试
进程，不能通过地址参数直接测试第三方服务器：

- `derp/derp_test.go`：转发、PeerGone、Ping/Pong、watch、preferred 状态；
- `derp/derphttp/derphttp_test.go`：官方 HTTP 客户端、Ping、probe、mesh watcher；
- `derp/derpserver/derpserver_test.go`：重复连接、mesh、限速及服务端内部状态；
- `net/stun/stun_test.go`：官方 STUN 报文生成和解析。

本项目在 `bench/conformance/official_test.go` 中把可观察的线缆级行为改写成外部服务器
测试。服务端内部数据结构、Go 特有并发实现、XDP 和只测试客户端错误处理的用例不适合
直接移植，仍由本项目自己的 Rust 单元测试、集成测试和压力测试覆盖。

社区方面，`rajsinghtech/rustscale` 的 `crates/derp/src/server.rs` 公布了一个用于集成
测试的简化 DERP server 及测试。本项目移植了其中可兼容官方协议的双向转发、最新连接
路由和非 Fast Start Upgrade 行为。它的“新连接关闭同 key 旧连接”规则与 Tailscale
当前的重复连接 Health 语义不同，因此只采用双方一致的“最新连接接收新流量”，并继续
按官方行为验证旧连接收到 Health、恢复时收到空 Health。

## 当前黑盒覆盖

| 领域 | 外部黑盒断言 |
|---|---|
| 官方 DERP-over-HTTP 客户端 | 三客户端连接、源 key、A→B、B→C |
| 基本转发 | 社区双客户端双向发送 |
| 断线传播 | reverse-path `PeerGone(Disconnected)` |
| 目的端不存在 | 普通包静默；disco 包 `PeerGone(NotHere)`；初始 burst 限制为 3 |
| 控制帧 | Ping/Pong payload 完整回显、NotePreferred 状态 |
| mesh watcher | 初始快照、regular/mesh flags、上线和下线事件 |
| 重复连接 | 两条连接 Health 非空、最新连接路由、恢复后空 Health |
| HTTP | probe、latency-check、方法限制、Upgrade Required、`generate_204` challenge |
| Upgrade | 官方 Fast Start 客户端及社区标准 HTTP 101 路径 |
| STUN | 官方 `stun.Request` 生成、官方 `stun.ParseResponse` 解析、事务 ID 和映射地址 |

此外，`cargo test --all-targets --locked` 覆盖协议 frame 编解码/边界、加密握手、
WebSocket DERP 和 Rust↔Rust regional mesh。生产 RSS 压测继续由
`scripts/rss-benchmark.sh` 独立执行。

## 运行

```bash
scripts/official-conformance.sh
```

端口占用时可以覆盖：

```bash
DERP_PORT=43340 STUN_PORT=43478 scripts/official-conformance.sh
```

该套件已接入 GitHub Actions；任何官方客户端互操作行为回归都会使 CI 失败。
