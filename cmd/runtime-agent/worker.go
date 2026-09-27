package main

import (
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

// worker is one forked CLI agent the supervisor manages. Because the supervisor
// forks it directly (vs the old host-side `docker exec`), it owns the real PID and
// puts the worker in its OWN process group (Setpgid) — so a STOP/CONT/KILL hits the
// worker AND every helper it spawns (curl/python/sh) by signalling the negative
// pgid. This is strictly cleaner than the old `pkill -f <tag>` cmdline-sentinel
// hack, which only worked because the host couldn't reach the in-container tree.
type worker struct {
	id         string
	ownerRunID string
	ownerToken string
	tag        string
	cmd        *exec.Cmd
	pgid       int

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
}

// uid/gid of the kali user the worker runs as. Resolved once at startup. The
// supervisor itself runs as root (PID1) so it can drop to kali for the worker.
var (
	kaliUID int = -1
	kaliGID int = -1
)

func resolveKali() {
	// Prefer reading /etc/passwd directly — os/user needs cgo for some libc setups
	// and we build CGO_ENABLED=0. The worker image creates `kali` via useradd.
	data, err := os.ReadFile("/etc/passwd")
	if err != nil {
		return
	}
	for _, line := range strings.Split(string(data), "\n") {
		f := strings.Split(line, ":")
		if len(f) >= 4 && f[0] == "kali" {
			if u, err := strconv.Atoi(f[2]); err == nil {
				kaliUID = u
			}
			if g, err := strconv.Atoi(f[3]); err == nil {
				kaliGID = g
			}
			return
		}
	}
}

// startWorker forks the spec'd argv as a new worker. The caller streams its output
// via the returned channels. stdout/stderr are merged-but-tagged: each is its own
// channel so the host can distinguish (the driver only re-parses stdout, but stderr
// is surfaced for diagnostics).
func startWorker(id string, spec *WorkerSpec) (*worker, <-chan Frame, error) {
	return startWorkerWithRuntime(id, spec, runtimeChildReaper, runtimeOOMTracker)
}

func startWorkerWithRuntime(id string, spec *WorkerSpec, reaper *childReaper, oomTracker *oomTracker) (*worker, <-chan Frame, error) {
	if len(spec.Argv) == 0 {
		return nil, nil, &startErr{"empty argv"}
	}
	cmd := exec.Command(spec.Argv[0], spec.Argv[1:]...)
	cmd.Dir = spec.Cwd

	// Build the worker env. Start from a minimal sane base, overlay the host's keys.
	env := baseEnv()
	for k, v := range spec.Env {
		env[k] = v
	}
	// Apply the *_FILE → env indirection the old shell prelude did (claude/cursor/
	// anthropic/openai tokens are mounted as files; the CLIs want them in env).
	applyTokenFiles(env)
	cmd.Env = flattenEnv(env)

	// Own process group so signals reach the whole tree; drop to kali (with sudo).
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if kaliUID >= 0 && kaliGID >= 0 {
		cmd.SysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(kaliUID), Gid: uint32(kaliGID),
		}
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, nil, err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return nil, nil, err
	}
	// Ordinary workers get instant EOF (codex hangs on an open stdin). A secure
	// prompt gets an explicit pipe + completion frame: cmd.Start alone is NOT proof
	// that os/exec's asynchronous Reader copy reached the child.
	var stdinPipe io.WriteCloser
	var stdinPayload []byte
	if spec.Stdin != "" {
		stdinPayload = []byte(spec.Stdin)
		spec.Stdin = "" // drop the decoded request copy before spawning
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

	// Make the dirs the worker writes owned by the kali user it runs as. Both the
	// worker's cwd (a per-worker dir like workspace/workers/cli-<seat>-N) AND its HOME
	// (a per-worker workspace/homes/cli-<id>) are created HOST-side by the ROOT web
	// process and bind-mount in owned root:root. The worker runs as kali and writes
	// BOTH dirs — codex app-server state + PATH aliases, claude session/config, PoC
	// files. A root-owned cwd/home makes every such write fail with EACCES, and the
	// engine then exits SILENTLY producing ZERO output (codex first warns "could not
	// create PATH aliases" then aborts the in-process app-server; claude exits 0 with
	// no stdout at all). chownWorkspaceRoot only fixes the workspace root, not these
	// per-worker subdirs. So chown the cwd and the (env-declared) HOME to kali right
	// before spawning, and pre-create the engine state dirs — the raw-setuid spawn
	// can't always mkdir them the way a `su` login shell can.
	if kaliUID >= 0 && kaliGID >= 0 {
		chownToKali := func(d string) {
			if d == "" {
				return
			}
			if err := os.MkdirAll(d, 0o755); err == nil {
				if err := os.Chown(d, kaliUID, kaliGID); err != nil {
					log.Printf("runtime-agent: chown %s -> kali(%d:%d): %v", d, kaliUID, kaliGID, err)
				}
			}
		}
		chownToKali(cmd.Dir)
		home := "/home/kali"
		for _, kv := range cmd.Env {
			if strings.HasPrefix(kv, "HOME=") {
				if v := kv[len("HOME="):]; v != "" {
					home = v
				}
				break
			}
		}
		chownToKali(home)
		for _, sub := range []string{
			"/.codex", "/.claude", "/.local", "/.local/bin",
			"/.local/share", "/.config", "/.cache",
		} {
			if err := os.MkdirAll(home+sub, 0o775); err == nil {
				_ = os.Chown(home+sub, kaliUID, kaliGID)
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

	events := make(chan Frame, 256)
	var streamWG sync.WaitGroup
	streamWG.Add(2)
	outputLimitBytes := resolveByteLimit(spec.OutputLimitBytes, "MUTEKI_WORKER_OUTPUT_LIMIT", defaultOutputLimitBytes)
	var streamBytes sync.Mutex
	var totalStreamBytes int64
	// Bounded byte streaming replaces bufio.Scanner (4 MiB max-token): long
	// JSONL tool lines must not fail silently, and total capture stays capped.
	pump := func(r io.Reader, t string) {
		defer streamWG.Done()
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
					events <- Frame{T: t, Line: string(line)}
				}
				if over {
					if len(pending) > 0 {
						events <- Frame{T: t, Line: string(pending)}
					}
					w.mu.Lock()
					w.outputLimit = true
					w.killRequested = true
					w.mu.Unlock()
					_ = w.signalTree(syscall.SIGKILL)
					return
				}
				// Cap pending incomplete line so a single huge line cannot grow forever.
				if int64(len(pending)) > outputLimitBytes && outputLimitBytes > 0 {
					w.mu.Lock()
					w.outputLimit = true
					w.killRequested = true
					w.mu.Unlock()
					_ = w.signalTree(syscall.SIGKILL)
					return
				}
			}
			if readErr != nil {
				if len(pending) > 0 {
					events <- Frame{T: t, Line: string(pending)}
					pending = nil
				}
				return
			}
		}
	}
	go pump(stdout, "out")
	go pump(stderr, "err")

	var stdinWG sync.WaitGroup
	if stdinPipe != nil {
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
			// Best-effort zero the mutable transport copy immediately after the pipe
			// completes; do not retain it for the worker's 30s status grace period.
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

	// Wall-clock cap: SIGKILL the whole tree once it has spent TimeoutSec ACTIVELY
	// running (authoritative, replaces the in-container `timeout -s KILL`). M7: the
	// budget is pause-aware — while the operator has the worker SIGSTOP-frozen
	// (w.paused), the clock does NOT advance, so a long HITL pause can't trip the
	// timeout and mislabel a deliberately paused worker as timed_out. A polling
	// goroutine (vs a fixed AfterFunc) so it can discount paused intervals.
	// Same loop also enforces workdir growth budget (#170).
	timerDone := make(chan struct{})
	go func() {
		budget := time.Duration(maxInt(1, spec.TimeoutSec)) * time.Second
		var active time.Duration // wall-clock spent NOT paused
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
					// Publish the supervisor-authored KILL cause before delivering the
					// signal. The child can exit and Cmd.Wait can return immediately;
					// setting this afterwards would leave a window where a concurrent
					// cgroup delta could be falsely attributed as an OOM.
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
		// The stdin receipt must precede exit/channel-close. Wait for its writer and
		// both output pumps, THEN reap — otherwise Wait can close pipes mid-transfer.
		stdinWG.Wait()
		streamWG.Wait()
		err := reaper.wait(cmd)
		close(timerDone)
		// Fast writers may finish before the periodic monitor's first tick.
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
		// Supervisors that killed for output/disk keep those typed causes; OOM still
		// wins when the kernel also claimed the tree (cgroup evidence attributable).
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

// signalTree sends sig to the worker's whole process group (negative pgid). Used for
// STOP/CONT/KILL/TERM. Safe to call after exit (best-effort).
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
		// See the timer path above: cause publication must happen before the
		// signal, otherwise the waiter can win the race and label an operator KILL
		// from unrelated container-wide OOM evidence. A failed attempt may leave
		// this true, which is an intentional conservative false-negative boundary.
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

// startErr is a tiny error type so we avoid pulling in fmt/errors churn.
type startErr struct{ msg string }

func (e *startErr) Error() string { return e.msg }

// exitInfo extracts (rc, terminating-signal) from a cmd.Wait() error.
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

// readOOMKill reads the container cgroup's cumulative oom_kill counter (v2
// memory.events, v1 fallback). -1 if unreadable. A nonzero delta across a worker's
// lifetime means the kernel OOM-killer SIGKILL'd something — the discriminator that
// tells a real wall-clock timeout (137 at budget) from an OOM victim (137 early).
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

// default stream / workdir budgets when the host omits override and env is unset.
// Intentionally not TSec's 64MiB/256MiB/4GiB product numbers (#170).
const (
	defaultOutputLimitBytes int64 = 64 * 1024 * 1024       // 64 MiB combined stdout+stderr
	defaultDiskLimitBytes   int64 = 4 * 1024 * 1024 * 1024 // 4 GiB workdir growth
)

func indexByte(b []byte, c byte) int {
	for i, v := range b {
		if v == c {
			return i
		}
	}
	return -1
}

// resolveByteLimit prefers an explicit StartWorker value, then a docker-style
// MUTEKI_WORKER_* env (e.g. "64m"), then the built-in default.
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
	// Accept integers only for simplicity (host already normalizes).
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil || n <= 0 || n > math.MaxInt64/mult {
		return 0, false
	}
	return n * mult, true
}

// dirSizeBytes walks a directory tree and sums regular file sizes. Returns -1
// on error so a transient walk failure never falsely trips the disk budget.
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
