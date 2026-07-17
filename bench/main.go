package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"tailscale.com/derp"
	"tailscale.com/types/key"
)

var (
	addr    = flag.String("addr", "127.0.0.1:3340", "DERP server host:port")
	clients = flag.Int("clients", 16, "number of clients")
	rounds  = flag.Int("rounds", 400, "bounded-window rounds")
	batch   = flag.Int("batch", 16, "packets per client per round")
	size    = flag.Int("size", 1200, "packet bytes")
	fast    = flag.Bool("fast-start", true, "use DERP fast start")
)

type client struct {
	derp    *derp.Client
	private key.NodePrivate
	conn    net.Conn
}

func dial(addr string, fast bool) (*client, error) {
	conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		return nil, err
	}
	br, bw := bufio.NewReaderSize(conn, 64<<10), bufio.NewWriterSize(conn, 64<<10)
	fmt.Fprintf(bw, "GET /derp HTTP/1.1\r\nHost: %s\r\nConnection: Upgrade\r\nUpgrade: DERP\r\n", addr)
	if fast {
		fmt.Fprint(bw, "Derp-Fast-Start: 1\r\n")
	}
	fmt.Fprint(bw, "\r\n")
	if err := bw.Flush(); err != nil {
		return nil, err
	}
	if !fast {
		req, _ := http.NewRequest("GET", "http://"+addr+"/derp", nil)
		res, err := http.ReadResponse(br, req)
		if err != nil {
			return nil, err
		}
		if res.StatusCode != 101 {
			return nil, fmt.Errorf("upgrade: %s", res.Status)
		}
	}
	private := key.NewNode()
	dc, err := derp.NewClient(private, conn, bufio.NewReadWriter(br, bw), func(string, ...any) {}, derp.CanAckPings(true))
	if err != nil {
		return nil, err
	}
	if msg, err := dc.Recv(); err != nil {
		return nil, err
	} else if _, ok := msg.(derp.ServerInfoMessage); !ok {
		return nil, fmt.Errorf("first frame %T", msg)
	}
	return &client{dc, private, conn}, nil
}

func main() {
	flag.Parse()
	if *clients < 2 || *batch < 1 || *rounds < 1 || *size < 1 {
		log.Fatal("invalid arguments")
	}
	cs := make([]*client, *clients)
	for i := range cs {
		c, err := dial(*addr, *fast)
		if err != nil {
			log.Fatalf("dial client %d: %v", i, err)
		}
		cs[i] = c
	}
	var delivered atomic.Uint64
	errCh := make(chan error, *clients)
	var readers sync.WaitGroup
	for _, c := range cs {
		readers.Add(1)
		go func(c *client) {
			defer readers.Done()
			for {
				msg, err := c.derp.Recv()
				if err != nil {
					if err != io.EOF {
						errCh <- err
					}
					return
				}
				if _, ok := msg.(derp.ReceivedPacket); ok {
					delivered.Add(1)
				}
			}
		}(c)
	}
	payload := make([]byte, *size)
	for i := range payload {
		payload[i] = byte(i)
	}
	started := time.Now()
	var sent uint64
	for round := 0; round < *rounds; round++ {
		target := sent + uint64((*clients)*(*batch))
		var senders sync.WaitGroup
		senders.Add(*clients)
		for i, c := range cs {
			dst := cs[(i+1)%len(cs)].private.Public()
			go func(c *client) {
				defer senders.Done()
				for range *batch {
					if err := c.derp.Send(dst, payload); err != nil {
						errCh <- err
						return
					}
				}
			}(c)
		}
		senders.Wait()
		sent = target
		deadline := time.Now().Add(10 * time.Second)
		for delivered.Load() < target {
			select {
			case err := <-errCh:
				log.Fatal(err)
			default:
			}
			if time.Now().After(deadline) {
				log.Fatalf("delivery timeout: %d/%d", delivered.Load(), target)
			}
			time.Sleep(10 * time.Microsecond)
		}
	}
	elapsed := time.Since(started)
	result := map[string]any{"clients": *clients, "rounds": *rounds, "batch": *batch, "packet_bytes": *size, "packets": sent, "seconds": elapsed.Seconds(), "packets_per_second": float64(sent) / elapsed.Seconds(), "payload_gbps": float64(sent*uint64(*size)*8) / elapsed.Seconds() / 1e9}
	out, _ := json.Marshal(result)
	fmt.Println(string(out))
	for _, c := range cs {
		c.conn.Close()
	}
	readers.Wait()
}
