package main

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestSharedScopeWorkerChild(t *testing.T) {
	if os.Getenv("MUTEKI_SHARED_SCOPE_TEST_CHILD") == "1" {
		time.Sleep(30 * time.Second)
	}
}

func TestSharedScopeOwnerLifecycle(t *testing.T) {
	root := t.TempDir()
	a := filepath.Join(root, "run-a")
	b := filepath.Join(root, "run-b")
	for _, slot := range []string{a, b} {
		if err := os.Mkdir(slot, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	s := &supervisor{
		runID: "shared-runtime-test", sharedPool: true, workspace: root,
		workers: map[string]*worker{}, owners: map[string]sharedOwner{},
		enc: json.NewEncoder(io.Discard),
	}
	tokenA := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	tokenB := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	s.opRegisterRun(&Request{OwnerRunID: "a", OwnerToken: tokenA, OwnerWorkspace: a})
	s.opRegisterRun(&Request{OwnerRunID: "b", OwnerToken: tokenB, OwnerWorkspace: b})
	if len(s.owners) != 2 {
		t.Fatal("both trusted Runs must be registered")
	}
	s.opRegisterRun(&Request{OwnerRunID: "outside", OwnerToken: tokenA, OwnerWorkspace: t.TempDir()})
	if len(s.owners) != 2 {
		t.Fatal("outside workspace was registered")
	}
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	start := func(id, token, cwd string) {
		s.opStartWorker(&Request{Spec: &WorkerSpec{
			OwnerRunID: id, OwnerToken: token, OwnerWorkspace: cwd,
			Argv: []string{exe, "-test.run=^TestSharedScopeWorkerChild$"},
			Cwd:  cwd, Env: map[string]string{"MUTEKI_SHARED_SCOPE_TEST_CHILD": "1"},
			TimeoutSec: 40,
		}})
	}
	start("a", tokenA, a)
	start("b", tokenB, b)
	t.Cleanup(func() { s.killAll() })
	if len(s.workers) != 2 {
		t.Fatalf("started workers=%d, want 2", len(s.workers))
	}
	if s.killOwned("a", "wrong-token") {
		t.Fatal("wrong owner token stopped a Run")
	}
	if !s.killOwned("a", tokenA) {
		t.Fatal("scoped teardown did not stop Run a")
	}
	if !s.killOwned("a", "") {
		t.Fatal("repeated teardown should confirm an inactive owner")
	}
	for _, w := range s.workers {
		state, _, _, _, _ := w.status()
		if w.ownerRunID == "a" && state == "running" {
			t.Fatal("Run a still has a managed worker")
		}
		if w.ownerRunID == "b" && state != "running" {
			t.Fatal("Run b was stopped with Run a")
		}
	}
	before := s.seq
	start("a", tokenA, a)
	if s.seq != before {
		t.Fatal("a late start reactivated a torn-down Run")
	}
	if !s.killOwned("b", tokenB) {
		t.Fatal("Run b did not stop")
	}
}
