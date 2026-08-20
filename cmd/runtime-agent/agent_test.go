package main

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

// fakeHost is a test stand-in for the host control receiver. It listens on a local
// TCP port; when the supervisor dials in and sends Hello, it validates the token and
// drives commands on that connection. This mirrors the reverse-connect topology
// without docker — the supervisor logic (fork/stream/signal) is what's exercised.
type fakeHost struct {
	ln     net.Listener
	token  string
	mu     sync.Mutex
	conn   net.Conn
	enc    *json.Encoder
	r      *bufio.Reader
	hello  Hello
	reqSeq int64
	frames chan Frame // every frame the supervisor sends, fanned out to tests
}

func newFakeHost(t *testing.T, token string) *fakeHost {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	h := &fakeHost{ln: ln, token: token, frames: make(chan Frame, 1024)}
	t.Cleanup(func() {
		ln.Close()
		if h.conn != nil {
			h.conn.Close()
		}
	})
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

func waitWorkerExit(t *testing.T, frames <-chan Frame) Frame {
	t.Helper()
	timer := time.NewTimer(8 * time.Second)
	defer timer.Stop()
	for {
		select {
		case frame, ok := <-frames:
			if !ok {
				t.Fatal("worker event stream closed without exit frame")
			}
			if frame.T == "exit" {
				return frame
			}
		case <-timer.C:
			t.Fatal("timed out waiting for worker exit")
		}
	}
}

func TestSupervisorSIGCHLDKeepsManagedWaitStatusAuthoritative(t *testing.T) {
	// This installs the production signal.Notify/SIGCHLD loop. Every child below
	// produces a real OS SIGCHLD while Cmd.Wait and the PID1 orphan reaper race to
	// observe it; only Cmd.Wait is allowed to consume a managed worker's status.
	s := &supervisor{workers: map[string]*worker{}}
	unexpectedExit := make(chan int, 1)
	stopSignals := s.installSignalHandlers(func(code int) { unexpectedExit <- code })
	defer stopSignals()

	cases := []struct {
		name       string
		script     string
		wantRC     int
		wantSignal int
	}{
		{name: "normal", script: "exit 0", wantRC: 0},
		{name: "nonzero", script: "exit 23", wantRC: 23},
		{name: "signal", script: "kill -TERM $$", wantRC: 128 + int(syscall.SIGTERM), wantSignal: int(syscall.SIGTERM)},
	}
	for round := 0; round < 12; round++ {
		for _, tc := range cases {
			t.Run(tc.name, func(t *testing.T) {
				_, frames, err := startWorker("sigchld-worker", &WorkerSpec{
					Argv: []string{"sh", "-c", tc.script}, Cwd: t.TempDir(), TimeoutSec: 5,
				})
				if err != nil {
					t.Fatalf("startWorker: %v", err)
				}
				exit := waitWorkerExit(t, frames)
				if exit.Rc != tc.wantRC || exit.Signalled != tc.wantSignal {
					t.Fatalf("lost/corrupt wait status: got rc=%d signal=%d, want rc=%d signal=%d", exit.Rc, exit.Signalled, tc.wantRC, tc.wantSignal)
				}
				if exit.Rc == -1 {
					t.Fatal("managed Cmd.Wait returned no-child status")
				}
			})
		}
	}
	select {
	case code := <-unexpectedExit:
		t.Fatalf("SIGCHLD entered shutdown path with code %d", code)
	default:
	}
}

func TestConcurrentWorkersDoNotClaimContainerOOMDelta(t *testing.T) {
	// memory.events is container-wide. Make two real managed processes overlap,
	// inject one cumulative counter increment, then SIGKILL both. Neither worker may
	// claim the shared delta as its own OOM evidence.
	var counter atomic.Int64
	tracker := newOOMTracker(func() int { return int(counter.Load()) })
	trigger := filepath.Join(t.TempDir(), "release")
	spec := func() *WorkerSpec {
		return &WorkerSpec{
			Argv:       []string{"sh", "-c", `while [ ! -e "$TRIGGER" ]; do :; done; kill -KILL $$`},
			Cwd:        t.TempDir(),
			Env:        map[string]string{"TRIGGER": trigger},
			TimeoutSec: 5,
		}
	}
	_, framesA, err := startWorkerWithRuntime("oom-a", spec(), runtimeChildReaper, tracker)
	if err != nil {
		t.Fatalf("start worker A: %v", err)
	}
	_, framesB, err := startWorkerWithRuntime("oom-b", spec(), runtimeChildReaper, tracker)
	if err != nil {
		t.Fatalf("start worker B: %v", err)
	}
	counter.Store(1)
	if err := os.WriteFile(trigger, []byte("go"), 0o600); err != nil {
		t.Fatal(err)
	}
	exitA := waitWorkerExit(t, framesA)
	exitB := waitWorkerExit(t, framesB)
	for name, exit := range map[string]Frame{"A": exitA, "B": exitB} {
		if exit.Signalled != int(syscall.SIGKILL) || exit.Rc != 128+int(syscall.SIGKILL) {
			t.Fatalf("worker %s did not follow SIGKILL test path: %+v", name, exit)
		}
		if exit.OOM {
			t.Fatalf("worker %s falsely claimed ambiguous container OOM delta", name)
		}
	}
}

func TestUniqueUncausedSIGKILLCanClaimOOMDelta(t *testing.T) {
	var counter atomic.Int64
	tracker := newOOMTracker(func() int { return int(counter.Load()) })
	trigger := filepath.Join(t.TempDir(), "release")
	_, frames, err := startWorkerWithRuntime("oom-only", &WorkerSpec{
		Argv:       []string{"sh", "-c", `while [ ! -e "$TRIGGER" ]; do :; done; kill -KILL $$`},
		Cwd:        t.TempDir(),
		Env:        map[string]string{"TRIGGER": trigger},
		TimeoutSec: 5,
	}, runtimeChildReaper, tracker)
	if err != nil {
		t.Fatalf("start worker: %v", err)
	}
	counter.Store(1)
	if err := os.WriteFile(trigger, []byte("go"), 0o600); err != nil {
		t.Fatal(err)
	}
	exit := waitWorkerExit(t, frames)
	if !exit.OOM || exit.Signalled != int(syscall.SIGKILL) {
		t.Fatalf("unique SIGKILL + counter delta was not attributed: %+v", exit)
	}
	if (oomEvidence{delta: 1}).attributable(int(syscall.SIGKILL), true, false) {
		t.Fatal("supervisor timeout KILL must override container-level OOM evidence")
	}
	if (oomEvidence{delta: 1}).attributable(int(syscall.SIGKILL), false, true) {
		t.Fatal("operator KILL must override container-level OOM evidence")
	}
}

func TestStartWorkerPipesStdinWithoutArgvExposure(t *testing.T) {
	secret := "runtime-stdin-secret-44556677"
	spec := &WorkerSpec{
		Argv:       []string{"sh", "-c", "IFS= read -r line; printf '%s\\n' \"$line\""},
		Cwd:        t.TempDir(),
		Stdin:      secret + "\n",
		TimeoutSec: 5,
	}
	for _, arg := range spec.Argv {
		if strings.Contains(arg, secret) {
			t.Fatalf("secret leaked into argv: %q", arg)
		}
	}
	_, frames, err := startWorker("stdin-worker", spec)
	if err != nil {
		t.Fatalf("startWorker: %v", err)
	}
	got := ""
	gotReceipt := false
	for frame := range frames {
		if frame.T == "out" {
			got += frame.Line
		}
		if frame.T == "stdin" {
			gotReceipt = frame.OK
		}
	}
	if got != secret {
		t.Fatalf("stdin prompt not delivered: got %q want %q", got, secret)
	}
	if !gotReceipt {
		t.Fatal("missing positive stdin completion receipt")
	}
	if spec.Stdin != "" {
		t.Fatal("runtime retained stdin prompt after worker exit")
	}
}

func TestStartWorkerReportsIncompleteLargeStdinForImmediateExit(t *testing.T) {
	spec := &WorkerSpec{
		Argv:       []string{"sh", "-c", "exit 0"},
		Cwd:        t.TempDir(),
		Stdin:      strings.Repeat("x", 4*1024*1024),
		TimeoutSec: 5,
	}
	_, frames, err := startWorker("stdin-epipe", spec)
	if err != nil {
		t.Fatalf("startWorker: %v", err)
	}
	gotReceipt := false
	gotExit := false
	for frame := range frames {
		switch frame.T {
		case "stdin":
			gotReceipt = true
			if frame.OK {
				t.Fatal("immediate-exit child falsely acknowledged full stdin")
			}
		case "exit":
			gotExit = true
		}
	}
	if !gotReceipt || !gotExit {
		t.Fatalf("missing terminal frames stdin=%v exit=%v", gotReceipt, gotExit)
	}
	if spec.Stdin != "" {
		t.Fatal("runtime retained failed stdin payload")
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

func TestSignalFailureDoesNotMutatePausedState(t *testing.T) {
	// A definitely nonexistent process group makes syscall.Kill return ESRCH.  The
	// supervisor must report that error and preserve its prior logical state.
	w := &worker{pgid: 1 << 30}
	if err := w.signal("STOP"); err == nil {
		t.Fatal("STOP on nonexistent process group falsely succeeded")
	}
	if w.paused {
		t.Fatal("failed STOP mutated paused=true")
	}
	w.paused = true
	if err := w.signal("CONT"); err == nil {
		t.Fatal("CONT on nonexistent process group falsely succeeded")
	}
	if !w.paused {
		t.Fatal("failed CONT mutated paused=false")
	}
	if err := w.signal("KILL"); err == nil {
		t.Fatal("KILL on nonexistent process group falsely succeeded")
	}
	if !w.killRequested {
		t.Fatal("failed KILL attempt did not preserve conservative cause fence")
	}
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

func TestReadTokenUnlinksBootstrapFileImmediately(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "token")
	if err := os.WriteFile(path, []byte("one-shot-secret\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	s := &supervisor{}
	if got := s.readToken(path); got != "one-shot-secret" {
		t.Fatalf("read token = %q", got)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("bootstrap token file still exists after read: %v", err)
	}
}

func TestSuccessfulHelloClearsTokenFromSupervisorMemory(t *testing.T) {
	host := newFakeHost(t, "one-shot")
	s := &supervisor{runID: "run-clear", token: "one-shot", workers: map[string]*worker{}}
	result := make(chan net.Conn, 1)
	go func() { result <- s.dialHost(host.addr(), 5*time.Second) }()
	hello := host.accept(t)
	if hello.Token != "one-shot" {
		t.Fatalf("host saw token %q", hello.Token)
	}
	conn := <-result
	if conn == nil {
		t.Fatal("successful hello returned no connection")
	}
	defer conn.Close()
	if s.token != "" {
		t.Fatalf("supervisor retained consumed token %q", s.token)
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
