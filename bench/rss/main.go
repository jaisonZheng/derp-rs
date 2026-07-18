package main

import (
	"bufio"
	"context"
	"crypto/tls"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"tailscale.com/derp"
	"tailscale.com/types/key"
)

var (
	addr        = flag.String("addr", "127.0.0.1:3340", "DERP server host:port")
	clientCount = flag.Int("clients", 100, "number of connected clients")
	mode        = flag.String("mode", "idle", "idle, active, or slow")
	duration    = flag.Duration("duration", 15*time.Second, "steady-state duration")
	packetSize  = flag.Int("size", 1200, "packet payload bytes")
	pps         = flag.Int("pps", 10000, "aggregate packets/second in active mode; zero saturates")
	dialers     = flag.Int("dialers", 128, "parallel connection attempts")
	slowBurst   = flag.Int("slow-burst", 64, "packets sent to each slow client before steady state")
	useTLS      = flag.Bool("tls", false, "connect with TLS; test certificates are not verified")
	churnBatch  = flag.Int("churn-batch", 0, "connections replaced per churn cycle; zero uses 10%")
	churnEvery  = flag.Duration("churn-every", 500*time.Millisecond, "delay between churn cycles")
)

type client struct {
	derp    *derp.Client
	private key.NodePrivate
	conn    net.Conn
}

func dial(addr string) (*client, error) {
	dialer := net.Dialer{Timeout: 10 * time.Second}
	raw, err := dialer.Dial("tcp", addr)
	if err != nil {
		return nil, err
	}
	conn := raw
	if *useTLS {
		tlsConn := tls.Client(raw, &tls.Config{
			ServerName:         "localhost",
			InsecureSkipVerify: true, // Test-only self-signed certificate.
		})
		if err := tlsConn.Handshake(); err != nil {
			raw.Close()
			return nil, err
		}
		conn = tlsConn
	}
	br := bufio.NewReaderSize(conn, 64<<10)
	bw := bufio.NewWriterSize(conn, 64<<10)
	fmt.Fprintf(bw, "GET /derp HTTP/1.1\r\nHost: %s\r\nConnection: Upgrade\r\nUpgrade: DERP\r\nDerp-Fast-Start: 1\r\n\r\n", addr)
	if err := bw.Flush(); err != nil {
		conn.Close()
		return nil, err
	}
	private := key.NewNode()
	dc, err := derp.NewClient(private, conn, bufio.NewReadWriter(br, bw), func(string, ...any) {}, derp.CanAckPings(true))
	if err != nil {
		conn.Close()
		return nil, err
	}
	if msg, err := dc.Recv(); err != nil {
		conn.Close()
		return nil, err
	} else if _, ok := msg.(derp.ServerInfoMessage); !ok {
		conn.Close()
		return nil, fmt.Errorf("first frame %T, want ServerInfoMessage", msg)
	}
	return &client{derp: dc, private: private, conn: conn}, nil
}

func connectAll(n int) ([]*client, error) {
	clients := make([]*client, n)
	jobs := make(chan int)
	errs := make(chan error, 1)
	var wg sync.WaitGroup
	workers := min(*dialers, n)
	for range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range jobs {
				c, err := dial(*addr)
				if err != nil {
					select {
					case errs <- fmt.Errorf("client %d: %w", i, err):
					default:
					}
					continue
				}
				clients[i] = c
			}
		}()
	}
	for i := range n {
		jobs <- i
	}
	close(jobs)
	wg.Wait()
	select {
	case err := <-errs:
		for _, c := range clients {
			if c != nil {
				c.conn.Close()
			}
		}
		return nil, err
	default:
	}
	return clients, nil
}

func startReaders(clients []*client, delivered *atomic.Uint64) {
	for _, c := range clients {
		go func() {
			for {
				msg, err := c.derp.Recv()
				if err != nil {
					return
				}
				if _, ok := msg.(derp.ReceivedPacket); ok {
					delivered.Add(1)
				}
			}
		}()
	}
}

func runActive(ctx context.Context, clients []*client, payload []byte, sent *atomic.Uint64) {
	workers := min(32, len(clients))
	if *pps > 0 {
		workers = min(workers, *pps)
	}
	var wg sync.WaitGroup
	for worker := range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			i := worker
			var ticker *time.Ticker
			if *pps > 0 {
				interval := time.Duration(float64(time.Second) * float64(workers) / float64(*pps))
				ticker = time.NewTicker(max(interval, time.Microsecond))
				defer ticker.Stop()
			}
			for {
				if ticker != nil {
					select {
					case <-ctx.Done():
						return
					case <-ticker.C:
					}
				} else {
					select {
					case <-ctx.Done():
						return
					default:
					}
				}
				src := clients[i%len(clients)]
				dst := clients[(i+1)%len(clients)].private.Public()
				if err := src.derp.Send(dst, payload); err != nil {
					return
				}
				sent.Add(1)
				i += workers
			}
		}()
	}
	wg.Wait()
}

