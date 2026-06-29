// runtime-agent — muteki's in-container Runtime Control Plane supervisor.
//
// It is the container's PID1 (ENTRYPOINT). It does NOT listen on any port. At startup
// it DIALS the host's control receiver (host.docker.internal:<port>), sends a Hello
// with {run_id, token}, and then serves the host's commands on that one connection:
// StartWorker / Signal / Status / TeardownRun / Health. It is a DUMB EXECUTOR — it
// forks workers, forwards their raw output, routes signals, reports status. It does
// NOT touch flag judgment, fact provenance, graph writes, or key lookups; those stay
// in the backend (§8). It opens NO port, so the worker (trusted, runs as kali+sudo)
// has no entry point to drive it — the reverse-connect model is what makes it a true
// "controlled端" rather than a network service.
//
// Single static binary, standard library only (CGO_ENABLED=0).
package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"io"
	"log"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
)

const agentVersion = "muteki-runtime-agent/2"

var startedAt = time.Now()

type supervisor struct {
	runID     string
	token     string
	workspace string

	// the single reverse connection to the host + a write mutex (all worker streams
	// multiplex onto it, so writes must be serialized).
	connMu      sync.Mutex
	enc         *json.Encoder
	helloReader *bufio.Reader // buffered reader positioned past the Hello handshake

	mu      sync.Mutex
	workers map[string]*worker
	seq     int
}

func main() {
	connect := flag.String("connect", "", "host control receiver host:port to dial (e.g. host.docker.internal:9100). Required.")
	runID := flag.String("run-id", "", "this run's id, sent in the Hello frame")
	tokenPath := flag.String("token", "", "path to the per-run token file (default: /run/muteki/control/token)")
	tokenInline := flag.String("token-value", "", "the per-run token directly (overrides --token file)")
	workspace := flag.String("workspace", "/home/kali/workspace", "worker workspace (mount target)")
	// kept for backward-compat with the baked ENTRYPOINT (--sock ... is ignored now).
	_ = flag.String("sock", "", "(ignored — reverse-connect model uses --connect)")
	_ = flag.String("addr", "", "(ignored — reverse-connect model uses --connect)")
	flag.Parse()

	log.SetFlags(log.LstdFlags | log.Lmicroseconds)
	log.SetPrefix("[runtime-agent] ")

	resolveKali()

	s := &supervisor{
		runID:     *runID,
		workspace: *workspace,
		workers:   map[string]*worker{},
	}

	// Token: inline value wins, else read the file.
	if *tokenInline != "" {
		s.token = strings.TrimSpace(*tokenInline)
	} else {
		tp := *tokenPath
		if tp == "" {
			tp = "/run/muteki/control/token"
		}
		s.token = s.readToken(tp)
	}

	// The host bind-mounts the workspace dir created by the (root) web process, so
	// it lands here owned by root:root. The worker runs as kali and writes its cwd
	// (codex app-server state, claude session/config, PoC files) DIRECTLY in the
	// workspace root — a root-owned root makes every such write fail with EACCES
	// ("could not create PATH aliases: Permission denied" for codex; a silent
	// no-output for claude). Chown the workspace root to kali BEFORE seeding/working
	// so the worker owns its own cwd. (seedWorkspaceDocs already chowns the files it
	// writes; this fixes the directory the host handed us.)
	s.chownWorkspaceRoot()

	// Bootstrap the workspace tool-awareness files (坑 A): the host bind-mounts an
	// (initially empty) workspace over /home/kali/workspace, shadowing anything baked
	// there. We cp the baked /opt/muteki/{AGENTS,CLAUDE}.md in AFTER the mount so the
	// CLIs auto-read them. Idempotent — never clobber a worker-modified copy.
	s.seedWorkspaceDocs()

	// Reap-on-signal: as PID1, handle TERM/INT so `docker stop` is graceful.
	sigc := make(chan os.Signal, 4)
	signal.Notify(sigc, syscall.SIGTERM, syscall.SIGINT, syscall.SIGCHLD)
	go func() {
		for sig := range sigc {
			switch sig {
			case syscall.SIGCHLD:
				reapOrphans()
			default:
				log.Printf("received %v, shutting down", sig)
				s.killAll()
				os.Exit(0)
			}
		}
	}()

	if *connect == "" {
		log.Fatalf("no --connect host:port given (reverse-connect model requires it)")
	}

	// Dial the host receiver, retrying until it's up (the backend may start the
	// receiver a moment after `docker run`). The connection is the lifeline; if it
	// drops, the run is over (the host treats a dropped connection as degraded), so
	// we exit and let `docker rm -f` clean up rather than silently re-dialing forever.
	conn := s.dialHost(*connect, 60*time.Second)
	if conn == nil {
		log.Fatalf("could not reach host control receiver at %s", *connect)
	}
	defer conn.Close()
	log.Printf("connected to host %s (run_id=%s, token=%v, workspace=%s)",
		*connect, s.runID, s.token != "", s.workspace)

	s.serve(conn)
	log.Printf("control connection closed; exiting")
}

