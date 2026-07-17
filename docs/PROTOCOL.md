# DERP v2 compatibility notes

This implementation was independently written against the public Tailscale
DERP documentation and source. The compatibility baseline inspected during
development was Tailscale main commit
`cfd101f9d773695def27a5f6289fc25ac36ac991` (2026-07-16); the executable used
for stable-client and performance validation was v1.100.0.

Primary references:

- <https://pkg.go.dev/tailscale.com/derp>
- <https://pkg.go.dev/tailscale.com/derp/derpserver>
- <https://github.com/tailscale/tailscale/blob/main/derp/derp.go>
- <https://github.com/tailscale/tailscale/blob/main/derp/derpserver/derpserver.go>
- <https://github.com/tailscale/tailscale/blob/main/net/stun/stun.go>
- <https://github.com/tailscale/tailscale/tree/main/cmd/derper>

## Connection sequence

1. The client performs a native `Upgrade: DERP`, DERP Fast Start, or a
   WebSocket upgrade with subprotocol `derp`.
2. The server sends `FrameServerKey`, containing `DERP🔑` and its 32-byte key.
3. The client sends its NodeKey plus a NaCl box containing `ClientInfo`.
4. The server authenticates and optionally admits the client.
5. The server replies with an authenticated `ServerInfo` box.
6. Both ends exchange length-delimited frames.

Protocol v2 receive frames include the source NodeKey. Unknown future frame
types are consumed and counted without desynchronizing the stream.

## Frame matrix

| Value | Frame | Direction |
| ---: | --- | --- |
| `0x01` | ServerKey | server → client |
| `0x02` | ClientInfo | client → server |
| `0x03` | ServerInfo | server → client |
| `0x04` | SendPacket | client → server |
| `0x05` | RecvPacket | server → client |
| `0x06` | KeepAlive | server → client |
| `0x07` | NotePreferred | client → server |
| `0x08` | PeerGone | server → client |
| `0x09` | PeerPresent | server → mesh |
| `0x0a` | ForwardPacket | mesh → server |
| `0x10` | WatchConns | mesh → server |
| `0x11` | ClosePeer | mesh → server |
| `0x12` | Ping | either direction |
| `0x13` | Pong | either direction |
| `0x14` | Health | server → client |
| `0x15` | Restarting | server → client |

Packet size is capped at 64 KiB. Client info and general frame limits are
separate to prevent attacker-controlled allocations.

## Routing behavior

For each NodeKey, the last active connection receives packets. Duplicate
connections receive a non-empty Health frame; when only one remains, an empty
Health frame clears the condition. Recent activity selects the active duplicate.

A successful A → B send records a reverse path. When A's final connection
leaves the server, B receives `PeerGone(Disconnected)`. An absent destination
only causes `PeerGone(NotHere)` for a valid-looking Tailscale disco wrapper,
rate-limited to three responses per second per connection, matching the
official behavior.

Mesh connections authenticate with the shared key carried in encrypted
`ClientInfo`. They subscribe with `WatchConns`, receive presence changes, and
carry `ForwardPacket` frames. Already-forwarded packets are never forwarded a
second time.

Interoperability was exercised with a Rust server meshed directly to the
official v1.100.0 Go `derper`; official Go clients connected to opposite
servers successfully exchanged a packet with the original source NodeKey.

## STUN

The UDP listener accepts RFC 5389 Binding Requests only when:

- the magic cookie and framing are valid;
- `SOFTWARE` is exactly `tailnode`;
- the final attribute is a valid FINGERPRINT.

It returns an IPv4 or IPv6 `XOR-MAPPED-ADDRESS`, using the transaction ID for
the IPv6 mask exactly as the official Tailscale STUN implementation does.
