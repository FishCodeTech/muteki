package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"log"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

const workerRingMaxBytes = 1024 * 1024

type worker struct {
	id         string
	ownerRunID string
	ownerToken string
	tag        string
	cmd        *exec.Cmd
	pgid       int
	stdinMu    sync.Mutex
	stdinPipe  io.WriteCloser

	mu            sync.Mutex
	paused        bool
	exited        bool
	rc            int
	signalled     int
	timedOut      bool
	oom           bool
	outputLimit   bool
	diskLimit     bool
	killRequested bool

	ringMu    sync.Mutex
	ringBuf   []byte
	ringTotal int64
}

func stripANSISequences(raw []byte) []byte {
	out := make([]byte, 0, len(raw))
	i := 0
	for i < len(raw) {
		if raw[i] == 0x1b && i+1 < len(raw) {
			next := raw[i+1]
			if next == '[' {
				i += 2
				for i < len(raw) && !(raw[i] >= 0x40 && raw[i] <= 0x7e) {
					i++
				}
				if i < len(raw) {
					i++
				}
				continue
			}
			if next == ']' {
				i += 2
				for i < len(raw) && raw[i] != 0x07 {
					if raw[i] == 0x1b && i+1 < len(raw) && raw[i+1] == '\\' {
						i += 2
						break
					}
					i++
				}
				if i < len(raw) && raw[i] == 0x07 {
					i++
				}
				continue
			}
		}
		out = append(out, raw[i])
		i++
	}
	return out
}

func (w *worker) appendRing(line []byte) {
	cleaned := stripANSISequences(line)
	w.ringMu.Lock()
	defer w.ringMu.Unlock()
	w.ringTotal += int64(len(cleaned)) + 1
	w.ringBuf = append(w.ringBuf, cleaned...)
	w.ringBuf = append(w.ringBuf, '\n')
	if len(w.ringBuf) > workerRingMaxBytes {
		excess := len(w.ringBuf) - workerRingMaxBytes
		copy(w.ringBuf, w.ringBuf[excess:])
		w.ringBuf = w.ringBuf[:workerRingMaxBytes]
	}
}

func (w *worker) verifyOutput(needle string) (bool, int64, string, string) {
	w.ringMu.Lock()
	snapshot := make([]byte, len(w.ringBuf))
	copy(snapshot, w.ringBuf)
	total := w.ringTotal
	w.ringMu.Unlock()
	sum := sha256.Sum256(snapshot)
	shaHex := hex.EncodeToString(sum[:])
	if needle == "" {
		return true, total, shaHex, ""
	}
	idx := bytes.Index(snapshot, []byte(needle))
	if idx < 0 {
		return false, total, shaHex, ""
	}
	start := idx - 80
	if start < 0 {
		start = 0
	}
	end := idx + len(needle) + 80
	if end > len(snapshot) {
		end = len(snapshot)
	}
	return true, total, shaHex, string(snapshot[start:end])
}

var (
	workspaceUID = 0
	workspaceGID = 0
)

func resolveWorkspaceOwner() {
	if uid, err := strconv.Atoi(os.Getenv("MUTEKI_WORKSPACE_UID")); err == nil && uid >= 0 {
		workspaceUID = uid
	}
	if gid, err := strconv.Atoi(os.Getenv("MUTEKI_WORKSPACE_GID")); err == nil && gid >= 0 {
		workspaceGID = gid
	}
}

func startWorker(id string, spec *WorkerSpec) (*worker, <-chan Frame, error) {
	return startWorkerWithRuntime(id, spec, runtimeChildReaper, runtimeOOMTracker)
}

