package main

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/fs"
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

const agentVersion = "muteki-runtime-agent/4"

var startedAt = time.Now()

type supervisor struct {
	runID      string
	sharedPool bool
	token      string
	workspace  string
	tempDir    string

	connMu      sync.Mutex
	enc         *json.Encoder
	helloReader *bufio.Reader

	mu            sync.Mutex
	workers       map[string]*worker
	owners        map[string]sharedOwner
	seq           int
	lease         executionLease
	streams       sync.WaitGroup
	scopedWorkers map[string]scopedWorker
}

type sharedOwner struct {
	token     string
	workspace string
	scope     *runScope
}

func main() {
	scopeMode := flag.Bool("scope-child", false, "internal Run subreaper control over inherited pipes")
	leaseRequired := flag.Bool("lease-required", false, "require renewable remote execution lease")
	connect := flag.String("connect", "", "host control receiver host:port to dial (e.g. host.docker.internal:9100). Required.")
	runID := flag.String("run-id", "", "this run's id, sent in the Hello frame")
	sharedPool := flag.Bool("shared-pool", false, "serve multiple explicitly trusted Runs")
	tokenPath := flag.String("token", "", "path to the per-run token file (default: /run/muteki/control/token)")
	tokenInline := flag.String("token-value", "", "the per-run token directly (overrides --token file)")
	workspace := flag.String("workspace", "/home/kali/workspace", "worker workspace (mount target)")
	_ = flag.String("sock", "", "(ignored — reverse-connect model uses --connect)")
	_ = flag.String("addr", "", "(ignored — reverse-connect model uses --connect)")
	flag.Parse()

	log.SetFlags(log.LstdFlags | log.Lmicroseconds)
	log.SetPrefix("[runtime-agent] ")

	if *scopeMode {
		os.Exit(scopeChild(*runID, *workspace))
	}
	if *sharedPool && os.Getpid() != 1 {
		log.Fatal("Shared runtime agent must be container PID1")
	}
	if os.Geteuid() != 0 {
		log.Fatal("Runtime Agent must run as container root")
	}
	resolveWorkspaceOwner()

	s := &supervisor{
		runID:      *runID,
		sharedPool: *sharedPool,
		workspace:  *workspace,
		workers:    map[string]*worker{},
		owners:     map[string]sharedOwner{},
	}

	if *leaseRequired {
		s.lease.initialize(time.Now())
		go func() {
			ticker := time.NewTicker(50 * time.Millisecond)
			defer ticker.Stop()
			for now := range ticker.C {
				if s.lease.expired(now) {
					log.Print("execution lease expired; exiting container")
					os.Exit(75)
				}
			}
		}()
	}

	if *tokenInline != "" {
		s.token = strings.TrimSpace(*tokenInline)
	} else {
		tp := *tokenPath
		if tp == "" {
			tp = "/run/muteki/control/token"
		}
		s.token = s.readToken(tp)
	}

	s.chownWorkspaceRoot()
	stopOwnership := s.maintainWorkspaceOwnership()
	defer stopOwnership()

	if !s.sharedPool {
		s.seedWorkspaceDocs()
	}

	_ = s.installSignalHandlers(os.Exit)

	if *connect == "" {
		log.Fatalf("no --connect host:port given (reverse-connect model requires it)")
	}

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

func (s *supervisor) dialHost(addr string, deadline time.Duration) net.Conn {
	t0 := time.Now()
	for time.Since(t0) < deadline {
		conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
		if err != nil {
			time.Sleep(500 * time.Millisecond)
			continue
		}
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
			return nil
		}
		s.token = ""
		s.enc = enc
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
				if s.sharedPool && req.Op == OpTeardownRun {
					go s.dispatch(&req)
				} else {
					s.dispatch(&req)
				}
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

func (s *supervisor) dispatch(req *Request) {
	if s.sharedPool && (req.Op == OpSignal || req.Op == OpStatus || req.Op == OpWriteStdin) {
		s.routeScopedControl(req)
		return
	}
	switch req.Op {
	case OpStartWorker:
		s.opStartWorker(req)
	case OpWriteStdin:
		s.opWriteStdin(req)
	case OpRegisterRun:
		s.opRegisterRun(req)
	case OpSignal:
		s.opSignal(req)
	case OpStatus:
		s.opStatus(req)
	case OpTeardownRun:
		if s.sharedPool {
			if req.OwnerRunID == "" {
				s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
			} else {
				s.send(Frame{T: "resp", ReqID: req.ReqID, OK: s.killOwned(req.OwnerRunID, req.OwnerToken), ScopedTeardown: true})
			}
		} else {
			s.killAll()
			s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true})
		}
	case OpRenewLease:
		err := s.lease.renew(time.Now(), req.LeaseMillis, req.LeaseExpiresAt)
		frame := Frame{T: "resp", ReqID: req.ReqID, OK: err == nil, ExecutionLease: true}
		if err != nil {
			frame.Error = err.Error()
		}
		s.send(frame)
	case OpHealth:
		s.opHealth(req)
	case OpVerifyOutput:
		s.opVerifyOutput(req)
	default:
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
	}
}

