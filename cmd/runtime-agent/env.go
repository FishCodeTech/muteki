package main

import (
	"os"
	"strings"
)

// baseEnv is the minimal environment a worker starts with before the host's overlay.
// We keep it small and deterministic; the host passes the engine-specific vars
// (MUTEKI_*, ANTHROPIC_*, CLAUDE_*, CODEX_*, CURSOR_*, OPENAI_*, HOME) explicitly.
func baseEnv() map[string]string {
	env := map[string]string{
		// cursor-agent installs to ~/.local/bin which is NOT on a non-login PATH —
		// include it so `cursor-agent` resolves (the old container bug). claude/codex
		// live in /usr/local/bin.
		"PATH": "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
		"HOME": "/home/kali",
		"USER": "kali",
		"LOGNAME": "kali",
		"LANG": "C.UTF-8",
		"PYTHONUNBUFFERED": "1",
		"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
	}
	// Inherit a TERM/TZ if the supervisor has one.
	for _, k := range []string{"TERM", "TZ"} {
		if v := os.Getenv(k); v != "" {
			env[k] = v
		}
	}
	return env
}

// applyTokenFiles resolves the *_FILE → value indirection the old docker-exec shell
// prelude did. The account projection mounts secrets as files and passes their paths
// as <VAR>_FILE; the CLIs want the literal value in <VAR>. If the file is readable,
// its trimmed contents become the env var (host overlay still wins if it already set
// the bare var).
func applyTokenFiles(env map[string]string) {
	pairs := []struct{ fileVar, valueVar string }{
		{"CLAUDE_CODE_OAUTH_TOKEN_FILE", "CLAUDE_CODE_OAUTH_TOKEN"},
		{"CURSOR_API_KEY_FILE", "CURSOR_API_KEY"},
		{"ANTHROPIC_API_KEY_FILE", "ANTHROPIC_API_KEY"},
		{"OPENAI_API_KEY_FILE", "OPENAI_API_KEY"},
	}
	for _, p := range pairs {
		if _, already := env[p.valueVar]; already {
			continue
		}
		path := env[p.fileVar]
		if path == "" {
			continue
		}
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		val := strings.TrimSpace(string(data))
		if val != "" {
			env[p.valueVar] = val
		}
	}
}

func flattenEnv(env map[string]string) []string {
	out := make([]string, 0, len(env))
	for k, v := range env {
		out = append(out, k+"="+v)
	}
	return out
}