func startWorkerWithRuntime(id string, spec *WorkerSpec, reaper *childReaper, oomTracker *oomTracker) (*worker, <-chan Frame, error) {
	if spec.TimeoutSec > int(math.MaxInt64/int64(time.Second)) || spec.TimeoutSec < 0 {
		return nil, nil, &startErr{"worker timeout exceeds supported duration"}
	}
	if len(spec.Argv) == 0 {
		return nil, nil, &startErr{"empty argv"}
	}
	cmd := exec.Command(spec.Argv[0], spec.Argv[1:]...)
	cmd.Dir = spec.Cwd

	env := baseEnv()
	mergeSpecEnv(env, spec.Env)
	env["USER"], env["LOGNAME"] = "root", "root"
	env["IS_SANDBOX"] = "1"
	applyTokenFiles(env)
	cmd.Env = flattenEnv(env)

	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setpgid:    true,
		Credential: &syscall.Credential{Uid: 0, Gid: 0},
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, nil, err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return nil, nil, err
	}
	var stdinPipe io.WriteCloser
	var stdinPayload []byte
	if spec.StdinMode == "stream" {
		stdinPipe, err = cmd.StdinPipe()
		if err != nil {
			return nil, nil, err
		}
	} else if spec.Stdin != "" {
		stdinPayload = []byte(spec.Stdin)
		spec.Stdin = ""
		stdinPipe, err = cmd.StdinPipe()
		if err != nil {
			for i := range stdinPayload {
				stdinPayload[i] = 0
			}
			return nil, nil, err
		}
	} else {
		cmd.Stdin = nil
	}


	if workspaceUID >= 0 && workspaceGID >= 0 {
		chownToWorkspaceOwner := func(d string) {
			if d == "" {
				return
			}
			if err := os.MkdirAll(d, 0o755); err == nil {
				if err := os.Chown(d, workspaceUID, workspaceGID); err != nil {
					log.Printf("runtime-agent: chown %s -> workspace owner(%d:%d): %v", d, workspaceUID, workspaceGID, err)
				}
			}
		}
		chownToWorkspaceOwner(cmd.Dir)
		home := "/home/kali"
		for _, kv := range cmd.Env {
			if strings.HasPrefix(kv, "HOME=") {
				if v := kv[len("HOME="):]; v != "" {
					home = v
				}
				break
			}
		}
		chownToWorkspaceOwner(home)
		for _, sub := range []string{
			"/.codex", "/.claude", "/.local", "/.local/bin",
			"/.local/share", "/.config", "/.cache",
		} {
			if err := os.MkdirAll(home+sub, 0o775); err == nil {
				_ = os.Chown(home+sub, workspaceUID, workspaceGID)
			}
		}
	}

	diskLimitBytes := resolveByteLimit(spec.DiskLimitBytes, "MUTEKI_WORKER_DISK_LIMIT", defaultDiskLimitBytes)
	budgetRoot := spec.Cwd
	if budgetRoot == "" {
		budgetRoot, _ = os.Getwd()
	}
	cwdBaseline := dirSizeBytes(budgetRoot)
	if diskLimitBytes > 0 && cwdBaseline < 0 {
		return nil, nil, &startErr{"cannot measure worker directory"}
	}
	oomObservation := oomTracker.begin()
	kimiUsage := checkpointKimiUsage(spec, env)
	if err := reaper.start(cmd); err != nil {
		oomTracker.cancel(oomObservation)
		if stdinPipe != nil {
			_ = stdinPipe.Close()
		}
		for i := range stdinPayload {
			stdinPayload[i] = 0
		}
		return nil, nil, err
	}
	pgid, _ := syscall.Getpgid(cmd.Process.Pid)
	w := &worker{id: id, ownerRunID: spec.OwnerRunID, ownerToken: spec.OwnerToken, tag: spec.Tag, cmd: cmd, pgid: pgid}
	if spec.StdinMode == "stream" {
		w.stdinPipe = stdinPipe
	}

	events := make(chan Frame, 16)
	var streamWG sync.WaitGroup
	streamWG.Add(2)
	outputLimitBytes := resolveByteLimit(spec.OutputLimitBytes, "MUTEKI_WORKER_OUTPUT_LIMIT", defaultOutputLimitBytes)
	var streamBytes sync.Mutex
	var totalStreamBytes int64
	const maxEncodedOutputLine = 500 * 1024
	stopOutput := func(reason string) {
		w.mu.Lock()
		w.outputLimit = true
		w.killRequested = true
		w.mu.Unlock()
		_ = w.signalTree(syscall.SIGKILL)
		events <- Frame{T: "err", Line: reason}
	}
	pump := func(r io.Reader, t string) {
		defer streamWG.Done()
		emit := func(line []byte) bool {
			if len(line) > maxEncodedOutputLine {
				stopOutput("Worker output line exceeds the 500 KiB frame budget")
				return false
			}
			text := string(line)
			encoded, err := json.Marshal(text)
			if err != nil || len(encoded) > maxEncodedOutputLine {
				stopOutput("Worker encoded output line exceeds the 500 KiB frame budget")
				return false
			}
			w.appendRing(line)
			events <- Frame{T: t, Line: text}
			return true
		}
		buf := make([]byte, 64*1024)
		var pending []byte
		for {
			n, readErr := r.Read(buf)
			if n > 0 {
				chunk := buf[:n]
				streamBytes.Lock()
				remaining := outputLimitBytes - totalStreamBytes
				over := int64(n) > remaining
				if over {
					if remaining < 0 {
						remaining = 0
					}
					chunk = chunk[:int(remaining)]
				}
				totalStreamBytes += int64(len(chunk))
				streamBytes.Unlock()
				searchStart := len(pending)
				pending = append(pending, chunk...)
				for {
					idx := indexByte(pending[searchStart:], '\n')
					if idx < 0 {
						break
					}
					idx += searchStart
					searchStart = 0
					line := pending[:idx]
					pending = pending[idx+1:]
					if !emit(line) {
						return
					}
				}
				if len(pending) > maxEncodedOutputLine {
					stopOutput("Worker incomplete output line exceeds the 500 KiB frame budget")
					return
				}
				if over {
					if len(pending) > 0 && !emit(pending) {
						return
					}
					stopOutput("Worker combined output exceeds its configured byte budget")
					return
				}
			}
			if readErr != nil {
				if len(pending) > 0 {
					emit(pending)
				}
				return
			}
		}
	}
	go pump(stdout, "out")
	go pump(stderr, "err")

	var stdinWG sync.WaitGroup
	if stdinPipe != nil && spec.StdinMode != "stream" {
		stdinWG.Add(1)
		go func(payload []byte) {
			defer stdinWG.Done()
			ok := true
			offset := 0
			for offset < len(payload) {
				n, writeErr := stdinPipe.Write(payload[offset:])
				if n > 0 {
					offset += n
				}
				if writeErr != nil || n == 0 {
					ok = false
					break
				}
			}
			if closeErr := stdinPipe.Close(); closeErr != nil {
				ok = false
			}
			for i := range payload {
				payload[i] = 0
			}
			frame := Frame{T: "stdin", OK: ok}
			if !ok {
				frame.Error = "stdin handoff incomplete"
			}
			events <- frame
		}(stdinPayload)
	}

	timerDone := make(chan struct{})
	go func() {
		budget := time.Duration(maxInt(1, spec.TimeoutSec)) * time.Second
		var active time.Duration
		const tick = 200 * time.Millisecond
		var diskTicks int
		last := time.Now()
		ticker := time.NewTicker(tick)
		defer ticker.Stop()
		for {
			select {
			case <-timerDone:
				return
			case now := <-ticker.C:
				w.mu.Lock()
				paused := w.paused
				alreadyLimited := w.outputLimit || w.diskLimit
				w.mu.Unlock()
				if !paused {
					active += now.Sub(last)
				}
				last = now
				if alreadyLimited {
					return
				}
				diskTicks++
				if diskLimitBytes > 0 && cwdBaseline >= 0 && diskTicks%10 == 0 {
					cur := dirSizeBytes(budgetRoot)
					if cur >= 0 && cur-cwdBaseline > diskLimitBytes {
						w.mu.Lock()
						w.diskLimit = true
						w.killRequested = true
						w.mu.Unlock()
						_ = w.signalTree(syscall.SIGKILL)
						return
					}
				}
				if active >= budget {
					w.mu.Lock()
					w.killRequested = true
					w.mu.Unlock()
					if err := w.signalTree(syscall.SIGKILL); err == nil {
						w.mu.Lock()
						w.timedOut = true
						w.mu.Unlock()
					}
					return
				}
			}
		}
	}()

	go func() {
		stdinWG.Wait()
		streamWG.Wait()
		err := reaper.wait(cmd)
		if kimiUsage != nil {
			events <- kimiUsage.frame()
		}
		w.stdinMu.Lock()
		if w.stdinPipe != nil {
			_ = w.stdinPipe.Close()
			w.stdinPipe = nil
		}
		w.stdinMu.Unlock()
		close(timerDone)
		if cur := dirSizeBytes(budgetRoot); diskLimitBytes > 0 && cur >= 0 && cur-cwdBaseline > diskLimitBytes {
			w.mu.Lock()
			w.diskLimit = true
			w.mu.Unlock()
		}
		rc, sig := exitInfo(err)

		w.mu.Lock()
		timedOutRequested := w.timedOut
		killRequested := w.killRequested
		outputLimitHit := w.outputLimit
		diskLimitHit := w.diskLimit
		w.mu.Unlock()
		oomEvidence := oomTracker.finish(oomObservation)
		oom := oomEvidence.attributable(sig, timedOutRequested, killRequested)
		if oomEvidence.delta > 0 && !oom {
			log.Printf("runtime-agent: observed container oom_kill delta=%d for worker %s but attribution was ambiguous or contradicted by the exit cause", oomEvidence.delta, id)
		}
		if oom {
			outputLimitHit = false
			diskLimitHit = false
		}

		w.mu.Lock()
		w.exited = true
		w.rc = rc
		w.signalled = sig
		w.oom = oom
		w.outputLimit = outputLimitHit
		w.diskLimit = diskLimitHit
		timedOut := w.timedOut && !oom && !outputLimitHit && !diskLimitHit
		w.timedOut = timedOut
		w.mu.Unlock()

		events <- Frame{
			T: "exit", Rc: rc, OOM: oom, TimedOut: timedOut,
			OutputLimit: outputLimitHit, DiskLimit: diskLimitHit, Signalled: sig,
		}
		close(events)
	}()

	return w, events, nil
}