func (s *supervisor) send(f Frame) {
	s.connMu.Lock()
	defer s.connMu.Unlock()
	if s.enc != nil {
		_ = s.enc.Encode(f)
	}
}

func (s *supervisor) opStartWorker(req *Request) {
	if !s.lease.admits(time.Now()) {
		s.send(Frame{T: "started", ReqID: req.ReqID, Error: "execution lease is not active"})
		return
	}
	if req.Spec == nil {
		s.send(Frame{T: "started", ReqID: req.ReqID, Error: "missing spec"})
		return
	}
	if s.sharedPool {
		owner := filepath.Clean(req.Spec.OwnerWorkspace)
		root := filepath.Clean(s.workspace)
		cwd := filepath.Clean(req.Spec.Cwd)
		s.mu.Lock()
		registered, active := s.owners[req.Spec.OwnerRunID]
		s.mu.Unlock()
		if req.Spec.OwnerRunID == "" || owner == root ||
			!pathWithin(owner, root) || !pathWithin(cwd, owner) ||
			!active || registered.token != req.Spec.OwnerToken || registered.workspace != owner {
			s.send(Frame{T: "started", ReqID: req.ReqID, Error: "invalid shared Run workspace ownership"})
			return
		}
		if registered.scope == nil {
			s.send(Frame{T: "started", ReqID: req.ReqID, Error: "Run subreaper is unavailable"})
			return
		}
		if err := registered.scope.forward(req); err != nil {
			s.send(Frame{T: "started", ReqID: req.ReqID, Error: err.Error()})
		}
		return
	} else {
		req.Spec.OwnerRunID = s.runID
	}
	if s.tempDir != "" {
		if req.Spec.Env == nil {
			req.Spec.Env = map[string]string{}
		}
		for _, key := range []string{"TMPDIR", "TMP", "TEMP", "MUTEKI_RUN_TEMP_DIR"} {
			req.Spec.Env[key] = s.tempDir
		}
	}
	s.mu.Lock()
	s.seq++
	scopeHash := sha256.Sum256([]byte(s.runID))
	nonce := shortRand()
	id := fmt.Sprintf("w-%x-%s-%s", scopeHash[:16], itoa(s.seq), nonce)
	s.mu.Unlock()
	if nonce == "" {
		s.send(Frame{T: "started", ReqID: req.ReqID, Error: "Worker identity entropy unavailable"})
		return
	}

	if !s.sharedPool {
		s.seedWorkspaceDocs()
	}

	w, events, err := startWorker(id, req.Spec)
	if err != nil {
		s.send(Frame{T: "started", ReqID: req.ReqID, WorkerID: id, Tag: req.Spec.Tag, Error: err.Error()})
		return
	}
	s.mu.Lock()
	s.workers[id] = w
	s.mu.Unlock()

	s.send(Frame{T: "started", ReqID: req.ReqID, WorkerID: id, Tag: req.Spec.Tag})

	s.streams.Add(1)
	go func(id string, reqID int64) {
		for ev := range events {
			ev.ReqID = reqID
			ev.WorkerID = id
			s.send(ev)
		}
		s.streams.Done()
		time.Sleep(30 * time.Second)
		s.mu.Lock()
		delete(s.workers, id)
		s.mu.Unlock()
	}(id, req.ReqID)
}

func (s *supervisor) opRegisterRun(req *Request) {
	if !s.sharedPool || req.OwnerRunID == "" || len(req.OwnerToken) < 32 {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
		return
	}
	owner := filepath.Clean(req.OwnerWorkspace)
	root := filepath.Clean(s.workspace)
	if owner == root || filepath.Dir(owner) != root || !pathWithin(owner, root) {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false})
		return
	}
	s.mu.Lock()
	previous, exists := s.owners[req.OwnerRunID]
	s.mu.Unlock()
	if exists {
		ok := previous.token == req.OwnerToken && previous.workspace == owner && previous.scope != nil
		if ok {
			previous.scope.mu.Lock()
			ok = !previous.scope.stopping
			previous.scope.mu.Unlock()
			select {
			case <-previous.scope.done:
				ok = false
			default:
			}
		}
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: ok})
		return
	}
	scope, err := s.startRunScope(req.OwnerRunID, req.OwnerToken, owner)
	if err != nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, Error: err.Error()})
		return
	}
	s.mu.Lock()
	if s.owners == nil {
		s.owners = map[string]sharedOwner{}
	}
	s.owners[req.OwnerRunID] = sharedOwner{token: req.OwnerToken, workspace: owner, scope: scope}
	s.mu.Unlock()
	s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true})
}

