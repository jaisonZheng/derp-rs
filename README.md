# derp-rs

`derp-rs` 是一个面向生产环境的高性能 Rust DERP 中继服务器。它实现了
Tailscale DERP v2 线协议、Fast Start、WebSocket-DERP、STUN、区域 Mesh、
官方运维 HTTP 端点、客户端准入、限速和优雅重启。

DERP 只中继由节点端到端加密的数据包，服务器无法解密 WireGuard 流量。

> 本项目是独立实现，不是 Tailscale 官方产品。部署前请自行进行安全审计和容量验证。

## 与官方 derper 的功能覆盖

### DERP 核心协议与服务器行为

| 模块 | 官方行为 | derp-rs 实现 | 验证方式 |
| --- | --- | --- | --- |
| 连接入口 | 原生 `Upgrade: DERP`、`Derp-Fast-Start: 1` | 完整 | 官方 Go `derphttp.Client`、标准 HTTP 101 黑盒测试 |
| WebSocket | `derp` WebSocket 子协议 | 完整 | Rust 端到端握手与 Ping/Pong 集成测试 |
| 身份握手 | `ServerKey`、NodeKey、NaCl `crypto_box`、`ClientInfo`/`ServerInfo` | 完整 | 官方客户端真实加密握手、协议向量测试 |
| DERP v2 转发 | 以 NodeKey 寻址，接收包携带源 NodeKey，最大载荷 64 KiB | 完整 | 官方客户端三节点转发、社区双向转发测试 |
| 反向路径通知 | 源节点最后一条连接断开后发送 `PeerGone(Disconnected)` | 完整 | 官方 `TestSendRecv` 行为黑盒适配 |
| 目的节点不存在 | 普通包静默；disco wrapper 返回 `PeerGone(NotHere)` | 完整 | 普通包/Disco 包对照测试 |
| NotHere 限速 | 每连接初始 burst 3，随后每秒补充 3 | 完整 | 连续 6 包只收到 3 个响应 |
| 重复连接 | 同 NodeKey 多连接、非空/空 `Health`、最新活动连接路由 | 完整 | 官方重复连接语义和社区 latest-writer 路由测试 |
| 保活和状态 | `Ping`/`Pong`、KeepAlive、NotePreferred、Health | 完整 | payload 回显、preferred 指标状态机、frame round-trip |
| 优雅重启 | `FrameRestarting`，建议重连及尝试时长 | 完整 | 协议编解码测试、SIGINT/SIGTERM 关闭路径 |
| 区域 Mesh | Mesh PSK、WatchConns、PeerPresent/Gone、ForwardPacket、ClosePeer | 完整 | watcher 黑盒测试、Rust↔Rust 和 Rust↔官方 Go derper 跨服务器转发 |
| STUN | Tailscale Binding Request 校验、IPv4/IPv6 XOR-MAPPED-ADDRESS | 完整 | 官方 `stun.Request` 生成和 `stun.ParseResponse` 解析 |
| HTTP 运维端点 | probe、latency-check、generate_204、bootstrap-dns | 完整 | 状态码、方法限制、challenge header 黑盒测试 |
| 可观测性 | `/metrics`、`/debug/vars`、`/debug/check` | 完整 | Prometheus 指标及连接状态测试 |
| TLS | PEM 证书链、TLS 1.2/1.3 | 完整 | 1,000–10,000 TLS 连接生产规模压测 |
| 客户端准入 | `DERPAdmitClientRequest` 风格的 NodeKey/来源验证 | 完整 | 可配置 HTTP Admission Controller，支持 fail-open/fail-closed |
| 资源保护 | 入站令牌桶、burst、有界发送队列、慢客户端写超时 | 完整 | 限速单测、5,000 慢接收端和连接抖动压测 |

全部官方线协议帧均已实现：

| 帧值 | 帧类型 | 帧值 | 帧类型 | 帧值 | 帧类型 |
| ---: | --- | ---: | --- | ---: | --- |
| `0x01` | ServerKey | `0x06` | KeepAlive | `0x11` | ClosePeer |
| `0x02` | ClientInfo | `0x07` | NotePreferred | `0x12` | Ping |
| `0x03` | ServerInfo | `0x08` | PeerGone | `0x13` | Pong |
| `0x04` | SendPacket | `0x09` | PeerPresent | `0x14` | Health |
| `0x05` | RecvPacket | `0x0a` | ForwardPacket | `0x15` | Restarting |
|  |  | `0x10` | WatchConns |  |  |

### 与官方 `cmd/derper` 平台集成的差异

下列项目不属于 DERP 线协议本身，不影响 Tailscale/Headscale 客户端使用本服务器：

