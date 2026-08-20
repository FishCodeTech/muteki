//go:build linux

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"testing"
	"time"
)

func TestSupervisorSIGCHLDReapsUnmanagedChildWithoutTouchingManagedChild(t *testing.T) {
	s := &supervisor{workers: map[string]*worker{}}
	stopSignals := s.installSignalHandlers(func(code int) {
		t.Errorf("unexpected shutdown path: %d", code)
	})
	defer stopSignals()

	// An unregistered direct child models an adopted orphan. Its /proc entry must
	// disappear after the production SIGCHLD path reaps it.
	orphan := exec.Command("sh", "-c", "exit 0")
	if err := orphan.Start(); err != nil {
		t.Fatal(err)
	}
	procPath := filepath.Join("/proc", strconv.Itoa(orphan.Process.Pid))
	deadline := time.Now().Add(3 * time.Second)
	for {
		_, err := os.Stat(procPath)
		if os.IsNotExist(err) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("unmanaged child %d remained unreaped", orphan.Process.Pid)
		}
		time.Sleep(10 * time.Millisecond)
	}

	_, frames, err := startWorker("managed-after-orphan", &WorkerSpec{
		Argv: []string{"sh", "-c", "exit 37"}, Cwd: t.TempDir(), TimeoutSec: 5,
	})
	if err != nil {
		t.Fatal(err)
	}
	exit := waitWorkerExit(t, frames)
	if exit.Rc != 37 || exit.Signalled != 0 {
		t.Fatalf("managed status was stolen after orphan reap: %+v", exit)
	}
}
