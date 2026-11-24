package main

import (
	"os"
	"strings"
)

func baseEnv() map[string]string {
	env := map[string]string{
		"PATH":             "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
		"HOME":             "/home/kali",
		"USER":             "root",
		"LOGNAME":          "root",
		"LANG":             "C.UTF-8",
		"PYTHONUNBUFFERED": "1",
		"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
	}
	for _, k := range []string{
		"TERM", "TZ", "MUTEKI_WORKSPACE_UID", "MUTEKI_WORKSPACE_GID",
		"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
		"http_proxy", "https_proxy", "all_proxy", "no_proxy",
	} {
		if v := os.Getenv(k); v != "" {
			env[k] = v
		}
	}
	return env
}

var proxyEnvKeys = map[string]bool{
	"HTTP_PROXY":  true,
	"HTTPS_PROXY": true,
	"ALL_PROXY":   true,
	"NO_PROXY":    true,
	"http_proxy":  true,
	"https_proxy": true,
	"all_proxy":   true,
	"no_proxy":    true,
}

func mergeSpecEnv(env, overlay map[string]string) {
	for k, v := range overlay {
		if proxyEnvKeys[k] && v == "" {
			delete(env, k)
			continue
		}
		env[k] = v
	}
}

func applyTokenFiles(env map[string]string) {
	pairs := []struct{ fileVar, valueVar string }{
		{"CLAUDE_CODE_OAUTH_TOKEN_FILE", "CLAUDE_CODE_OAUTH_TOKEN"},
		{"ANTHROPIC_AUTH_TOKEN_FILE", "ANTHROPIC_AUTH_TOKEN"},
		{"CURSOR_API_KEY_FILE", "CURSOR_API_KEY"},
		{"CURSOR_AUTH_TOKEN_FILE", "CURSOR_AUTH_TOKEN"},
		{"FACTORY_API_KEY_FILE", "FACTORY_API_KEY"},
		{"ANTHROPIC_API_KEY_FILE", "ANTHROPIC_API_KEY"},
		{"OPENAI_API_KEY_FILE", "OPENAI_API_KEY"},
		{"OPENCODE_API_KEY_FILE", "OPENCODE_API_KEY"},
		{"DEEPSEEK_API_KEY_FILE", "DEEPSEEK_API_KEY"},
		{"KIMI_MODEL_API_KEY_FILE", "KIMI_MODEL_API_KEY"},
		{"XAI_API_KEY_FILE", "XAI_API_KEY"},
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
