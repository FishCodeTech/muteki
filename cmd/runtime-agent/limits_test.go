package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestParseDockerSize(t *testing.T) {
	cases := map[string]int64{
		"64m":  64 * 1024 * 1024,
		"2g":   2 * 1024 * 1024 * 1024,
		"512":  512,
		"1Gi":  1024 * 1024 * 1024,
		"32Mi": 32 * 1024 * 1024,
	}
	for raw, want := range cases {
		got, ok := parseDockerSize(raw)
		if !ok || got != want {
			t.Fatalf("parseDockerSize(%q)=%d,%v want %d,true", raw, got, ok, want)
		}
	}
	if _, ok := parseDockerSize(""); ok {
		t.Fatal("empty should fail")
	}
	if _, ok := parseDockerSize("0m"); ok {
		t.Fatal("zero should fail")
	}
}

func TestResolveByteLimitPrecedence(t *testing.T) {
	t.Setenv("MUTEKI_WORKER_OUTPUT_LIMIT", "32m")
	if got := resolveByteLimit(10, "MUTEKI_WORKER_OUTPUT_LIMIT", 99); got != 10 {
		t.Fatalf("explicit wins: got %d", got)
	}
	if got := resolveByteLimit(0, "MUTEKI_WORKER_OUTPUT_LIMIT", 99); got != 32*1024*1024 {
		t.Fatalf("env wins: got %d", got)
	}
	t.Setenv("MUTEKI_WORKER_OUTPUT_LIMIT", "")
	if got := resolveByteLimit(0, "MUTEKI_WORKER_OUTPUT_LIMIT", 99); got != 99 {
		t.Fatalf("fallback: got %d", got)
	}
}

func TestDirSizeBytes(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "a.txt"), bytes.Repeat([]byte("x"), 100), 0o644); err != nil {
		t.Fatal(err)
	}
	sub := filepath.Join(root, "sub")
	if err := os.Mkdir(sub, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sub, "b.txt"), bytes.Repeat([]byte("y"), 50), 0o644); err != nil {
		t.Fatal(err)
	}
	got := dirSizeBytes(root)
	if got != 150 {
		t.Fatalf("dirSizeBytes=%d want 150", got)
	}
}

// This subprocess executes the actual Worker pump/monitor, not a copied loop.
func TestLimitsChild(t *testing.T) {
	switch os.Getenv("MUTEKI_LIMIT_TEST_CHILD") {
	case "long-line":
		os.Stdout.WriteString(strings.Repeat("x", 6*1024*1024) + "\n")
		os.Exit(0)
	case "flood":
		os.Stdout.WriteString(strings.Repeat("x", 4*1024*1024))
		time.Sleep(10 * time.Second)
		os.Exit(0)
	case "fast-disk":
		if err := os.WriteFile("growth.bin", make([]byte, 1024*1024), 0600); err != nil {
			os.Exit(2)
		}
		os.Exit(0)
	}
}

func TestRealWorkerResourceLimits(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		name                 string
		output, disk         int64
		wantOutput, wantDisk bool
	}{
		{"long-line", 8 * 1024 * 1024, 4 * 1024 * 1024, false, false},
		{"flood", 128 * 1024, 4 * 1024 * 1024, true, false},
		{"fast-disk", 128 * 1024, 128 * 1024, false, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			w, frames, err := startWorkerWithRuntime(tc.name, &WorkerSpec{
				Argv: []string{executable, "-test.run=^TestLimitsChild$"}, Cwd: t.TempDir(),
				Env: map[string]string{"MUTEKI_LIMIT_TEST_CHILD": tc.name}, TimeoutSec: 15,
				OutputLimitBytes: tc.output, DiskLimitBytes: tc.disk,
			}, newChildReaper(), newOOMTracker(func() int { return 0 }))
			if err != nil {
				t.Fatal(err)
			}
			defer func() {
				w.mu.Lock()
				done := w.exited
				w.mu.Unlock()
				if !done {
					w.signalTree(syscall.SIGKILL)
				}
			}()
			var output int64
			deadline := time.After(20 * time.Second)
			for {
				select {
				case f, ok := <-frames:
					if !ok {
						t.Fatal("missing terminal result")
					}
					if f.T == "out" || f.T == "err" {
						output += int64(len(f.Line))
					}
					if f.T == "exit" {
						if f.OutputLimit != tc.wantOutput || f.DiskLimit != tc.wantDisk || f.TimedOut {
							t.Fatalf("unexpected exit: %+v", f)
						}
						if output > tc.output {
							t.Fatalf("output not bounded: %d", output)
						}
						if tc.name == "long-line" && output != 6*1024*1024 {
							t.Fatalf("long line lost: %d", output)
						}
						return
					}
				case <-deadline:
					t.Fatal("worker did not exit")
				}
			}
		})
	}
}