| 官方平台能力 | derp-rs 方案 | 影响 |
| --- | --- | --- |
| ACME、GCP 证书存储 | 接收用户提供的 PEM 证书和私钥 | 证书申请/续期交给 Caddy、certbot 或运维系统 |
| 本机 `tailscaled` `WhoIsNodeKey` | 通用 HTTP Admission Controller | 可对接任意控制面，但不直接依赖本机 tailscaled |
| 动态 bootstrap DNS | 可选静态 JSON 文件 | 适合私有部署；DNS 内容由运维系统生成 |
| Linux XDP 快速路径 | Tokio 用户态转发 | 不需要 XDP 权限；当前基准仍高于官方普通用户态 derper |
| 实验性 ACE proxy | 未实现 | ACE 不属于稳定 DERP v2 服务器兼容范围 |

协议行为以 Tailscale
[`derp`](https://pkg.go.dev/tailscale.com/derp)、
[`derpserver`](https://pkg.go.dev/tailscale.com/derp/derpserver) 和
[`cmd/derper`](https://github.com/tailscale/tailscale/tree/main/cmd/derper)
为基准。实现时复核的上游主线提交为
`cfd101f9d773695def27a5f6289fc25ac36ac991`，稳定互操作与性能基线为
Tailscale v1.100.0。

## 快速开始

需要 Rust 1.85 或更新版本。

```bash
cargo build --release --locked
./target/release/derper-rs \
  --addr 0.0.0.0:443 \
  --stun-addr 0.0.0.0:3478 \
  --private-key /var/lib/derper-rs/derper.key \
  --tls-cert /etc/letsencrypt/live/derp.example.com/fullchain.pem \
  --tls-key /etc/letsencrypt/live/derp.example.com/privkey.pem
```

首次启动会以 `0600` 权限原子创建持久化 DERP NodeKey。生产环境应直接让
`derper-rs` 终止 TLS；不要将 `/derp` 的流式连接放到会缓冲或改写 Upgrade
语义的普通反向代理后。

完整参数：

```bash
derper-rs --help
```

防火墙至少开放 TCP 443（或 `--addr` 配置端口）和 UDP 3478。
然后在控制平面或 Headscale 的 DERP map 中配置该节点，例如：

```yaml
regions:
  901:
    regionid: 901
    regioncode: private
    regionname: Private DERP
    nodes:
      - name: derp-1
        regionid: 901
        hostname: derp.example.com
        derpport: 443
        stunport: 3478
```

## 区域 Mesh

每台服务器使用同一份 32 字节十六进制 PSK：

```bash
openssl rand -hex 32 > /etc/derper-rs/mesh.key
chmod 600 /etc/derper-rs/mesh.key

derper-rs \
  --mesh-psk-file /etc/derper-rs/mesh.key \
  --mesh-with https://derp-2.example.com/derp
```

Mesh 使用 DERP 自身认证和 `WatchConns` 路由公告。应只在受信任节点间分发
PSK；收到 Mesh 权限后，对端可观察连接键并关闭客户端连接。

## 准入和限速

`--verify-client-url` 会发送兼容 `DERPAdmitClientRequest` 的 JSON：

```json
{"NodePublic":"nodekey:...","Source":"203.0.113.10"}
```

响应 `{"Allow":true}` 即放行。使用
`--verify-client-fail-open=false` 可在控制器不可用时拒绝连接。

`--rate-limit` 和 `--rate-burst` 按客户端限制入站字节；每个客户端采用有界队列，
慢客户端不会无限占用内存。Prometheus 指标位于 `/metrics`。

## Docker 与 systemd

```bash
docker build -t derper-rs .
docker run --rm \
  -p 443:3340/tcp -p 3478:3478/udp \
  -v derper-data:/var/lib/derper-rs \
  derper-rs
```

systemd 示例位于 [`deploy/derper-rs.service`](deploy/derper-rs.service)。

## 验证与性能

```bash
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
./scripts/official-conformance.sh
./scripts/benchmark.sh
```

一致性套件会启动 release 二进制，并用 Tailscale 官方 Go 客户端和社区公开测试行为
执行进程外黑盒验证。测试来源、适配原则和覆盖矩阵见
[`docs/CONFORMANCE.md`](docs/CONFORMANCE.md)。

### 官方客户端黑盒一致性

release 版服务器由固定版本的官方 Go 客户端从进程外测试，覆盖三节点转发、
源 NodeKey、Ping/Pong、PeerGone、NotHere 限速、WatchConns、重复连接 Health、
最新连接路由、NotePreferred、Fast Start、标准 Upgrade、HTTP 端点和 STUN。

| 环境 | 结果 |
| --- | ---: |
| Apple Silicon macOS release 进程 | 12/12 通过 |
| 腾讯云 Ubuntu 24.04 x86_64 release 进程 | 12/12 通过 |
| GitHub Actions Ubuntu | Rust 测试、Clippy、黑盒套件全部通过 |

完整测试来源和断言见 [`docs/CONFORMANCE.md`](docs/CONFORMANCE.md)。

### 转发吞吐对比

同机五轮中位数：Apple M3、16 个官方 Go DERP 客户端、1,200 字节载荷、
每轮 256,000 个已确认送达的数据包，Fast Start 明文 loopback。

| 服务端 | 五轮 packets/s | 中位数 | 有效载荷 | RSS 样本 |
| --- | --- | ---: | ---: | ---: |
| derp-rs 0.1.0 | 380,213；366,442；375,886；289,351；312,408 | **366,442** | **3.518 Gbit/s** | **4,704 KiB** |
| 官方 derper v1.100.0 | 259,203；280,457；256,018；263,363；286,846 | 263,363 | 2.528 Gbit/s | 22,208 KiB |

该环境下 derp-rs 中位吞吐高 **39.14%**，样本 RSS 低 **78.82%**；官方 Go
进程使用的 RSS 是 Rust 的 4.72 倍。完整方法和限制见
[`docs/BENCHMARK.md`](docs/BENCHMARK.md)。

### 生产规模 RSS 对比

腾讯云 Ubuntu 2 vCPU/1.92 GiB 主机，使用官方客户端创建 100–10,000 条连接。
每个场景启动全新服务器，覆盖空闲、持续转发、慢接收端背压和每 500 ms 替换约
10% 连接的 churn；TLS 使用相同临时证书。

| 传输 | 工作负载 | 连接数 | derp-rs 稳态 RSS | Go derper 稳态 RSS | Rust 降幅 |
| --- | --- | ---: | ---: | ---: | ---: |
| 明文 | Idle | 1,000 | **15.9 MiB** | 51.5 MiB | 69.17% |
| 明文 | Idle | 10,000 | **111.4 MiB** | 358.9 MiB | 68.96% |
| 明文 | Active | 5,000 | **70.0 MiB** | 204.1 MiB | 65.72% |
| 明文 | Slow receiver | 5,000 | **111.9 MiB** | 308.1 MiB | 63.69% |
| 明文 | Churn | 5,000 | **58.3 MiB** | 202.9 MiB | 71.26% |
| TLS | Idle | 1,000 | **24.0 MiB** | 79.1 MiB | 69.67% |
| TLS | Idle | 10,000 | **184.9 MiB** | 595.1 MiB | 68.93% |
| TLS | Active | 5,000 | **107.3 MiB** | 324.7 MiB | 66.95% |
| TLS | Slow receiver | 5,000 | **156.8 MiB** | 439.2 MiB | 64.30% |
| TLS | Churn | 5,000 | **96.0 MiB** | 314.4 MiB | 69.47% |

完整矩阵中，明文稳态 RSS 低 **62.38%–71.36%**、峰值低
**62.38%–79.85%**；TLS 稳态低 **63.65%–70.99%**、峰值低
**63.65%–78.91%**。5,000 连接 churn 的峰值尤其明显：明文 Rust/Go 为
58.6/290.8 MiB，TLS 为 100.1/391.4 MiB。

内存优化没有牺牲吞吐：5,000 连接下，相比原始 Rust 实现，明文 Idle/Active/Slow
分别再降低 24.84%/22.40%/16.74%，TLS 分别降低 17.30%/15.51%/9.01%；
同时五轮吞吐中位数由 313,906 提升到 366,442 packets/s。完整 CSV、采样方法和
结论边界见 [`docs/RSS-BENCHMARK.md`](docs/RSS-BENCHMARK.md)。

> 上述结果证明的是已定义硬件与 100–10,000 连接工作负载包络内的表现，不等同于
> 对所有操作系统、分配器、未来版本和无限连接数的数学保证。容量规划前应在目标主机
> 重跑仓库内脚本。

课程项目的 LaTeX 源文件与编译版报告见
[`docs/report/derp-rs-report.tex`](docs/report/derp-rs-report.tex) 和
[`output/pdf/derp-rs-project-report.pdf`](output/pdf/derp-rs-project-report.pdf)。

## 设计

- Tokio 多线程运行时，每连接独立读写任务。
- `DashMap` 活跃连接热路径，重复连接控制面单独加锁。
- `bytes::Bytes` 引用计数载荷，避免本地转发复制。
- 64 KiB 聚合写缓冲，一次最多批量编码 64 帧。
- 每客户端有界 MPSC 队列，原子 Prometheus 计数器。
- rustls TLS，持久 NodeKey，常量时间 Mesh PSK 比较。

协议细节和兼容基线见 [`docs/PROTOCOL.md`](docs/PROTOCOL.md)。

## License

BSD-3-Clause。Tailscale 的名称和商标归其权利人所有。
