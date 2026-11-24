# Security Policy

## Authorization and Third-Party Conduct

Access to Muteki's open-source code does not grant permission to test someone else's systems. Third-party unauthorized attacks are the actions of those carrying them out, who bear responsibility for those actions and their consequences. The author and this project disclaim all responsibility and liability for those third-party actions and do not authorize or endorse them. Read the [disclaimer and unauthorized-use responsibility statement (中文)](DISCLAIMER.md) alongside this security policy and the unchanged [LICENSE](LICENSE).

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

- **It does not isolate malicious challenges.** The default Docker runtime is not a malicious-code or microVM isolation guarantee. Two worker
  privilege modes exist: the **default** mode starts the container with
  `--security-opt no-new-privileges=true`, so although the image ships a
  `kali ALL=(ALL) NOPASSWD:ALL` sudo entry it cannot be used to elevate —
  the worker runs as unprivileged `kali`. An explicit **elevated** mode
  (`worker_privilege=elevated`) is the operator opt-in that keeps that
  unrestricted passwordless sudo: the worker can install tools, change the
  system and read every file and credential inside the container.
  The in-container control channel is explicitly **not** a security boundary
  against the worker.
- **Workers in one container share a trust domain.** All Workers of a Run (and,
  with shared container scope, of every Run in that pool) run as the same system
  user. They can read each other's projected credentials and working files. Do not
  put mutually untrusted accounts or Runs into the same Run or shared pool.
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

- **Flag verification trust assumption.** The provenance checks in `crates/server/src/runs/`
  guards against the *model hallucinating* a flag — it does **not** guard against a
  *malicious challenge fabricating* a format-matching flag. For competitive use, verify
  accepted flags against the contest scoreboard, and prefer high-entropy flag formats
  (e.g. `flag{<sha256>}`) over short, brute-forceable ones.
- **Credentials.** Host CLIs and selected accounts supply model credentials. Keep the Rust state directory (`MUTEKI_STATE_DIR`) and account projections private and out of version control. Container Workers need explicitly injectable credentials; a host CLI login is not automatically shared. Selected providers and CLIs may transmit prompts and files to their services.
- **Control plane.** Web delegates container control to an independent Runner. The local connection uses controlled IPC; remote nodes use authenticated TLS/WSS and execution leases. Only the Compose Runner service mounts the Docker socket. The Runner remains a privileged host component; its access must be restricted. Authentication does not make a shared Worker container a strong sandbox. Runtime selection and platform support require their own verification through the published workflow results.
- **Remote Runner TLS.** The remote node listener uses a private CA generated per installation (`<state>/remote-runner-tls/`); enrollment commands pin its SHA-256 fingerprint, and the Runner trusts only that CA, not the system roots. Enrollment tokens are single-use and expire within 15 minutes; node credentials last 30 days and can be rotated or revoked from the settings page. Anyone holding the CA key or a node credential can impersonate that side; keep the state directory private. Expose the listener only on networks that need it.

## Supported Versions

This project is pre-1.0 and moves fast. Security fixes target the `main` branch.