func runSlow(clients []*client, payload []byte, sent *atomic.Uint64) error {
	if len(clients) < 4 {
		return fmt.Errorf("slow mode needs at least four clients")
	}
	pairs := len(clients) / 2
	for _, destination := range clients[pairs:] {
		if tcp, ok := destination.conn.(*net.TCPConn); ok {
			if err := tcp.SetReadBuffer(4 << 10); err != nil {
				return err
			}
		}
	}
	errs := make(chan error, pairs)
	var wg sync.WaitGroup
	for i := range pairs {
		wg.Add(1)
		go func() {
			defer wg.Done()
			source := clients[i]
			destination := clients[pairs+i]
			for range *slowBurst {
				if err := source.derp.Send(destination.private.Public(), payload); err != nil {
					errs <- err
					return
				}
				sent.Add(1)
			}
		}()
	}
	wg.Wait()
	close(errs)
	return <-errs
}

func runChurn(clients []*client, delivered *atomic.Uint64) (uint64, error) {
	batchSize := *churnBatch
	if batchSize == 0 {
		batchSize = max(1, len(clients)/10)
	}
	batchSize = min(batchSize, len(clients))
	deadline := time.Now().Add(*duration)
	offset := 0
	var replaced uint64
	for time.Now().Before(deadline) {
		indices := make([]int, batchSize)
		for i := range batchSize {
			index := (offset + i) % len(clients)
			indices[i] = index
			clients[index].conn.Close()
		}
		replacements, err := connectAll(batchSize)
		if err != nil {
			return replaced, err
		}
		for i, index := range indices {
			clients[index] = replacements[i]
		}
		startReaders(replacements, delivered)
		replaced += uint64(batchSize)
		offset = (offset + batchSize) % len(clients)
		if remaining := time.Until(deadline); remaining > 0 {
			time.Sleep(min(*churnEvery, remaining))
		}
	}
	return replaced, nil
}

func main() {
	flag.Parse()
	if *clientCount < 2 || *packetSize < 1 || *duration <= 0 {
		log.Fatal("invalid arguments")
	}
	switch *mode {
	case "idle", "active", "slow", "churn":
	default:
		log.Fatalf("unknown mode %q", *mode)
	}
	clients, err := connectAll(*clientCount)
	if err != nil {
		log.Fatal(err)
	}
	defer func() {
		for _, c := range clients {
			c.conn.Close()
		}
	}()

	payload := make([]byte, *packetSize)
	for i := range payload {
		payload[i] = byte(i)
	}
	var sent, delivered atomic.Uint64
	var replaced uint64
	switch *mode {
	case "idle":
		startReaders(clients, &delivered)
		fmt.Printf("READY mode=%s clients=%d\n", *mode, len(clients))
		time.Sleep(*duration)
	case "active":
		startReaders(clients, &delivered)
		fmt.Printf("READY mode=%s clients=%d\n", *mode, len(clients))
		ctx, cancel := context.WithTimeout(context.Background(), *duration)
		runActive(ctx, clients, payload, &sent)
		cancel()
	case "slow":
		startReaders(clients[:len(clients)/2], &delivered)
		if err := runSlow(clients, payload, &sent); err != nil {
			log.Fatal(err)
		}
		fmt.Printf("READY mode=%s clients=%d\n", *mode, len(clients))
		time.Sleep(*duration)
	case "churn":
		startReaders(clients, &delivered)
		fmt.Printf("READY mode=%s clients=%d\n", *mode, len(clients))
		replaced, err = runChurn(clients, &delivered)
		if err != nil {
			log.Fatal(err)
		}
	}

	result := map[string]any{
		"mode":        *mode,
		"clients":     len(clients),
		"duration_s":  duration.Seconds(),
		"sent":        sent.Load(),
		"delivered":   delivered.Load(),
		"packet_size": *packetSize,
		"replaced":    replaced,
	}
	if err := json.NewEncoder(os.Stdout).Encode(result); err != nil && err != io.ErrClosedPipe {
		log.Fatal(err)
	}
}
