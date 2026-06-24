# Security Policy

## Reporting a Vulnerability

Please report security vulnerabilities **privately** via GitHub Security Advisories:

> **[Report a vulnerability](https://github.com/FishCodeTech/muteki/security/advisories/new)**
> (repo → *Security* tab → *Report a vulnerability*)

Do **not** open a public issue for security problems. We will acknowledge your
report and work with you on a fix and coordinated disclosure.

---

## Runtime Trust Boundary

**Muteki is an offensive security automation tool.** It drives CLI coding agents to
execute commands, run security tooling, and reach target services in order to solve
CTF challenges. Understand the trust model before you run it.

### What Muteki does NOT promise

- **It does not isolate malicious challenges.** Strong sandboxing (malicious-code
  isolation, microVM/Firecracker, untrusted-input confinement) is an explicit
  **non-goal / accepted risk** of this project — not a missing feature. The worker
  executes content returned by the challenge target with privileges (the worker user
  has `NOPASSWD` sudo so it can install tools / change the system while solving), and
  the in-container control channel is explicitly **not** a security boundary against
  the worker.
- **The container worker backend is NOT a security sandbox.** Its purpose is a
  consistent Kali/CTF toolchain, a VPS / standalone-machine deployment form, and
  runtime-dependency / workspace / credential-mount isolation — **not** containment of
  untrusted code. Do not rely on it to protect the host from a malicious challenge.

### How to run it safely

> ⚠️ **Run Muteki only in a dedicated, disposable environment** — a dedicated VPS, a
> throwaway VM, or a standalone machine with no sensitive data. **Do not run it on
> your primary workstation, a shared host, or a production environment.**

Treat the machine running Muteki as you would treat a machine you are pointing
offensive tooling *from*: assume any challenge could attempt to execute code through
the worker, and isolate accordingly.

### Other things to be aware of

- **Flag verification trust assumption.** The provenance gate (`muteki/solver/gate.py`)
  guards against the *model hallucinating* a flag — it does **not** guard against a
  *malicious challenge fabricating* a format-matching flag. For competitive use, verify
  accepted flags against the contest scoreboard, and prefer high-entropy flag formats
  (e.g. `flag{<sha256>}`) over short, brute-forceable ones.
- **Credentials.** Muteki reads engine credentials (OAuth tokens / API keys / the
  codex `auth.json`) from the macOS Keychain or environment and projects them into the
  worker environment. These are passed to the proprietary engine CLIs
  (`claude` / `codex` / `cursor-agent`), which transmit data to their
  respective providers (Anthropic / OpenAI / Cursor). Credential stores live under
  gitignored paths (`sessions/_secrets/`, `.env`) and are never committed; verify your
  own deployment keeps them out of version control.
- **Control plane.** In container mode the in-container supervisor dials back to a
  host-local control receiver (`127.0.0.1`, default port `9100`, per-run token). This
  is part of normal operation; be aware the control receiver listens on host loopback
  when deploying on a shared host.

## Supported Versions

This project is pre-1.0 and moves fast. Security fixes target the `main` branch.