func (s *supervisor) opSignal(req *Request) {
	w := s.lookup(req.WorkerID)
	if w == nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: "worker unavailable"})
		return
	}
	if s.sharedPool && (req.OwnerRunID != w.ownerRunID || req.OwnerToken != w.ownerToken) {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: "shared worker ownership mismatch"})
		return
	}
	if err := w.signal(strings.ToUpper(req.Signal)); err != nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: err.Error()})
		return
	}
	state, rc, paused, _, _ := w.status()
	s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true, State: state, RcPtr: rc, Paused: paused})
}

func (s *supervisor) opWriteStdin(req *Request) {
	w := s.lookup(req.WorkerID)
	if w == nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: "worker unavailable"})
		return
	}
	if err := w.writeStdin(req.Data); err != nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: err.Error()})
		return
	}
	s.send(Frame{T: "resp", ReqID: req.ReqID, OK: true})
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

func (s *supervisor) opVerifyOutput(req *Request) {
	w := s.lookup(req.WorkerID)
	if w == nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: "worker unavailable"})
		return
	}
	if s.sharedPool && (req.OwnerRunID != w.ownerRunID || req.OwnerToken != w.ownerToken) {
		s.send(Frame{T: "resp", ReqID: req.ReqID, OK: false, Error: "shared worker ownership mismatch"})
		return
	}
	matched, total, shaHex, excerpt := w.verifyOutput(req.Needle)
	s.send(Frame{
		T:          "resp",
		ReqID:      req.ReqID,
		OK:         true,
		MatchFound: matched,
		RingBytes:  total,
		RingSHA256: shaHex,
		Excerpt:    excerpt,
	})
}

func (s *supervisor) opHealth(req *Request) {
	s.mu.Lock()
	n := 0
	for _, w := range s.workers {
		if st, _, _, _, _ := w.status(); st == "running" {
			n++
		}
	}
	for _, worker := range s.scopedWorkers {
		if worker.running {
			n++
		}
	}
	s.mu.Unlock()
	total, available, sampled := hostMemory()
	s.send(Frame{
		MemorySampled: sampled, HostMemoryTotal: total, HostMemoryAvailable: available,
		T: "resp", ReqID: req.ReqID, OK: true, Version: agentVersion,
		ScopedTeardown: true, ExecutionLease: true, ScopedProcesses: supportsRunSubreaper(),
		Workers: n, Uptime: int64(time.Since(startedAt).Seconds()),
	})
}

func (s *supervisor) readToken(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		log.Printf("no token file at %s (%v) — cannot authenticate", path, err)
		return ""
	}
	token := strings.TrimSpace(string(data))
	for i := range data {
		data[i] = 0
	}
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		log.Printf("could not remove bootstrap token %s: %v", path, err)
		return ""
	}
	return token
}

func (s *supervisor) chownWorkspaceRoot() {
	if workspaceUID < 0 || workspaceGID < 0 {
		return
	}
	if err := os.MkdirAll(s.workspace, 0o755); err != nil {
		log.Printf("chown workspace: mkdir %s: %v", s.workspace, err)
		return
	}
	if err := os.Chown(s.workspace, workspaceUID, workspaceGID); err != nil {
		log.Printf("chown workspace %s -> workspace owner(%d:%d): %v", s.workspace, workspaceUID, workspaceGID, err)
		return
	}
	log.Printf("chowned workspace %s to workspace owner(%d:%d)", s.workspace, workspaceUID, workspaceGID)
}

func (s *supervisor) maintainWorkspaceOwnership() func() {
	if workspaceUID == 0 && workspaceGID == 0 {
		return func() {}
	}
	done := make(chan struct{})
	stopped := make(chan struct{})
	handoff := func() {
		root, err := os.OpenRoot(s.workspace)
		if err != nil {
			log.Printf("workspace ownership: %v", err)
			return
		}
		defer root.Close()
		err = fs.WalkDir(root.FS(), ".", func(name string, entry fs.DirEntry, walkErr error) error {
			if os.IsNotExist(walkErr) {
				return nil
			}
			if walkErr != nil {
				return walkErr
			}
			info, err := entry.Info()
			if os.IsNotExist(err) {
				return nil
			}
			if err != nil {
				return err
			}
			stat, ok := info.Sys().(*syscall.Stat_t)
			if !ok || (int(stat.Uid) == workspaceUID && int(stat.Gid) == workspaceGID) {
				return nil
			}
			err = root.Lchown(name, workspaceUID, workspaceGID)
			if os.IsNotExist(err) {
				return nil
			}
			return err
		})
		if err != nil {
			log.Printf("workspace ownership handoff incomplete: %v", err)
		}
	}
	go func() {
		defer close(stopped)
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				handoff()
			case <-done:
				handoff()
				return
			}
		}
	}()
	return func() { close(done); <-stopped }
}

