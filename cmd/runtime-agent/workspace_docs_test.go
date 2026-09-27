package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestManagedWorkspaceDocMarker(t *testing.T) {
	if !managedWorkspaceDoc([]byte("<!-- muteki-workspace-doc:1 -->\n# hi\n")) {
		t.Fatal("expected managed marker")
	}
	if managedWorkspaceDoc([]byte("# operator owned\n")) {
		t.Fatal("operator file must not look managed")
	}
}

func TestSeedManagedWorkspaceDoc(t *testing.T) {
	dir := t.TempDir()
	srcV1 := []byte("<!-- muteki-workspace-doc:1 -->\nv1 private cwd\n")
	srcV2 := []byte("<!-- muteki-workspace-doc:1 -->\nv2 private cwd + shared/\n")

	action, err := seedManagedWorkspaceDoc(dir, "AGENTS.md", srcV1)
	if err != nil || action != "seeded" {
		t.Fatalf("seed: action=%q err=%v", action, err)
	}
	got, _ := os.ReadFile(filepath.Join(dir, "AGENTS.md"))
	if string(got) != string(srcV1) {
		t.Fatalf("seeded body mismatch: %q", got)
	}

	action, err = seedManagedWorkspaceDoc(dir, "AGENTS.md", srcV1)
	if err != nil || action != "unchanged" {
		t.Fatalf("unchanged: action=%q err=%v", action, err)
	}

	action, err = seedManagedWorkspaceDoc(dir, "AGENTS.md", srcV2)
	if err != nil || action != "upgraded" {
		t.Fatalf("upgrade: action=%q err=%v", action, err)
	}
	got, _ = os.ReadFile(filepath.Join(dir, "AGENTS.md"))
	if string(got) != string(srcV2) {
		t.Fatalf("upgraded body mismatch: %q", got)
	}
	if !strings.Contains(string(got), "shared/") {
		t.Fatal("upgraded doc should describe shared/")
	}

	opDir := t.TempDir()
	opPath := filepath.Join(opDir, "AGENTS.md")
	if err := os.WriteFile(opPath, []byte("operator custom notes\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	action, err = seedManagedWorkspaceDoc(opDir, "AGENTS.md", srcV2)
	if err != nil || action != "skip-operator" {
		t.Fatalf("operator: action=%q err=%v", action, err)
	}
	got, _ = os.ReadFile(opPath)
	if string(got) != "operator custom notes\n" {
		t.Fatalf("operator file was clobbered: %q", got)
	}
}

func TestSeedDoesNotFollowLinksOrQuotedMarkers(t *testing.T) {
	dir := t.TempDir()
	src := []byte("<!-- muteki-workspace-doc:2 -->\nupdated\n")
	outside := filepath.Join(t.TempDir(), "operator.txt")
	original := []byte("<!-- muteki-workspace-doc:1 -->\nkeep external\n")
	os.WriteFile(outside, original, 0600)
	os.Symlink(outside, filepath.Join(dir, "AGENTS.md"))
	action, err := seedManagedWorkspaceDoc(dir, "AGENTS.md", src)
	if err != nil || action != "skip-operator" {
		t.Fatalf("symlink: %s %v", action, err)
	}
	got, _ := os.ReadFile(outside)
	if string(got) != string(original) {
		t.Fatal("external file overwritten")
	}
	if managedWorkspaceDoc([]byte("operator notes quote <!-- muteki-workspace-doc:1 -->")) {
		t.Fatal("embedded marker is not ownership")
	}
}
