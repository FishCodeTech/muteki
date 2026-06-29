package main

import (
	"bufio"
	"log"
	"os"
	"os/exec"
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
	id   string
	tag  string
	cmd  *exec.Cmd
	pgid int

	mu        sync.Mutex
	paused    bool
	exited    bool
	rc        int
	signalled int
	timedOut  bool
	oom       bool
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
	// Worker reads nothing from stdin → instant EOF (codex hangs on an open stdin).
	cmd.Stdin = nil

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

	oomBefore := readOOMKill()
	if err := cmd.Start(); err != nil {
		return nil, nil, err
	}
	pgid, _ := syscall.Getpgid(cmd.Process.Pid)
	w := &worker{id: id, tag: spec.Tag, cmd: cmd, pgid: pgid}

	events := make(chan Frame, 256)
	var streamWG sync.WaitGroup
	streamWG.Add(2)
	pump := func(r *bufio.Scanner, t string) {
		defer streamWG.Done()
		// Large buffer: CLI JSON lines (full tool outputs) can be long.
		r.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
		for r.Scan() {
			events <- Frame{T: t, Line: r.Text()}
		}
	}
	so := bufio.NewScanner(stdout)
	se := bufio.NewScanner(stderr)
	go pump(so, "out")
	go pump(se, "err")

	// Wall-clock cap: SIGKILL the whole tree once it has spent TimeoutSec ACTIVELY
	// running (authoritative, replaces the in-container `timeout -s KILL`). M7: the
	// budget is pause-aware — while the operator has the worker SIGSTOP-frozen
	// (w.paused), the clock does NOT advance, so a long HITL pause can't trip the
	// timeout and mislabel a deliberately paused worker as timed_out. A polling
	// goroutine (vs a fixed AfterFunc) so it can discount paused intervals.
	timerDone := make(chan struct{})
	go func() {
		budget := time.Duration(maxInt(1, spec.TimeoutSec)) * time.Second
		var active time.Duration // wall-clock spent NOT paused
		const tick = 200 * time.Millisecond
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
				w.mu.Unlock()
				if !paused {
					active += now.Sub(last)
				}
				last = now
				if active >= budget {
					w.mu.Lock()
					w.timedOut = true
					w.mu.Unlock()
					w.signalTree(syscall.SIGKILL)
					return
				}
			}
		}
	}()

	go func() {
		// Wait for both pumps to drain the pipes, THEN reap — otherwise Wait() can
		// close the pipes mid-read and we lose trailing output.
		streamWG.Wait()
		err := cmd.Wait()
		close(timerDone)
		rc, sig := exitInfo(err)
		oomAfter := readOOMKill()
		oom := oomBefore >= 0 && oomAfter > oomBefore

		w.mu.Lock()
		w.exited = true
		w.rc = rc
		w.signalled = sig
		w.oom = oom
		timedOut := w.timedOut && !oom // an OOM that races the timer is an OOM
		w.timedOut = timedOut
		w.mu.Unlock()

		events <- Frame{T: "exit", Rc: rc, OOM: oom, TimedOut: timedOut, Signalled: sig}
		close(events)
	}()

	return w, events, nil
}

// signalTree sends sig to the worker's whole process group (negative pgid). Used for
// STOP/CONT/KILL/TERM. Safe to call after exit (best-effort).
func (w *worker) signalTree(sig syscall.Signal) {
	if w.pgid > 0 {
		_ = syscall.Kill(-w.pgid, sig)
		return
	}
	if w.cmd != nil && w.cmd.Process != nil {
		_ = w.cmd.Process.Signal(sig)
	}
}

func (w *worker) signal(name string) error {
	switch name {
	case "STOP":
		w.mu.Lock()
		w.paused = true
		w.mu.Unlock()
		w.signalTree(syscall.SIGSTOP)
	case "CONT":
		w.mu.Lock()
		w.paused = false
		w.mu.Unlock()
		w.signalTree(syscall.SIGCONT)
	case "TERM":
		w.signalTree(syscall.SIGTERM)
	case "KILL":
		w.signalTree(syscall.SIGKILL)
	default:
		return &startErr{"unknown signal " + name}
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

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}
