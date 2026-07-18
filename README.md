# derp-rs

`derp-rs` 是一个面向生产环境的高性能 Rust DERP 中继服务器。它实现了
Tailscale DERP v2 线协议、Fast Start、WebSocket-DERP、STUN、区域 Mesh、
官方运维 HTTP 端点、客户端准入、限速和优雅重启。

DERP 只中继由节点端到端加密的数据包，服务器无法解密 WireGuard 流量。

> 本项目是独立实现，不是 Tailscale 官方产品。部署前请自行进行安全审计和容量验证。

## 功能

| 能力 | 状态 |
| --- | --- |
| DERP v2 握手，NaCl `crypto_box` 身份认证 | 完整 |
| 所有官方帧类型 `0x01..0x15` | 完整 |
| 标准 HTTP Upgrade 与 `Derp-Fast-Start: 1` | 完整 |
| WebSocket，`derp` 子协议 | 完整 |
| 点对点转发、源 NodeKey、反向路径失效通知 | 完整 |
| disco `PeerGone(NotHere)` 检测及官方 3 次/秒限速 | 完整 |
| 重复连接健康状态、活跃连接切换 | 完整 |
| `Ping`/`Pong`、KeepAlive、`FrameRestarting` | 完整 |
| 区域 Mesh：WatchConns、PeerPresent/Gone、ForwardPacket、ClosePeer | 完整 |
| Tailscale STUN 请求校验、IPv4/IPv6 XOR-MAPPED-ADDRESS | 完整 |
| `/derp/probe`、`/derp/latency-check`、`/generate_204` | 完整 |
| `/bootstrap-dns`、`/metrics`、`/debug/vars`、`/debug/check` | 完整 |
| PEM TLS、Admission Controller、流量令牌桶、背压队列 | 完整 |
| 官方 Go DERP 客户端和官方 derper Mesh 交叉验证 | 通过 |

以下属于官方 `cmd/derper` 的平台集成或实验性能力，而不是 DERP 线协议：
本实现使用手工提供的 PEM 证书，不内置 ACME/GCP 证书管理；准入使用通用
HTTP Admission Controller，不直接调用本机 `tailscaled` 的 `WhoIsNodeKey`；
bootstrap DNS 使用静态 JSON；没有 Linux XDP 或实验中的 ACE proxy。

协议行为以 Tailscale
[`derp`](https://pkg.go.dev/tailscale.com/derp)、
[`derpserver`](https://pkg.go.dev/tailscale.com/derp/derpserver) 和
[`cmd/derper`](https://github.com/tailscale/tailscale/tree/main/cmd/derper)
为基准。实现时复核的上游主线提交为
`cfd101f9d773695def27a5f6289fc25ac36ac991`。

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

同机五轮中位数（Apple M3，16 个官方 Go DERP 客户端，1200 字节载荷，
每轮 256,000 包）：

| 服务端 | 包/秒中位数 | 有效载荷 | RSS |
| --- | ---: | ---: | ---: |
| derp-rs 0.1.0 | 366,442 | 3.518 Gbit/s | 4,704 KiB |
| Tailscale derper v1.100.0 | 263,363 | 2.528 Gbit/s | 22,208 KiB |

该环境下 Rust 中位吞吐高 39.14%，RSS 低 78.82%。完整参数、五轮原始数据、
方法和限制见 [`docs/BENCHMARK.md`](docs/BENCHMARK.md)。

生产规模 RSS 另在 2 vCPU Linux 主机上使用 100–10,000 个连接验证，覆盖
明文、TLS、持续转发、慢接收端和连接抖动。在全部已测场景中，Rust 稳态 RSS
低 62.38%–71.36%，峰值 RSS 低 62.38%–79.85%。完整原始数据、方法和结论边界
见 [`docs/RSS-BENCHMARK.md`](docs/RSS-BENCHMARK.md)。

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
