//go:build linux

package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

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
	}
}

func directChildPIDs() []int { children, _ := directChildPIDsChecked(); return children }

func directChildPIDsChecked() ([]int, error) {
	tasks, err := os.ReadDir("/proc/self/task")
	if err != nil {
		return nil, err
	}
	inspected := 0
	seen := make(map[int]struct{})
	for _, task := range tasks {
		if !task.IsDir() {
			continue
		}
		data, err := os.ReadFile(filepath.Join("/proc/self/task", task.Name(), "children"))
		if os.IsNotExist(err) || errors.Is(err, syscall.ESRCH) {
			continue
		}
		if err != nil {
			return nil, err
		}
		inspected++
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
	if inspected == 0 {
		return nil, fmt.Errorf("No live task membership could be inspected")
	}
	return out, nil
}
