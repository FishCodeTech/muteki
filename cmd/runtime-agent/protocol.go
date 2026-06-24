package main

// Runtime Control Plane wire protocol — newline-delimited JSON over a single REVERSE
// connection (§8-9 of docs/DESIGN_worker_image_clean_rebuild.md). Dependency-free
// (encoding/json only) so the supervisor stays a single static binary.
//
// The host-side Python client (muteki/solver/control_client.py) speaks the exact
// same frames; any change here must be mirrored there.
//
// Topology (reverse-connect, forward-control):
//   - The supervisor does NOT listen on any port. At startup it DIALS the host's
//     control receiver (host.docker.internal:<port>) and sends a Hello frame with
//     {run_id, token}. The host validates the token and replies HelloAck.
//   - That one connection then carries ALL control for the run. The HOST is still
//     the command side: it sends Request frames (each tagged with a ReqID), the
//     supervisor executes and replies with Response/StreamEvent frames tagged with
//     the same ReqID / a WorkerID.
//   - Because there is exactly ONE connection per run (the container dials once),
//     multiple workers' streams are MULTIPLEXED on it via WorkerID on each frame.
//
// Why reverse: a listening supervisor (UDS or TCP) is either unreachable across the
// Docker Desktop VM (UDS) or an open network service the worker itself could drive
// (TCP). Dialing OUT to the host means the supervisor opens no port (the worker has
// no entry point to it), works across the VM (container→host is supported), and N
// runs all reach the host's single receiver port (no per-run published ports).
//
// Auth: the Hello frame's token is validated by the host against the per-run token
// it generated. NOT a boundary against the worker (trusted, runs as kali+sudo) —
// it just keeps a stray/duplicate connection from driving the wrong run.

// Op codes (host → supervisor).
const (
	OpStartWorker = "StartWorker"
	OpSignal      = "Signal"
	OpStatus      = "Status"
	OpTeardownRun = "TeardownRun"
	OpHealth      = "Health"
)

// Hello is the FIRST frame the supervisor sends after dialing the host receiver.
type Hello struct {
	Hello   int    `json:"hello"`            // protocol marker, always 1
	RunID   string `json:"run_id"`
	Token   string `json:"token"`
	Version string `json:"version,omitempty"`
}

// HelloAck is the host's reply to Hello.
type HelloAck struct {
	OK    bool   `json:"ok"`
	Error string `json:"error,omitempty"`
}

// Request is one command frame the HOST sends on the established connection.
type Request struct {
	Op    string `json:"op"`
	ReqID int64  `json:"req_id"` // correlation id; the reply echoes it

	// StartWorker
	Spec *WorkerSpec `json:"spec,omitempty"`

	// Signal / Status — the worker to act on.
	WorkerID string `json:"worker_id,omitempty"`
	// Signal — one of "STOP" | "CONT" | "TERM" | "KILL".
	Signal string `json:"signal,omitempty"`
}

// WorkerSpec is everything the supervisor needs to fork+exec a worker.
type WorkerSpec struct {
	Argv []string `json:"argv"` // resolved container-side argv (argv[0] = bin)
	Cwd  string   `json:"cwd"`  // absolute path inside the container
	// Env overlays the worker's environment (NOT the supervisor's). Only keys the
	// host chooses to pass arrive here; the supervisor adds nothing of its own
	// except a sane PATH/HOME default if absent.
	Env map[string]string `json:"env,omitempty"`
	// TimeoutSec is the authoritative wall-clock cap; the supervisor SIGKILLs the
	// worker tree at this many seconds (mirrors the old in-container `timeout -s KILL`).
	TimeoutSec int `json:"timeout_sec"`
	// Tag is an opaque per-worker label the host uses for its own bookkeeping; the
	// supervisor echoes it back in the Started reply for correlation.
	Tag string `json:"tag,omitempty"`
}

// Frame is the tagged union the supervisor sends back on the connection. Exactly one
// of the embedded shapes is meaningful, keyed by T. All carry ReqID (which command
// they answer) and, for worker output, WorkerID (which worker on the multiplexed
// connection).
//
//	T == "started"  -> StartWorker reply: WorkerID set (or Error on spawn failure)
//	T == "out"|"err" -> one raw line of worker stdout/stderr (Line), WorkerID set
//	T == "exit"     -> worker terminated: Rc/OOM/TimedOut/Signalled, WorkerID set
//	T == "resp"     -> generic Response payload (Signal/Status/Teardown/Health)
type Frame struct {
	T        string `json:"t"`
	ReqID    int64  `json:"req_id"`
	WorkerID string `json:"worker_id,omitempty"`
	Tag      string `json:"tag,omitempty"`

	// t == "out" | "err"
	Line string `json:"line,omitempty"`

	// t == "started"
	Error string `json:"error,omitempty"`

	// t == "exit"
	Rc        int  `json:"rc,omitempty"`
	OOM       bool `json:"oom,omitempty"`
	TimedOut  bool `json:"timed_out,omitempty"`
	Signalled int  `json:"signalled,omitempty"`

	// t == "resp" (Signal / Status / TeardownRun / Health)
	OK       bool   `json:"ok,omitempty"`
	State    string `json:"state,omitempty"` // Status: running | exited | timed_out | oom | unknown
	RcPtr    *int   `json:"rc_ptr,omitempty"`
	Paused   bool   `json:"paused,omitempty"`
	Version  string `json:"version,omitempty"` // Health
	Workers  int    `json:"workers,omitempty"` // Health: running worker count
	Uptime   int64  `json:"uptime_sec,omitempty"`
}
