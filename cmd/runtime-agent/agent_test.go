package main

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// fakeHost is a test stand-in for the host control receiver. It listens on a local
// TCP port; when the supervisor dials in and sends Hello, it validates the token and
// drives commands on that connection. This mirrors the reverse-connect topology
// without docker — the supervisor logic (fork/stream/signal) is what's exercised.
type fakeHost struct {
	ln       net.Listener
	token    string
	mu       sync.Mutex
	conn     net.Conn
	enc      *json.Encoder
	r        *bufio.Reader
	hello    Hello
	reqSeq   int64
	frames   chan Frame // every frame the supervisor sends, fanned out to tests
}

func newFakeHost(t *testing.T, token string) *fakeHost {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	h := &fakeHost{ln: ln, token: token, frames: make(chan Frame, 1024)}
	t.Cleanup(func() { ln.Close(); if h.conn != nil { h.conn.Close() } })
	return h
}

func (h *fakeHost) addr() string { return h.ln.Addr().String() }

// accept waits for the supervisor to dial in, completes the Hello handshake, and
// starts reading frames into h.frames. Returns the Hello it received.
func (h *fakeHost) accept(t *testing.T) Hello {
	t.Helper()
	conn, err := h.ln.Accept()
	if err != nil {
		t.Fatal(err)
	}
	h.conn = conn
	h.enc = json.NewEncoder(conn)
	h.r = bufio.NewReader(conn)
	line, err := h.r.ReadBytes('\n')
	if err != nil {
		t.Fatalf("read hello: %v", err)
	}
	if err := json.Unmarshal(line, &h.hello); err != nil {
		t.Fatalf("bad hello: %v", err)
	}
	ok := h.token == "" || h.hello.Token == h.token
	_ = h.enc.Encode(HelloAck{OK: ok, Error: errIf(!ok, "unauthorized")})
	if !ok {
		conn.Close()
		return h.hello
	}
	go func() {
		for {
			b, err := h.r.ReadBytes('\n')
			if len(b) > 0 {
				var f Frame
				if json.Unmarshal(b, &f) == nil {
					h.frames <- f
				}
			}
			if err != nil {
				return
			}
		}
	}()
	return h.hello
}

func (h *fakeHost) send(t *testing.T, req Request) int64 {
	t.Helper()
	h.mu.Lock()
	h.reqSeq++
	req.ReqID = h.reqSeq
	id := req.ReqID
	err := h.enc.Encode(req)
	h.mu.Unlock()
	if err != nil {
		t.Fatalf("send: %v", err)
	}
	return id
}

// waitFrame blocks for a frame matching pred (or fails after timeout).
func (h *fakeHost) waitFrame(t *testing.T, pred func(Frame) bool, timeout time.Duration) Frame {
	t.Helper()
	deadline := time.After(timeout)
	for {
		select {
		case f := <-h.frames:
			if pred(f) {
				return f
			}
		case <-deadline:
			t.Fatal("timed out waiting for frame")
		}
	}
}

func errIf(c bool, s string) string {
	if c {
		return s
	}
	return ""
}

// startSupervisorDialing runs a supervisor that dials the fake host in a background
// goroutine (the dial blocks until the host accepts). Safe to call as `go
// startSupervisorDialing(...)`: it uses t.Errorf (goroutine-safe) not t.Fatal.
func startSupervisorDialing(t *testing.T, host *fakeHost, runID, token string) {
	t.Helper()
	ws := filepath.Join(t.TempDir(), "workspace")
	if err := os.MkdirAll(ws, 0o755); err != nil {
		t.Errorf("mkdir workspace: %v", err)
		return
	}
	s := &supervisor{runID: runID, token: token, workspace: ws, workers: map[string]*worker{}}
	conn := s.dialHost(host.addr(), 5*time.Second)
	if conn == nil {
		t.Errorf("supervisor could not dial fake host")
		return
	}
	s.serve(conn)
}

func TestHelloHandshakeAndStartWorker(t *testing.T) {
	host := newFakeHost(t, "tok123")
	// supervisor dials in a goroutine; host accepts.
	go startSupervisorDialing(t, host, "run-A", "tok123")
	hello := host.accept(t)
	if hello.RunID != "run-A" || hello.Token != "tok123" {
		t.Fatalf("bad hello: %+v", hello)
	}

	// StartWorker: echo two lines.
	reqID := host.send(t, Request{Op: OpStartWorker, Spec: &WorkerSpec{
		Argv: []string{"sh", "-c", "echo hello-stream; echo line2"}, Cwd: "/tmp", TimeoutSec: 10,
	}})
	started := host.waitFrame(t, func(f Frame) bool { return f.T == "started" && f.ReqID == reqID }, 5*time.Second)
	if started.WorkerID == "" || started.Error != "" {
		t.Fatalf("bad started: %+v", started)
	}
	wid := started.WorkerID

	var gotHello, gotLine2, gotExit bool
	deadline := time.After(8 * time.Second)
	for !gotExit {
		select {
		case f := <-host.frames:
			if f.WorkerID != wid {
				continue
			}
			switch f.T {
			case "out":
				if f.Line == "hello-stream" {
					gotHello = true
				}
				if f.Line == "line2" {
					gotLine2 = true
				}
			case "exit":
				gotExit = true
				if f.Rc != 0 {
					t.Fatalf("bad exit rc=%d", f.Rc)
				}
			}
		case <-deadline:
			t.Fatal("no exit frame")
		}
	}
	if !gotHello || !gotLine2 {
		t.Fatalf("missing stdout lines hello=%v line2=%v", gotHello, gotLine2)
	}
}