// dialHost dials the host receiver and completes the Hello handshake. Returns the
// live connection or nil on failure after the deadline.
func (s *supervisor) dialHost(addr string, deadline time.Duration) net.Conn {
	t0 := time.Now()
	for time.Since(t0) < deadline {
		conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
		if err != nil {
			time.Sleep(500 * time.Millisecond)
			continue
		}
		// send Hello, await HelloAck.
		enc := json.NewEncoder(conn)
		if err := enc.Encode(Hello{Hello: 1, RunID: s.runID, Token: s.token, Version: agentVersion}); err != nil {
			conn.Close()
			time.Sleep(500 * time.Millisecond)
			continue
		}
		r := bufio.NewReader(conn)
		line, err := r.ReadBytes('\n')
		if err != nil {
			conn.Close()
			time.Sleep(500 * time.Millisecond)
			continue
		}
		var ack HelloAck
		if json.Unmarshal(trimNL(line), &ack) != nil || !ack.OK {
			log.Printf("host rejected hello: %s", strings.TrimSpace(string(line)))
			conn.Close()
			return nil // auth failure is terminal, don't retry
		}
		s.enc = enc
		// stash the reader so serve() continues from where Hello left off.
		s.helloReader = r
		return conn
	}
	return nil
}

func (s *supervisor) serve(conn net.Conn) {
	r := s.helloReader
	if r == nil {
		r = bufio.NewReader(conn)
	}
	for {
		line, err := r.ReadBytes('\n')
		if len(line) > 0 {
			var req Request
			if json.Unmarshal(trimNL(line), &req) == nil {
				s.dispatch(&req)
			}
		}
		if err != nil {
			if err != io.EOF {
				log.Printf("control read: %v", err)
			}
			return
		}
	}
}

// dispatch handles one host command. StartWorker runs the worker and streams its
// frames asynchronously (so the control connection keeps accepting commands); the
// others reply synchronously.
func (s *supervisor) dispatch(req *Request) {
	switch req.Op {
	case OpStartWorker:
		s.opStartWorker(req)
	case OpSignal:
		s.opSignal(req)
	case OpStatus:
		s.opStatus(req)
	case OpTeardownRun:
		s.killAll()
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true})
	case OpHealth:
		s.opHealth(req)
	default:
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
	}
}

// send serializes one frame onto the shared connection (worker streams + command
// replies all funnel through here, so the mutex prevents interleaved JSON).
func (s *supervisor) send(f Frame) {
	s.connMu.Lock()
	defer s.connMu.Unlock()
	if s.enc != nil {
		_ = s.enc.Encode(f)
	}
}

func (s *supervisor) opStartWorker(req *Request) {
	if req.Spec == nil {
		s.send(Frame{T: "started", ReqID: req.ReqID, Error: "missing spec"})
		return
	}
	s.mu.Lock()
	s.seq++
	id := "w-" + itoa(s.seq) + "-" + shortRand()
	s.mu.Unlock()

	// Ensure the tool-awareness docs are in place right before a worker starts.
	s.seedWorkspaceDocs()

	w, events, err := startWorker(id, req.Spec)
	if err != nil {
		s.send(Frame{T: "started", ReqID: req.ReqID, WorkerID: id, Tag: req.Spec.Tag, Error: err.Error()})
		return
	}
	s.mu.Lock()
	s.workers[id] = w
	s.mu.Unlock()

	// started ack carries the worker id; the host keys subsequent frames on it.
	s.send(Frame{T: "started", ReqID: req.ReqID, WorkerID: id, Tag: req.Spec.Tag})

	// pump this worker's events onto the shared connection, tagged with worker id.
	go func(id string, reqID int64) {
		for ev := range events {
			ev.ReqID = reqID
			ev.WorkerID = id
			s.send(ev)
		}
		// drop from registry after a grace so a late Status still sees terminal state.
		time.Sleep(30 * time.Second)
		s.mu.Lock()
		delete(s.workers, id)
		s.mu.Unlock()
	}(id, req.ReqID)
}