func (w *worker) writeStdin(data string) error {
	w.stdinMu.Lock()
	defer w.stdinMu.Unlock()
	if w.stdinPipe == nil {
		return errors.New("worker stdin is unavailable")
	}
	raw := []byte(data)
	if len(raw) == 0 || raw[len(raw)-1] != '\n' {
		raw = append(raw, '\n')
	}
	for len(raw) > 0 {
		n, err := w.stdinPipe.Write(raw)
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrShortWrite
		}
		raw = raw[n:]
	}
	return nil
}

func (w *worker) signalTree(sig syscall.Signal) error {
	if w.pgid > 0 {
		return syscall.Kill(-w.pgid, sig)
	}
	if w.cmd != nil && w.cmd.Process != nil {
		return w.cmd.Process.Signal(sig)
	}
	return &startErr{"worker process unavailable"}
}

func (w *worker) signal(name string) error {
	var sig syscall.Signal
	switch name {
	case "STOP":
		sig = syscall.SIGSTOP
	case "CONT":
		sig = syscall.SIGCONT
	case "TERM":
		sig = syscall.SIGTERM
	case "KILL":
		sig = syscall.SIGKILL
	default:
		return &startErr{"unknown signal " + name}
	}
	if name == "KILL" {
		w.mu.Lock()
		w.killRequested = true
		w.mu.Unlock()
	}
	if err := w.signalTree(sig); err != nil {
		return err
	}
	if name == "STOP" || name == "CONT" {
		w.mu.Lock()
		w.paused = name == "STOP"
		w.mu.Unlock()
	}
	return nil
}