func TestSignalKill(t *testing.T) {
	host := newFakeHost(t, "")
	go startSupervisorDialing(t, host, "run-K", "")
	host.accept(t)

	reqID := host.send(t, Request{Op: OpStartWorker, Spec: &WorkerSpec{
		Argv: []string{"sh", "-c", "echo started; sleep 60"}, Cwd: "/tmp", TimeoutSec: 120,
	}})
	started := host.waitFrame(t, func(f Frame) bool { return f.T == "started" && f.ReqID == reqID }, 5*time.Second)
	wid := started.WorkerID
	host.waitFrame(t, func(f Frame) bool { return f.WorkerID == wid && f.T == "out" && f.Line == "started" }, 5*time.Second)

	// KILL it.
	host.send(t, Request{Op: OpSignal, WorkerID: wid, Signal: "KILL"})

	exit := host.waitFrame(t, func(f Frame) bool { return f.WorkerID == wid && f.T == "exit" }, 6*time.Second)
	if exit.Signalled != 9 {
		t.Fatalf("expected SIGKILL(9), got signalled=%d rc=%d", exit.Signalled, exit.Rc)
	}
}

func TestStopContPauseResume(t *testing.T) {
	host := newFakeHost(t, "")
	go startSupervisorDialing(t, host, "run-P", "")
	host.accept(t)

	reqID := host.send(t, Request{Op: OpStartWorker, Spec: &WorkerSpec{
		Argv: []string{"sh", "-c", "echo up; sleep 30"}, Cwd: "/tmp", TimeoutSec: 60,
	}})
	started := host.waitFrame(t, func(f Frame) bool { return f.T == "started" && f.ReqID == reqID }, 5*time.Second)
	wid := started.WorkerID
	host.waitFrame(t, func(f Frame) bool { return f.WorkerID == wid && f.T == "out" }, 5*time.Second)

	check := func(sig string, wantPaused bool) {
		host.send(t, Request{Op: OpSignal, WorkerID: wid, Signal: sig})
		host.waitFrame(t, func(f Frame) bool { return f.T == "resp" && f.OK }, 3*time.Second)
		statReq := host.send(t, Request{Op: OpStatus, WorkerID: wid})
		st := host.waitFrame(t, func(f Frame) bool { return f.T == "resp" && f.ReqID == statReq }, 3*time.Second)
		if st.Paused != wantPaused {
			t.Fatalf("after %s: paused=%v want %v (state=%s)", sig, st.Paused, wantPaused, st.State)
		}
	}
	check("STOP", true)
	check("CONT", false)
	host.send(t, Request{Op: OpSignal, WorkerID: wid, Signal: "KILL"})
}

func TestHealth(t *testing.T) {
	host := newFakeHost(t, "")
	go startSupervisorDialing(t, host, "run-H", "")
	host.accept(t)
	reqID := host.send(t, Request{Op: OpHealth})
	f := host.waitFrame(t, func(f Frame) bool { return f.T == "resp" && f.ReqID == reqID }, 3*time.Second)
	if !f.OK || f.Version != agentVersion {
		t.Fatalf("bad health: %+v", f)
	}
}

func TestTokenRejected(t *testing.T) {
	host := newFakeHost(t, "right")
	// supervisor dials with the WRONG token → host rejects in accept().
	done := make(chan struct{})
	go func() {
		ws := filepath.Join(t.TempDir(), "ws")
		_ = os.MkdirAll(ws, 0o755)
		s := &supervisor{runID: "run-X", token: "wrong", workspace: ws, workers: map[string]*worker{}}
		conn := s.dialHost(host.addr(), 3*time.Second)
		if conn != nil {
			t.Errorf("dial should have failed on bad token")
		}
		close(done)
	}()
	hello := host.accept(t)
	if hello.Token != "wrong" {
		t.Fatalf("expected wrong token in hello, got %q", hello.Token)
	}
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("supervisor did not abort on rejected hello")
	}
}

func TestSeedWorkspaceDocsIdempotent(t *testing.T) {
	ws := filepath.Join(t.TempDir(), "workspace")
	if err := os.MkdirAll(ws, 0o755); err != nil {
		t.Fatal(err)
	}
	dst := filepath.Join(ws, "CLAUDE.md")
	if err := os.WriteFile(dst, []byte("worker-edited"), 0o644); err != nil {
		t.Fatal(err)
	}
	s := &supervisor{workspace: ws, workers: map[string]*worker{}}
	s.seedWorkspaceDocs() // /opt/muteki absent → must NOT clobber dst
	got, _ := os.ReadFile(dst)
	if string(got) != "worker-edited" {
		t.Fatalf("seed clobbered worker-edited file: %q", got)
	}
}