func (s *supervisor) opSignal(req *Request) {
	w := s.lookup(req.WorkerID)
	if w == nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
		return
	}
	ok := w.signal(strings.ToUpper(req.Signal)) == nil
	s.send(Frame{T: "resp", ReqID: req.ReqID, OK: ok})
}

func (s *supervisor) opStatus(req *Request) {
	w := s.lookup(req.WorkerID)
	if w == nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true, State: "unknown"})
		return
	}
	state, rc, paused, _, _ := w.status()
	s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true, State: state, RcPtr: rc, Paused: paused})
}

func (s *supervisor) opHealth(req *Request) {
	s.mu.Lock()
	n := 0
	for _, w := range s.workers {
		if st, _, _, _, _ := w.status(); st == "running" {
			n++
		}
	}
	s.mu.Unlock()
	s.send(Frame{
		T: "resp", ReqID: req.ReqID, OK: true, Version: agentVersion,
		Workers: n, Uptime: int64(time.Since(startedAt).Seconds()),
	})
}

func (s *supervisor) readToken(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		log.Printf("no token file at %s (%v) — auth disabled", path, err)
		return ""
	}
	return strings.TrimSpace(string(data))
}

// chownWorkspaceRoot makes the bind-mounted workspace directory owned by the kali
// user the worker runs as. The host (root) created and mounted it root:root, which
// would block every cwd write the worker CLI does. Best-effort: log and continue if
// kali isn't resolvable or chown fails (e.g. unusual mount) — the worker may still
// manage in some cases, and we don't want to abort the supervisor over it.
func (s *supervisor) chownWorkspaceRoot() {
	if kaliUID < 0 || kaliGID < 0 {
		return
	}
	if err := os.MkdirAll(s.workspace, 0o755); err != nil {
		log.Printf("chown workspace: mkdir %s: %v", s.workspace, err)
		return
	}
	if err := os.Chown(s.workspace, kaliUID, kaliGID); err != nil {
		log.Printf("chown workspace %s -> kali(%d:%d): %v", s.workspace, kaliUID, kaliGID, err)
		return
	}
	log.Printf("chowned workspace %s to kali(%d:%d)", s.workspace, kaliUID, kaliGID)
}

func (s *supervisor) seedWorkspaceDocs() {
	for _, name := range []string{"AGENTS.md", "CLAUDE.md"} {
		src := filepath.Join("/opt/muteki", name)
		dst := filepath.Join(s.workspace, name)
		if _, err := os.Stat(dst); err == nil {
			continue // already present (worker may have edited it) — don't clobber
		}
		data, err := os.ReadFile(src)
		if err != nil {
			continue
		}
		if err := os.MkdirAll(s.workspace, 0o755); err != nil {
			continue
		}
		if err := os.WriteFile(dst, data, 0o644); err != nil {
			log.Printf("seed %s: %v", dst, err)
			continue
		}
		if kaliUID >= 0 {
			_ = os.Chown(dst, kaliUID, kaliGID)
		}
		log.Printf("seeded %s", dst)
	}
}

func (s *supervisor) lookup(id string) *worker {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.workers[id]
}

func (s *supervisor) killAll() {
	s.mu.Lock()
	ws := make([]*worker, 0, len(s.workers))
	for _, w := range s.workers {
		ws = append(ws, w)
	}
	s.mu.Unlock()
	for _, w := range ws {
		w.signal("KILL")
	}
}

func reapOrphans() {
	for {
		var ws syscall.WaitStatus
		pid, err := syscall.Wait4(-1, &ws, syscall.WNOHANG, nil)
		if pid <= 0 || err != nil {
			return
		}
	}
}

func trimNL(b []byte) []byte {
	return []byte(strings.TrimRight(string(b), "\r\n"))
}