func (w *worker) status() (state string, rc *int, paused, oom, timedOut bool) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if !w.exited {
		if w.paused {
			return "running", nil, true, false, false
		}
		return "running", nil, false, false, false
	}
	r := w.rc
	switch {
	case w.oom:
		state = "oom"
	case w.outputLimit:
		state = "output_limit"
	case w.diskLimit:
		state = "disk_limit"
	case w.timedOut:
		state = "timed_out"
	default:
		state = "exited"
	}
	return state, &r, false, w.oom, w.timedOut
}

type startErr struct{ msg string }

func (e *startErr) Error() string { return e.msg }

func exitInfo(err error) (int, int) {
	if err == nil {
		return 0, 0
	}
	if ee, ok := err.(*exec.ExitError); ok {
		if ws, ok := ee.Sys().(syscall.WaitStatus); ok {
			if ws.Signaled() {
				return 128 + int(ws.Signal()), int(ws.Signal())
			}
			return ws.ExitStatus(), 0
		}
		return ee.ExitCode(), 0
	}
	return -1, 0
}

func readOOMKill() int {
	for _, p := range []string{
		"/sys/fs/cgroup/memory.events",
		"/sys/fs/cgroup/memory/memory.oom_control",
	} {
		data, err := os.ReadFile(p)
		if err != nil {
			continue
		}
		for _, line := range strings.Split(string(data), "\n") {
			f := strings.Fields(line)
			if len(f) == 2 && f[0] == "oom_kill" {
				if n, err := strconv.Atoi(f[1]); err == nil {
					return n
				}
			}
		}
	}
	return -1
}

