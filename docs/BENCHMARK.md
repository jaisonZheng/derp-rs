# Performance comparison

Benchmark date: 2026-07-17.

This comparison uses the repository's load generator, which imports the
official `tailscale.com/derp` client at v1.100.0. It checks that every sent
packet was received before advancing each bounded window, so a high number
cannot be produced by silently dropping queued packets.

## Environment

- Apple M3, macOS 15.6, arm64
- Rust 1.94.0, release profile with thin LTO and one codegen unit
- Go 1.26.x
- Rust server: derp-rs 0.1.0
- Go server: official `tailscale.com/cmd/derper@v1.100.0`
- Loopback TCP, DERP Fast Start, TLS and STUN disabled
- 16 clients in a ring
- 1,000 windows × 16 packets per client
- 1,200 payload bytes
- 256,000 verified packets per process run
- Five runs per implementation; reported throughput is the median

Command:

```bash
./scripts/benchmark.sh
```

The script builds both release servers and the official-client load generator,
then executes the same workload against each implementation.

## Results

| Server | packets/s runs | Median packets/s | Median payload | RSS sample |
| --- | --- | ---: | ---: | ---: |
| derp-rs 0.1.0 | 279,133; 263,318; 313,906; 355,714; 338,843 | 313,906 | 3.013 Gbit/s | 4,624 KiB |
| Tailscale derper v1.100.0 | 244,223; 248,927; 257,908; 259,970; 236,187 | 248,927 | 2.390 Gbit/s | 17,968 KiB |

In this run, derp-rs delivered 26.10% more packets per second at the median and
used 74.27% less sampled RSS (the Go process used 3.89× as much).

## Interpretation and limits

This is a local relay-throughput microbenchmark, not an Internet latency test.
Loopback emphasizes framing, routing, queues, allocation, scheduling, and
socket writes. WAN performance is normally constrained by path bandwidth and
RTT. RSS is sampled after a run and is not a peak-resident-memory measurement.

Results vary with thermals and background load; the raw runs are included so
the spread is visible. Use the script on the intended production machine and
run longer connection-count, slow-client, packet-loss, and TLS tests before
capacity planning.
