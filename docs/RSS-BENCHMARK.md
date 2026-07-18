# Production-scale RSS comparison

Benchmark date: 2026-07-18.

The objective of this test is narrower and stronger than the loopback
throughput microbenchmark: compare steady and peak resident memory under a
defined production-like envelope. It does not claim a mathematical guarantee
for every kernel, allocator, future release, or workload.

Within the tested envelope—100 to 10,000 simultaneous connections, plaintext
and TLS, idle clients, sustained relay traffic, backpressured receivers, and
connection churn—derp-rs used less steady and peak RSS than the official Go
server in every case.

## Environment

- Tencent Cloud VM, Ubuntu, Linux 6.8 x86_64
- 2 vCPU, 1.92 GiB RAM
- File descriptor limit raised to 65,535 for the benchmark processes
- derp-rs built by GitHub Actions from the tested commit using the stable Rust
  release profile
- official `tailscale.com/cmd/derper@v1.100.0`
- load generator built against the official
  `tailscale.com/derp@v1.100.0` client
- 1,200-byte relay payloads
- TLS cases use the normal server TLS stacks with the same temporary RSA
  certificate; client certificate verification is disabled because the
  certificate is self-signed

Each case starts a fresh server. After the health probe and a two-second
warmup, baseline RSS is the mean of ten 100 ms samples. Steady RSS is the mean
of twenty 100 ms samples after the client reports that the requested state is
ready. Peak RSS is sampled every 50 ms for the lifetime of the load process.
On Linux the source is `/proc/<pid>/status` `VmRSS`.

Run the matrix with:

```bash
./scripts/rss-benchmark.sh

TLS_MODE=1 \
SCENARIOS="idle:100 idle:1000 idle:5000 idle:10000 active:1000 active:5000 slow:1000 slow:5000 churn:1000 churn:5000" \
./scripts/rss-benchmark.sh
```

## Workload definitions

- `idle`: clients complete the official DERP handshake and continue reading
  control frames without sending relay packets.
- `active`: clients form a ring and target an aggregate 10,000 packets/s while
  every receiver verifies delivered packets.
- `slow`: half the clients stop reading with a 4 KiB socket receive buffer;
  paired senders transmit 64 packets each, exercising queue bounds and blocked
  writers.
- `churn`: approximately 10% of live connections are closed and replaced every
  500 ms.

## Results

Selected steady RSS values:

| Transport | Workload | Connections | derp-rs | Go derper | Rust reduction |
| --- | --- | ---: | ---: | ---: | ---: |
| Plain | Idle | 1,000 | 15.9 MiB | 51.5 MiB | 69.17% |
| Plain | Idle | 10,000 | 111.4 MiB | 358.9 MiB | 68.96% |
| Plain | Active | 5,000 | 70.0 MiB | 204.1 MiB | 65.72% |
| Plain | Slow receiver | 5,000 | 111.9 MiB | 308.1 MiB | 63.69% |
| Plain | Churn | 5,000 | 58.3 MiB | 202.9 MiB | 71.26% |
| TLS | Idle | 1,000 | 24.0 MiB | 79.1 MiB | 69.67% |
| TLS | Idle | 10,000 | 184.9 MiB | 595.1 MiB | 68.93% |
| TLS | Active | 5,000 | 107.3 MiB | 324.7 MiB | 66.95% |
| TLS | Slow receiver | 5,000 | 156.8 MiB | 439.2 MiB | 64.30% |
| TLS | Churn | 5,000 | 96.0 MiB | 314.4 MiB | 69.47% |

Across the complete plaintext matrix, derp-rs steady RSS was 62.38% to 71.36%
lower and peak RSS was 62.38% to 79.85% lower. Across the complete TLS matrix,
steady RSS was 63.65% to 70.99% lower and peak RSS was 63.65% to 78.91% lower.

The 5,000-connection churn cases illustrate GC peak behavior. Plaintext peak
RSS was 58.6 MiB for Rust versus 290.8 MiB for Go. TLS peak RSS was 100.1 MiB
for Rust versus 391.4 MiB for Go.

The complete machine-readable results, including baseline RSS, incremental
bytes per connection, packet counts, and replacement counts, are in
[`bench/results/rss-linux-2026-07-18.csv`](../bench/results/rss-linux-2026-07-18.csv).

## Effect of the memory optimization

The optimized build was also compared on the same Linux machine with the
original commit `5fb943359cb321d28f98f3e22bc81d231b8d1968`.

| Transport | 5,000 connections | Original Rust | Optimized Rust | Reduction |
| --- | --- | ---: | ---: | ---: |
| Plain | Idle | 78.0 MiB | 58.6 MiB | 24.84% |
| Plain | Active | 90.2 MiB | 70.0 MiB | 22.40% |
| Plain | Slow receiver | 134.4 MiB | 111.9 MiB | 16.74% |
| TLS | Idle | 115.6 MiB | 95.6 MiB | 17.30% |
| TLS | Active | 127.0 MiB | 107.3 MiB | 15.51% |
| TLS | Slow receiver | 172.3 MiB | 156.8 MiB | 9.01% |

The optimized writer:

- allocates no batch buffer for a connection that has not received data;
- encodes multiple frames directly into one buffer instead of allocating a
  temporary payload and then copying it into a second buffer;
- grows with observed batches and releases a large buffer after 15 seconds
  without packet data;
- caps a single write batch at approximately 64 KiB;
- removes two redundant per-session atomic heap allocations;
- keeps the common single-session peer set inline.

The same change improved the five-run local throughput median from 313,906 to
366,442 packets/s while reducing active-connection memory, so the RSS reduction
was not purchased by lowering relay throughput.

## What this proves—and what it does not

The data supports this statement:

> On the tested Linux host, for every tested workload from 100 through 10,000
> simultaneous connections, including TLS, sustained relay traffic,
> backpressure, and churn, derp-rs used materially less steady and peak RSS than
> official derper v1.100.0.

It does not prove that Rust must use less RSS on every operating system,
allocator, architecture, future Go or Rust release, extension configuration,
packet distribution, or unbounded connection count. Re-run the checked-in
matrix on the intended host after runtime, dependency, or protocol changes.