const (
	defaultOutputLimitBytes int64 = 64 * 1024 * 1024
	defaultDiskLimitBytes   int64 = 4 * 1024 * 1024 * 1024
)

func indexByte(b []byte, c byte) int {
	for i, v := range b {
		if v == c {
			return i
		}
	}
	return -1
}

func resolveByteLimit(explicit int64, envKey string, fallback int64) int64 {
	if explicit > 0 {
		return explicit
	}
	raw := strings.TrimSpace(os.Getenv(envKey))
	if raw == "" {
		return fallback
	}
	if n, ok := parseDockerSize(raw); ok && n > 0 {
		return n
	}
	return fallback
}

func parseDockerSize(raw string) (int64, bool) {
	s := strings.TrimSpace(strings.ToLower(raw))
	if s == "" {
		return 0, false
	}
	mult := int64(1)
	switch {
	case strings.HasSuffix(s, "kib") || strings.HasSuffix(s, "ki"):
		mult = 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "kib"), "ki")
	case strings.HasSuffix(s, "mib") || strings.HasSuffix(s, "mi"):
		mult = 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "mib"), "mi")
	case strings.HasSuffix(s, "gib") || strings.HasSuffix(s, "gi"):
		mult = 1024 * 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "gib"), "gi")
	case strings.HasSuffix(s, "tib") || strings.HasSuffix(s, "ti"):
		mult = 1024 * 1024 * 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "tib"), "ti")
	case strings.HasSuffix(s, "kb") || strings.HasSuffix(s, "k"):
		mult = 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "kb"), "k")
	case strings.HasSuffix(s, "mb") || strings.HasSuffix(s, "m"):
		mult = 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "mb"), "m")
	case strings.HasSuffix(s, "gb") || strings.HasSuffix(s, "g"):
		mult = 1024 * 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "gb"), "g")
	case strings.HasSuffix(s, "tb") || strings.HasSuffix(s, "t"):
		mult = 1024 * 1024 * 1024 * 1024
		s = strings.TrimSuffix(strings.TrimSuffix(s, "tb"), "t")
	case strings.HasSuffix(s, "b"):
		s = strings.TrimSuffix(s, "b")
	}
	s = strings.TrimSpace(s)
	if s == "" {
		return 0, false
	}
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil || n <= 0 || n > math.MaxInt64/mult {
		return 0, false
	}
	return n * mult, true
}

func dirSizeBytes(root string) int64 {
	if root == "" {
		return -1
	}
	var total int64
	err := filepath.Walk(root, func(path string, info os.FileInfo, walkErr error) error {
		if walkErr != nil {
			if errors.Is(walkErr, os.ErrNotExist) {
				return nil
			}
			return walkErr
		}
		if info == nil || !info.Mode().IsRegular() {
			return nil
		}
		total += info.Size()
		return nil
	})
	if err != nil {
		return -1
	}
	return total
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}
