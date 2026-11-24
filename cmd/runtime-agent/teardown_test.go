package main

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
)

func TestSharedTeardownRequiresOwnedScopeExitEvidence(t *testing.T) {
	var buf bytes.Buffer
	tokenA := strings.Repeat("a", 32)
	tokenB := strings.Repeat("b", 32)
	s := &supervisor{
		sharedPool: true,
		workspace:  "/home/kali/workspace/runs",
		workers:    map[string]*worker{},
		owners: map[string]sharedOwner{
			"run-a": {token: tokenA, workspace: "/home/kali/workspace/runs/run-a"},
			"run-b": {token: tokenB, workspace: "/home/kali/workspace/runs/run-b"},
		},
		enc: json.NewEncoder(&buf),
	}

	s.dispatch(&Request{
		Op:         OpTeardownRun,
		ReqID:      7,
		OwnerRunID: "run-a",
		OwnerToken: tokenA,
	})

	var frame Frame
	if err := json.NewDecoder(&buf).Decode(&frame); err != nil {
		t.Fatal(err)
	}
	if frame.OK || !frame.ScopedTeardown || frame.ReqID != 7 {
		t.Fatalf("missing scope exit evidence was accepted: %+v", frame)
	}
	if _, still := s.owners["run-a"]; !still {
		t.Fatal("unconfirmed Run was removed")
	}
	if _, kept := s.owners["run-b"]; !kept {
		t.Fatal("the other run was removed")
	}
}
