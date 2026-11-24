package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"time"
)

const scopeFrameLimit = 2 * 1024 * 1024

type runScope struct {
	mu       sync.Mutex
	input    io.WriteCloser
	stopping bool
	armed    bool
	done     chan struct{}
	drained  chan struct{}
	ready    chan error
	err      error
	queue    chan []byte
}
type scopedWorker struct {
	scope   *runScope
	owner   string
	token   string
	running bool
}

func scopeChild(runID, workspace string) int {
	if err := enableRunSubreaper(); err != nil {
		log.Printf("Run scope unavailable: %v", err)
		return 77
	}
	resolveWorkspaceOwner()
	tempRoot, tempDir, err := newRunTemp()
	if err != nil {
		log.Printf("Run temporary directory unavailable: %v", err)
		return 77
	}
	runtimeOOMTracker = newOOMTracker(func() int { return -1 })
	s := &supervisor{runID: runID, workspace: workspace, tempDir: tempDir, workers: map[string]*worker{}, owners: map[string]sharedOwner{}, enc: json.NewEncoder(os.Stdout)}
	s.chownWorkspaceRoot()
	s.seedWorkspaceDocs()
	stopSignals := s.installSignalHandlers(func(int) {
		if err := stopRunDescendants(8 * time.Second); err != nil {
			os.Exit(78)
		}
		if err := os.RemoveAll(tempRoot); err != nil {
			os.Exit(78)
		}
		os.Exit(0)
	})
	defer stopSignals()
	s.send(Frame{T: "scope_ready", OK: true, ScopedProcesses: true})
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 64*1024), scopeFrameLimit)
	for scanner.Scan() {
		var request Request
		if json.Unmarshal(scanner.Bytes(), &request) != nil {
			return 78
		}
		if request.Op == OpRegisterRun || request.Op == OpRenewLease || request.Op == OpTeardownRun {
			return 78
		}
		if request.Op == OpWriteStdin {
			go s.dispatch(&request)
		} else {
			s.dispatch(&request)
		}
	}
	if scanner.Err() != nil {
		log.Printf("Run scope control stream failed: %v", scanner.Err())
	}
	if err := stopRunDescendants(8 * time.Second); err != nil {
		log.Printf("Run scope cleanup unconfirmed: %v", err)
		return 78
	}
	flushed := make(chan struct{})
	go func() { s.streams.Wait(); close(flushed) }()
	select {
	case <-flushed:
		if err := os.RemoveAll(tempRoot); err != nil {
			log.Printf("Run temporary directory cleanup unconfirmed: %v", err)
			return 78
		}
		return 0
	case <-time.After(2 * time.Second):
		return 78
	}
}

func newRunTemp() (string, string, error) {
	root, err := os.MkdirTemp("/tmp", "muteki-run-")
	if err != nil {
		return "", "", err
	}
	work := filepath.Join(root, "work")
	for _, step := range []func() error{
		func() error { return os.Chmod(root, 0o711) },
		func() error { return os.Mkdir(work, 0o700) },
		func() error { return os.Chown(work, workspaceUID, workspaceGID) },
	} {
		if err := step(); err != nil {
			os.RemoveAll(root)
			return "", "", err
		}
	}
	return root, work, nil
}

