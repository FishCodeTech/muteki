package main

import (
	"os/exec"
	"sync"
)

// childReaper gives every direct child exactly one authoritative waiter.
//
// Managed workers are reaped by os/exec's Cmd.Wait.  PID1 also adopts orphaned
// grandchildren, but a wildcard Wait4(-1) cannot distinguish those orphans from
// managed workers and can steal a worker's wait status before Cmd.Wait sees it.
// start and the managed-PID registration therefore share a lock with orphan
// discovery, and the platform reaper waits only for specific, unregistered PIDs.
type childReaper struct {
	mu      sync.Mutex
	managed map[int]struct{}
}

func newChildReaper() *childReaper {
	return &childReaper{managed: make(map[int]struct{})}
}

var runtimeChildReaper = newChildReaper()

func (r *childReaper) start(cmd *exec.Cmd) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if err := cmd.Start(); err != nil {
		return err
	}
	r.managed[cmd.Process.Pid] = struct{}{}
	return nil
}

func (r *childReaper) wait(cmd *exec.Cmd) error {
	// Do not hold r.mu while blocking. reapOrphans holds it while taking its child
	// snapshot and skips this PID until Cmd.Wait has completed.
	err := cmd.Wait()
	r.mu.Lock()
	if cmd.Process != nil {
		delete(r.managed, cmd.Process.Pid)
	}
	r.mu.Unlock()

	// Re-scan after a managed parent is gone: children it left behind may only now
	// have been adopted by PID1, and SIGCHLD delivery is allowed to coalesce.
	r.reapOrphans()
	return err
}

func reapOrphans() {
	runtimeChildReaper.reapOrphans()
}
