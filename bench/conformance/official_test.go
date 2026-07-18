// Copyright (c) 2026 derp-rs contributors
// SPDX-License-Identifier: BSD-3-Clause
//
// These black-box tests adapt the public Tailscale DERP tests so they can run
// against an external server. The protocol expectations come from:
//
//	derp/derp_test.go
//	derp/derphttp/derphttp_test.go
//	derp/derpserver/derpserver_test.go
//	net/stun/stun_test.go
package conformance

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"tailscale.com/derp"
	"tailscale.com/derp/derphttp"
	"tailscale.com/net/netmon"
	"tailscale.com/net/stun"
	"tailscale.com/types/key"
)

const defaultMeshKey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

var netMon = netmon.NewStatic()

func serverURL(t *testing.T) string {
	t.Helper()
	v := os.Getenv("DERP_URL")
	if v == "" {
		t.Skip("DERP_URL is not set; run scripts/official-conformance.sh")
	}
	return strings.TrimRight(v, "/")
}

func newClient(t *testing.T, private key.NodePrivate, configure func(*derphttp.Client)) *derphttp.Client {
	t.Helper()
	c, err := derphttp.NewClient(private, serverURL(t)+"/derp", t.Logf, netMon)
	if err != nil {
		t.Fatal(err)
	}
	if configure != nil {
		configure(c)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Connect(ctx); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	m, err := recvTimeout(c, 5*time.Second)
	if err != nil {
		c.Close()
		t.Fatalf("first Recv: %v", err)
	}
	if _, ok := m.(derp.ServerInfoMessage); !ok {
		c.Close()
		t.Fatalf("first Recv type = %T, want derp.ServerInfoMessage", m)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

type recvResult struct {
	message derp.ReceivedMessage
	err     error
}

func recvTimeout(c *derphttp.Client, timeout time.Duration) (derp.ReceivedMessage, error) {
	ch := make(chan recvResult, 1)
	go func() {
		m, err := c.Recv()
		if p, ok := m.(derp.ReceivedPacket); ok {
			p.Data = bytes.Clone(p.Data)
			m = p
		}
		ch <- recvResult{m, err}
	}()
	select {
	case result := <-ch:
		return result.message, result.err
	case <-time.After(timeout):
		return nil, context.DeadlineExceeded
	}
}

func wantPacket(t *testing.T, c *derphttp.Client, source key.NodePublic, data []byte) {
	t.Helper()
	m, err := recvTimeout(c, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	p, ok := m.(derp.ReceivedPacket)
	if !ok {
		t.Fatalf("Recv type = %T, want derp.ReceivedPacket", m)
	}
	if p.Source != source || !bytes.Equal(p.Data, data) {
		t.Fatalf("Recv = {%v, %q}, want {%v, %q}", p.Source, p.Data, source, data)
	}
}

func TestOfficialDERPHTTPClientSendRecv(t *testing.T) {
	// Adapted from derp/derphttp.TestSendRecv and derp.TestSendRecv.
	privA, privB, privC := key.NewNode(), key.NewNode(), key.NewNode()
	a := newClient(t, privA, nil)
	b := newClient(t, privB, nil)
	c := newClient(t, privC, nil)

	ab := []byte("official test: A -> B")
	if err := a.Send(privB.Public(), ab); err != nil {
		t.Fatal(err)
	}
	wantPacket(t, b, privA.Public(), ab)

	bc := []byte("official test: B -> C")
	if err := b.Send(privC.Public(), bc); err != nil {
		t.Fatal(err)
	}
	wantPacket(t, c, privB.Public(), bc)

	// A reverse-path recipient must learn when its packet source disconnects.
	if err := a.Close(); err != nil {
		t.Fatal(err)
	}
	m, err := recvTimeout(b, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	gone, ok := m.(derp.PeerGoneMessage)
	if !ok || gone.Peer != privA.Public() || gone.Reason != derp.PeerGoneReasonDisconnected {
		t.Fatalf("Recv = %#v, want PeerGone(A, Disconnected)", m)
	}
}

func TestOfficialPeerNotHereSemantics(t *testing.T) {
	// Normal packets to an unknown peer are deliberately silent.
	normal := newClient(t, key.NewNode(), nil)
	if err := normal.Send(key.NewNode().Public(), []byte("not a disco packet")); err != nil {
		t.Fatal(err)
	}
	if m, err := recvTimeout(normal, 250*time.Millisecond); err == nil {
		t.Fatalf("unexpected response to normal unknown-destination packet: %#v", m)
	}
	normal.Close() // unblock the timed-out Recv goroutine

	// Disco wrappers get PeerGone(NotHere), limited to an initial burst of 3.
	sender := newClient(t, key.NewNode(), nil)
	missing := key.NewNode().Public()
	disco := append([]byte("TS\xF0\x9F\x92\xAC"), make([]byte, 32+24)...)
	for range 6 {
		if err := sender.Send(missing, disco); err != nil {
			t.Fatal(err)
		}
	}
	for i := range 3 {
		m, err := recvTimeout(sender, time.Second)
		if err != nil {
			t.Fatalf("PeerGone %d: %v", i+1, err)
		}
		gone, ok := m.(derp.PeerGoneMessage)
		if !ok || gone.Peer != missing || gone.Reason != derp.PeerGoneReasonNotHere {
			t.Fatalf("Recv = %#v, want PeerGone(missing, NotHere)", m)
		}
	}
	if m, err := recvTimeout(sender, 250*time.Millisecond); err == nil {
		t.Fatalf("rate limiter allowed more than the official initial burst: %#v", m)
	}
	sender.Close()
}

func TestOfficialPingPong(t *testing.T) {
	// Adapted from derp.TestServerRepliesToPing and derphttp.TestPing.
	c := newClient(t, key.NewNode(), nil)
	want := [8]byte{0, 1, 2, 3, 4, 5, 6, 7}
	if err := c.SendPing(want); err != nil {
		t.Fatal(err)
	}
	m, err := recvTimeout(c, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	got, ok := m.(derp.PongMessage)
	if !ok || [8]byte(got) != want {
		t.Fatalf("Recv = %#v, want Pong(%v)", m, want)
	}
}

func TestOfficialWatchConnectionChanges(t *testing.T) {
	// Adapted from derp.TestWatch.
	meshText := os.Getenv("DERP_MESH_PSK")
	if meshText == "" {
		meshText = defaultMeshKey
	}
	meshKey, err := key.ParseDERPMesh(meshText)
	if err != nil {
		t.Fatal(err)
	}
	watcherPriv := key.NewNode()
	watcher := newClient(t, watcherPriv, func(c *derphttp.Client) {
		c.MeshKey = meshKey
		c.WatchConnectionChanges = true
	})

	wantPresent(t, watcher, map[key.NodePublic]derp.PeerPresentFlags{
		watcherPriv.Public(): derp.PeerPresentIsMeshPeer,
	})

	regularPriv := key.NewNode()
	regular := newClient(t, regularPriv, nil)
	wantPresent(t, watcher, map[key.NodePublic]derp.PeerPresentFlags{
		regularPriv.Public(): derp.PeerPresentIsRegular,
	})

	watcher2Priv := key.NewNode()
	watcher2 := newClient(t, watcher2Priv, func(c *derphttp.Client) {
		c.MeshKey = meshKey
		c.WatchConnectionChanges = true
	})
	wantPresent(t, watcher, map[key.NodePublic]derp.PeerPresentFlags{
		watcher2Priv.Public(): derp.PeerPresentIsMeshPeer,
	})
	wantPresent(t, watcher2, map[key.NodePublic]derp.PeerPresentFlags{
		watcherPriv.Public():  derp.PeerPresentIsMeshPeer,
		watcher2Priv.Public(): derp.PeerPresentIsMeshPeer,
		regularPriv.Public():  derp.PeerPresentIsRegular,
	})

	if err := regular.Close(); err != nil {
		t.Fatal(err)
	}
	wantGone(t, watcher, regularPriv.Public())
	wantGone(t, watcher2, regularPriv.Public())
}

func wantPresent(t *testing.T, c *derphttp.Client, want map[key.NodePublic]derp.PeerPresentFlags) {
	t.Helper()
	for len(want) != 0 {
		m, err := recvTimeout(c, 3*time.Second)
		if err != nil {
			t.Fatal(err)
		}
		present, ok := m.(derp.PeerPresentMessage)
		if !ok {
			t.Fatalf("Recv type = %T, want derp.PeerPresentMessage", m)
		}
		flags, exists := want[present.Key]
		if !exists {
			t.Fatalf("unexpected PeerPresent for %v", present.Key)
		}
		if present.Flags != flags {
			t.Fatalf("PeerPresent(%v) flags = %v, want %v", present.Key, present.Flags, flags)
		}
		if !present.IPPort.IsValid() {
			t.Fatalf("PeerPresent(%v) has invalid endpoint", present.Key)
		}
		delete(want, present.Key)
	}
}

func wantGone(t *testing.T, c *derphttp.Client, peer key.NodePublic) {
	t.Helper()
	m, err := recvTimeout(c, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	gone, ok := m.(derp.PeerGoneMessage)
	if !ok || gone.Peer != peer || gone.Reason != derp.PeerGoneReasonDisconnected {
		t.Fatalf("Recv = %#v, want PeerGone(%v, Disconnected)", m, peer)
	}
}

func TestOfficialDuplicateConnectionHealth(t *testing.T) {
	// Adapted from derpserver duplicate-client tests.
	priv := key.NewNode()
	first := newClient(t, priv, nil)
	second := newClient(t, priv, nil)

	for name, c := range map[string]*derphttp.Client{"first": first, "second": second} {
		m, err := recvTimeout(c, 3*time.Second)
		if err != nil {
			t.Fatalf("%s connection: %v", name, err)
		}
		health, ok := m.(derp.HealthMessage)
		if !ok || health.Problem == "" {
			t.Fatalf("%s connection got %#v, want non-empty Health", name, m)
		}
	}
	if err := second.Close(); err != nil {
		t.Fatal(err)
	}
	m, err := recvTimeout(first, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	health, ok := m.(derp.HealthMessage)
	if !ok || health.Problem != "" {
		t.Fatalf("remaining connection got %#v, want empty Health", m)
	}
}

func TestOfficialNotePreferredMetric(t *testing.T) {
	// Mirrors derp.TestSendRecv's preferred/home-node state transitions.
	url := serverURL(t)
	baseline := metricValue(t, url, "derp_preferred_clients")
	c := newClient(t, key.NewNode(), nil)

	c.NotePreferred(true)
	waitMetric(t, url, "derp_preferred_clients", baseline+1)
	c.NotePreferred(true)
	waitMetric(t, url, "derp_preferred_clients", baseline+1)
	c.NotePreferred(false)
	waitMetric(t, url, "derp_preferred_clients", baseline)
	c.NotePreferred(true)
	waitMetric(t, url, "derp_preferred_clients", baseline+1)
	c.Close()
	waitMetric(t, url, "derp_preferred_clients", baseline)
}

func metricValue(t *testing.T, baseURL, name string) int64 {
	t.Helper()
	resp, err := http.Get(baseURL + "/metrics")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	for _, line := range strings.Split(string(body), "\n") {
		fields := strings.Fields(line)
		if len(fields) == 2 && fields[0] == name {
			value, err := strconv.ParseInt(fields[1], 10, 64)
			if err != nil {
				t.Fatal(err)
			}
			return value
		}
	}
	t.Fatalf("metric %q not found", name)
	return 0
}

func waitMetric(t *testing.T, url, name string, want int64) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		got := metricValue(t, url, name)
		if got == want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("%s = %d, want %d", name, got, want)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestOfficialHTTPHandlers(t *testing.T) {
	// Adapted from derphttp.TestProbe and derpserver.ServeNoContent tests.
	base := serverURL(t)
	for _, tc := range []struct {
		method string
		path   string
		want   int
	}{
		{"GET", "/derp/probe", http.StatusOK},
		{"HEAD", "/derp/latency-check", http.StatusOK},
		{"POST", "/derp/probe", http.StatusMethodNotAllowed},
		{"GET", "/derp/sdf", http.StatusUpgradeRequired},
	} {
		req, err := http.NewRequest(tc.method, base+tc.path, nil)
		if err != nil {
			t.Fatal(err)
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != tc.want {
			t.Errorf("%s %s = %d, want %d", tc.method, tc.path, resp.StatusCode, tc.want)
		}
	}

	req, err := http.NewRequest("GET", base+"/generate_204", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("X-Tailscale-Challenge", "official-test_123")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusNoContent {
		t.Fatalf("/generate_204 = %d, want 204", resp.StatusCode)
	}
	if got, want := resp.Header.Get("X-Tailscale-Response"), "response official-test_123"; got != want {
		t.Fatalf("X-Tailscale-Response = %q, want %q", got, want)
	}
}

func TestOfficialSTUNRequestResponse(t *testing.T) {
	// Uses the official generator and parser from net/stun.
	server := os.Getenv("DERP_STUN_ADDR")
	if server == "" {
		t.Skip("DERP_STUN_ADDR is not set; run scripts/official-conformance.sh")
	}
	remote, err := net.ResolveUDPAddr("udp", server)
	if err != nil {
		t.Fatal(err)
	}
	conn, err := net.DialUDP("udp", nil, remote)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	if err := conn.SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	tx := stun.NewTxID()
	if _, err := conn.Write(stun.Request(tx)); err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, 2048)
	n, err := conn.Read(buf)
	if err != nil {
		t.Fatal(err)
	}
	gotTx, gotAddr, err := stun.ParseResponse(buf[:n])
	if err != nil {
		t.Fatal(err)
	}
	if gotTx != tx {
		t.Fatalf("transaction ID = %x, want %x", gotTx, tx)
	}
	wantAddr, err := netip.ParseAddrPort(conn.LocalAddr().String())
	if err != nil {
		t.Fatal(err)
	}
	if gotAddr != wantAddr {
		t.Fatalf("mapped address = %v, want %v", gotAddr, wantAddr)
	}
}

func TestCommunityRustscaleBidirectionalRelay(t *testing.T) {
	// Adapted from rustscale crates/derp/server.rs:
	// two_clients_exchange_packets.
	privA, privB := key.NewNode(), key.NewNode()
	a := newClient(t, privA, nil)
	b := newClient(t, privB, nil)

	fromA := []byte("rustscale test: hello from A")
	if err := a.Send(privB.Public(), fromA); err != nil {
		t.Fatal(err)
	}
	wantPacket(t, b, privA.Public(), fromA)

	fromB := []byte("rustscale test: hello from B")
	if err := b.Send(privA.Public(), fromB); err != nil {
		t.Fatal(err)
	}
	wantPacket(t, a, privB.Public(), fromB)
}

func TestCommunityRustscaleNewestConnectionReceives(t *testing.T) {
	// rustscale calls this last_writer_wins. Its test server closes the older
	// connection, whereas the official server keeps duplicates alive and sends
	// Health frames. Preserve official duplicate semantics and test the
	// compatible routing assertion: traffic goes to the newest connection.
	duplicatePriv := key.NewNode()
	first := newClient(t, duplicatePriv, nil)
	second := newClient(t, duplicatePriv, nil)
	senderPriv := key.NewNode()
	sender := newClient(t, senderPriv, nil)

	for name, c := range map[string]*derphttp.Client{"first": first, "second": second} {
		m, err := recvTimeout(c, 3*time.Second)
		if err != nil {
			t.Fatalf("%s duplicate Health: %v", name, err)
		}
		if health, ok := m.(derp.HealthMessage); !ok || health.Problem == "" {
			t.Fatalf("%s got %#v, want duplicate Health", name, m)
		}
	}

	data := []byte("rustscale test: to newest connection")
	if err := sender.Send(duplicatePriv.Public(), data); err != nil {
		t.Fatal(err)
	}
	wantPacket(t, second, senderPriv.Public(), data)
	if m, err := recvTimeout(first, 250*time.Millisecond); err == nil {
		t.Fatalf("older duplicate unexpectedly received routed traffic: %#v", m)
	}
	first.Close()
}

func TestCommunityRustscaleNonFastStartUpgrade(t *testing.T) {
	// Adapted from rustscale crates/derp/server.rs:
	// non_fast_start_upgrade.
	u, err := url.Parse(serverURL(t))
	if err != nil {
		t.Fatal(err)
	}
	conn, err := net.DialTimeout("tcp", u.Host, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	if err := conn.SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	fmt.Fprintf(conn, "GET /derp HTTP/1.1\r\nHost: %s\r\nUpgrade: DERP\r\nConnection: Upgrade\r\n\r\n", u.Host)
	reader := bufio.NewReader(conn)
	status, err := reader.ReadString('\n')
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(status, "101 Switching Protocols") {
		t.Fatalf("status = %q, want 101 Switching Protocols", status)
	}
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			t.Fatal(err)
		}
		if line == "\r\n" {
			break
		}
	}
	frameType, frameLen, err := derp.ReadFrameHeader(reader)
	if err != nil {
		t.Fatal(err)
	}
	if frameType != derp.FrameServerKey {
		t.Fatalf("first frame = %#x, want FrameServerKey", frameType)
	}
	body := make([]byte, frameLen)
	if _, err := io.ReadFull(reader, body); err != nil {
		t.Fatal(err)
	}
	if !bytes.HasPrefix(body, []byte(derp.Magic)) || len(body) < len(derp.Magic)+derp.KeyLen {
		t.Fatalf("invalid ServerKey frame body")
	}
}

func TestEnvironmentIsExternalServer(t *testing.T) {
	// Give failures a clear target in CI logs.
	resp, err := http.Get(serverURL(t) + "/debug/check")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("external server health = %s: %s", resp.Status, body)
	}
	t.Logf("tested external server: %s", fmt.Sprintf("%s", bytes.TrimSpace(body)))
}