func (s *supervisor) startRunScope(runID, token, workspace string) (*runScope, error) {
	binary, err := os.Executable()
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(binary, "--scope-child", "--run-id", runID, "--workspace", workspace)
	input, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	output, outputWrite, err := os.Pipe()
	if err != nil {
		input.Close()
		return nil, err
	}
	cmd.Stdout = outputWrite
	cmd.Stderr = os.Stderr
	scope := &runScope{input: input, done: make(chan struct{}), drained: make(chan struct{}), ready: make(chan error, 1), queue: make(chan []byte, 4)}
	if err := runtimeChildReaper.start(cmd); err != nil {
		input.Close()
		output.Close()
		outputWrite.Close()
		return nil, err
	}
	outputWrite.Close()
	go func() {
		for {
			select {
			case bytes := <-scope.queue:
				if _, err := input.Write(bytes); err != nil {
					scope.mu.Lock()
					armed, stopping := scope.armed, scope.stopping
					scope.mu.Unlock()
					if armed && !stopping {
						s.scopeFailure(runID, "Run scope control write failed")
					}
					return
				}
			case <-scope.done:
				return
			}
		}
	}()
	go func() {
		scanner := bufio.NewScanner(output)
		scanner.Buffer(make([]byte, 64*1024), scopeFrameLimit)
		ready := false
		defer close(scope.drained)
		defer output.Close()
		for scanner.Scan() {
			var frame Frame
			if json.Unmarshal(scanner.Bytes(), &frame) != nil {
				break
			}
			if !ready {
				if frame.T != "scope_ready" || !frame.OK || !frame.ScopedProcesses {
					break
				}
				ready = true
				scope.ready <- nil
				continue
			}
			s.mu.Lock()
			if frame.T == "started" && frame.Error == "" {
				if s.scopedWorkers == nil {
					s.scopedWorkers = map[string]scopedWorker{}
				}
				if previous, exists := s.scopedWorkers[frame.WorkerID]; exists && previous.scope != scope {
					s.mu.Unlock()
					s.scopeFailure(runID, "duplicate Worker identity")
					return
				}
				s.scopedWorkers[frame.WorkerID] = scopedWorker{scope: scope, owner: runID, token: token, running: true}
			} else if frame.T == "exit" {
				worker := s.scopedWorkers[frame.WorkerID]
				worker.running = false
				s.scopedWorkers[frame.WorkerID] = worker
				id := frame.WorkerID
				time.AfterFunc(30*time.Second, func() {
					s.mu.Lock()
					defer s.mu.Unlock()
					if current, ok := s.scopedWorkers[id]; ok && current.scope == scope && !current.running {
						delete(s.scopedWorkers, id)
					}
				})
			}
			s.mu.Unlock()
			s.send(frame)
		}
		if !ready {
			scope.ready <- fmt.Errorf("Run scope did not establish subreaper/pidfd support")
		}
		scope.mu.Lock()
		armed, stopping := scope.armed, scope.stopping
		scope.mu.Unlock()
		if armed && !stopping {
			s.scopeFailure(runID, "Run scope control stream closed unexpectedly")
		}
	}()
	go func() {
		err := runtimeChildReaper.wait(cmd)
		scope.mu.Lock()
		scope.err = err
		armed, stopping := scope.armed, scope.stopping
		scope.mu.Unlock()
		close(scope.done)
		if armed && (!stopping || err != nil) {
			s.scopeFailure(runID, "Run scope exited without confirmed cleanup")
		}
	}()
	select {
	case err := <-scope.ready:
		if err != nil {
			scope.mu.Lock()
			scope.stopping = true
			input.Close()
			scope.mu.Unlock()
			return nil, err
		}
		return scope, nil
	case <-time.After(5 * time.Second):
		scope.mu.Lock()
		scope.stopping = true
		input.Close()
		scope.mu.Unlock()
		return nil, fmt.Errorf("Run scope startup timed out")
	}
}
func (s *supervisor) scopeFailure(runID, reason string) {
	log.Printf("shared runtime failed closed: run=%s reason=%s container_oom_kill_total=%d", runID, reason, readOOMKill())
	os.Exit(76)
}
func (scope *runScope) forward(req *Request) error {
	bytes, err := json.Marshal(req)
	if err != nil {
		return err
	}
	if len(bytes) > scopeFrameLimit-1 {
		return fmt.Errorf("Run scope request exceeds frame bound")
	}
	scope.mu.Lock()
	defer scope.mu.Unlock()
	if scope.stopping {
		return fmt.Errorf("Run scope is stopping")
	}
	select {
	case <-scope.done:
		return fmt.Errorf("Run scope is unavailable")
	default:
	}
	select {
	case scope.queue <- append(bytes, '\n'):
		if req.Op == OpStartWorker {
			scope.armed = true
		}
		return nil
	default:
		return fmt.Errorf("Run scope control queue is full; no action was queued")
	}
}
func (scope *runScope) stop() error {
	scope.mu.Lock()
	scope.stopping = true
	_ = scope.input.Close()
	scope.mu.Unlock()
	deadline := time.NewTimer(12 * time.Second)
	defer deadline.Stop()
	select {
	case <-scope.done:
		scope.mu.Lock()
		err := scope.err
		scope.mu.Unlock()
		if err != nil {
			return err
		}
	case <-deadline.C:
		return fmt.Errorf("Run scope stop remained unconfirmed")
	}
	select {
	case <-scope.drained:
		return nil
	case <-deadline.C:
		return fmt.Errorf("Run scope exit stream did not drain")
	}

}
func (s *supervisor) routeScopedControl(req *Request) {
	s.mu.Lock()
	worker, exists := s.scopedWorkers[req.WorkerID]
	s.mu.Unlock()
	if !exists {
		s.send(Frame{T: "resp", ReqID: req.ReqID, Error: "Worker scope unavailable"})
		return
	}
	if req.Op == OpSignal && (req.OwnerRunID != worker.owner || req.OwnerToken != worker.token) {
		s.send(Frame{T: "resp", ReqID: req.ReqID, Error: "Worker scope ownership mismatch"})
		return
	}
	if err := worker.scope.forward(req); err != nil {
		s.send(Frame{T: "resp", ReqID: req.ReqID, Error: err.Error()})
	}
}