const workspaceDocMarker = "<!-- muteki-workspace-doc:"

func managedWorkspaceDoc(data []byte) bool {
	return bytes.HasPrefix(data, []byte(workspaceDocMarker))
}

func seedManagedWorkspaceDoc(workspace, name string, srcData []byte) (string, error) {
	if len(srcData) == 0 {
		return "skip-missing-src", nil
	}
	if !bytes.HasPrefix(srcData, []byte(workspaceDocMarker)) {
		return "skip-unmanaged-src", nil
	}
	if err := os.MkdirAll(workspace, 0o755); err != nil {
		return "", err
	}
	dst := filepath.Join(workspace, name)
	if info, err := os.Lstat(dst); err == nil && (!info.Mode().IsRegular()) {
		return "skip-operator", nil
	} else if err != nil && !os.IsNotExist(err) {
		return "", err
	}
	existed := false
	existing, err := os.ReadFile(dst)
	if err == nil {
		existed = true
		if !managedWorkspaceDoc(existing) {
			return "skip-operator", nil
		}
		if bytes.Equal(existing, srcData) {
			return "unchanged", nil
		}
	} else if !os.IsNotExist(err) {
		return "", err
	}
	tmp, err := os.CreateTemp(workspace, ".muteki-workspace-doc-*")
	if err != nil {
		return "", err
	}
	defer os.Remove(tmp.Name())
	if _, err = tmp.Write(srcData); err != nil {
		tmp.Close()
		return "", err
	}
	if err = tmp.Chmod(0o644); err != nil {
		tmp.Close()
		return "", err
	}
	if err = tmp.Close(); err != nil {
		return "", err
	}
	if err = os.Rename(tmp.Name(), dst); err != nil {
		return "", err
	}
	if workspaceUID >= 0 {
		_ = os.Chown(dst, workspaceUID, workspaceGID)
	}
	if existed {
		return "upgraded", nil
	}
	return "seeded", nil
}

func (s *supervisor) seedWorkspaceDocs() {
	s.seedWorkspaceDocsAt(s.workspace)
}

func (s *supervisor) seedWorkspaceDocsAt(workspace string) {
	for _, name := range []string{"AGENTS.md", "CLAUDE.md"} {
		src := filepath.Join("/opt/muteki", name)
		data, err := os.ReadFile(src)
		if err != nil {
			continue
		}
		action, err := seedManagedWorkspaceDoc(workspace, name, data)
		if err != nil {
			log.Printf("seed %s: %v", filepath.Join(workspace, name), err)
			continue
		}
		switch action {
		case "seeded", "upgraded":
			log.Printf("%s %s", action, filepath.Join(workspace, name))
		}
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

func pathWithin(child, parent string) bool {
	rel, err := filepath.Rel(parent, child)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(os.PathSeparator))
}

func (s *supervisor) killOwned(ownerRunID, ownerToken string) bool {
	s.mu.Lock()
	owner, exists := s.owners[ownerRunID]
	s.mu.Unlock()
	if !exists {
		return true
	}
	if owner.token != ownerToken || owner.scope == nil {
		return false
	}
	if err := owner.scope.stop(); err != nil {
		s.scopeFailure(ownerRunID, err.Error())
		return false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if current, ok := s.owners[ownerRunID]; !ok || current.scope != owner.scope {
		return false
	}
	delete(s.owners, ownerRunID)
	for id, worker := range s.scopedWorkers {
		if worker.scope == owner.scope {
			delete(s.scopedWorkers, id)
		}
	}
	return true
}

func (s *supervisor) installSignalHandlers(exit func(int)) func() {
	sigc := make(chan os.Signal, 16)
	done := make(chan struct{})
	var once sync.Once
	signal.Notify(sigc, syscall.SIGTERM, syscall.SIGINT, syscall.SIGCHLD)
	go func() {
		for {
			select {
			case <-done:
				return
			case sig := <-sigc:
				switch sig {
				case syscall.SIGCHLD:
					reapOrphans()
				default:
					log.Printf("received %v, shutting down", sig)
					s.killAll()
					exit(0)
				}
			}
		}
	}()
	return func() {
		once.Do(func() {
			signal.Stop(sigc)
			close(done)
		})
	}
}

func trimNL(b []byte) []byte {
	return []byte(strings.TrimRight(string(b), "\r\n"))
}
