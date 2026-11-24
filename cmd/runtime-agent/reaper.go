package main

import (
	"os/exec"
	"sync"
)

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
	err := cmd.Wait()
	r.mu.Lock()
	if cmd.Process != nil {
		delete(r.managed, cmd.Process.Pid)
	}
	r.mu.Unlock()

	r.reapOrphans()
	return err
}

func reapOrphans() {
	runtimeChildReaper.reapOrphans()
}
