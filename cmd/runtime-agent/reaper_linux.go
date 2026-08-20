//go:build linux

package main

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

// reapOrphans reaps only adopted/unmanaged children. Linux exposes the direct
// children of each task in /proc; enumerate every task because Go may fork from
// any runtime thread. Holding r.mu makes the snapshot mutually exclusive with a
// managed cmd.Start + registration, so a just-started worker cannot be mistaken
// for an orphan even if it exits immediately.
func (r *childReaper) reapOrphans() {
	r.mu.Lock()
	defer r.mu.Unlock()

	for {
		children := directChildPIDs()
		reaped := false
		for _, pid := range children {
			if _, managed := r.managed[pid]; managed {
				continue
			}
			var status syscall.WaitStatus
			got, err := syscall.Wait4(pid, &status, syscall.WNOHANG, nil)
			if err == nil && got == pid {
				reaped = true
			}
		}
		if !reaped {
			return
		}
		// Reaping an adopted process can expose another generation of orphaned
		// descendants. Refresh /proc until a complete pass makes no progress.
	}
}

func directChildPIDs() []int {
	tasks, err := os.ReadDir("/proc/self/task")
	if err != nil {
		return nil
	}
	seen := make(map[int]struct{})
	for _, task := range tasks {
		if !task.IsDir() {
			continue
		}
		data, err := os.ReadFile(filepath.Join("/proc/self/task", task.Name(), "children"))
		if err != nil {
			continue
		}
		for _, field := range strings.Fields(string(data)) {
			pid, err := strconv.Atoi(field)
			if err == nil && pid > 0 {
				seen[pid] = struct{}{}
			}
		}
	}
	out := make([]int, 0, len(seen))
	for pid := range seen {
		out = append(out, pid)
	}
	return out
}
