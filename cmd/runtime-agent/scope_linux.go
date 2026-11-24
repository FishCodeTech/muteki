//go:build linux && (amd64 || arm64)

package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const pidfdOpenTrap = 434
const pidfdSignalTrap = 424

func enableRunSubreaper() error {
	if _, _, errno := syscall.Syscall6(syscall.SYS_PRCTL, 36, 1, 0, 0, 0, 0); errno != 0 {
		return errno
	}
	fd, _, errno := syscall.Syscall(pidfdOpenTrap, uintptr(os.Getpid()), 0, 0)
	if errno != 0 {
		return fmt.Errorf("pidfd_open required for scoped cleanup: %w", errno)
	}
	defer syscall.Close(int(fd))
	if _, _, errno = syscall.Syscall6(pidfdSignalTrap, fd, 0, 0, 0, 0, 0); errno != 0 {
		return fmt.Errorf("pidfd_send_signal required for scoped cleanup: %w", errno)
	}
	return nil
}
func supportsRunSubreaper() bool { return true }

func killDirectChild(pid int) error {
	fd, _, errno := syscall.Syscall(pidfdOpenTrap, uintptr(pid), 0, 0)
	if errno == syscall.ESRCH {
		return nil
	}
	if errno == syscall.EINVAL {
		_, err := os.ReadFile(filepath.Join("/proc", strconv.Itoa(pid), "status"))
		if os.IsNotExist(err) || errors.Is(err, syscall.ESRCH) {
			return nil
		}
	}
	if errno != 0 {
		return fmt.Errorf("pidfd_open child %d: %w", pid, errno)
	}
	defer syscall.Close(int(fd))
	status, err := os.ReadFile(filepath.Join("/proc", strconv.Itoa(pid), "status"))
	if os.IsNotExist(err) || errors.Is(err, syscall.ESRCH) {
		return nil
	}
	if err != nil {
		return err
	}
	parent := -1
	for _, line := range strings.Split(string(status), "\n") {
		if strings.HasPrefix(line, "PPid:") {
			fields := strings.Fields(line)
			if len(fields) == 2 {
				parent, _ = strconv.Atoi(fields[1])
			}
			break
		}
	}
	if parent != os.Getpid() {
		return nil
	}
	if _, _, errno = syscall.Syscall6(pidfdSignalTrap, fd, uintptr(syscall.SIGKILL), 0, 0, 0, 0); errno != 0 && errno != syscall.ESRCH {
		return fmt.Errorf("pidfd_send_signal child %d: %w", pid, errno)
	}
	return nil
}
func stopRunDescendants(timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for {
		children, err := directChildPIDsChecked()
		if err != nil {
			return err
		}
		for _, pid := range children {
			if err := killDirectChild(pid); err != nil {
				return err
			}
		}
		runtimeChildReaper.reapOrphans()
		remaining, err := directChildPIDsChecked()
		if err != nil {
			return err
		}
		if len(remaining) == 0 {
			return nil
		}
		if !time.Now().Before(deadline) {
			return fmt.Errorf("Run descendants did not stop before deadline")
		}
		time.Sleep(10 * time.Millisecond)
	}
}
