package main


const (
	OpStartWorker = "StartWorker"
	OpWriteStdin  = "WriteStdin"
	OpSignal      = "Signal"
	OpStatus      = "Status"
	OpTeardownRun = "TeardownRun"
	OpRegisterRun = "RegisterRun"
	OpHealth       = "Health"
	OpRenewLease   = "RenewLease"
	OpVerifyOutput = "VerifyOutput"
)

type Hello struct {
	Hello   int    `json:"hello"`
	RunID   string `json:"run_id"`
	Token   string `json:"token"`
	Version string `json:"version,omitempty"`
}

type HelloAck struct {
	OK    bool   `json:"ok"`
	Error string `json:"error,omitempty"`
}

type Request struct {
	LeaseMillis    int64  `json:"lease_millis,omitempty"`
	LeaseExpiresAt int64  `json:"lease_expires_at,omitempty"`
	Op             string `json:"op"`
	ReqID          int64  `json:"req_id"`

	Spec *WorkerSpec `json:"spec,omitempty"`

	WorkerID string `json:"worker_id,omitempty"`
	Data string `json:"data,omitempty"`
	OwnerRunID     string `json:"owner_run_id,omitempty"`
	OwnerToken     string `json:"owner_token,omitempty"`
	OwnerWorkspace string `json:"owner_workspace,omitempty"`
	Signal string `json:"signal,omitempty"`
	Needle string `json:"needle,omitempty"`
}

type WorkerSpec struct {
	OwnerRunID     string   `json:"owner_run_id,omitempty"`
	OwnerToken     string   `json:"owner_token,omitempty"`
	OwnerWorkspace string   `json:"owner_workspace,omitempty"`
	Argv           []string `json:"argv"`
	Cwd            string   `json:"cwd"`
	Stdin string `json:"stdin,omitempty"`
	StdinMode string `json:"stdin_mode,omitempty"`
	Env map[string]string `json:"env,omitempty"`
	TimeoutSec int `json:"timeout_sec"`
	Tag string `json:"tag,omitempty"`
	OutputLimitBytes int64 `json:"output_limit_bytes,omitempty"`
	DiskLimitBytes int64 `json:"disk_limit_bytes,omitempty"`
}

type Frame struct {
	T        string `json:"t"`
	ReqID    int64  `json:"req_id"`
	WorkerID string `json:"worker_id,omitempty"`
	Tag      string `json:"tag,omitempty"`

	Line string `json:"line,omitempty"`

	Error string `json:"error,omitempty"`

	Rc          int  `json:"rc,omitempty"`
	OOM         bool `json:"oom,omitempty"`
	TimedOut    bool `json:"timed_out,omitempty"`
	OutputLimit bool `json:"output_limit,omitempty"`
	DiskLimit   bool `json:"disk_limit,omitempty"`
	Signalled   int  `json:"signalled,omitempty"`

	OK                  bool   `json:"ok,omitempty"`
	State               string `json:"state,omitempty"`
	RcPtr               *int   `json:"rc_ptr,omitempty"`
	Paused              bool   `json:"paused,omitempty"`
	Version             string `json:"version,omitempty"`
	ExecutionLease      bool   `json:"execution_lease,omitempty"`
	ScopedProcesses     bool   `json:"scoped_processes,omitempty"`
	ScopedTeardown      bool   `json:"scoped_teardown,omitempty"`
	Workers             int    `json:"workers,omitempty"`
	Uptime              int64  `json:"uptime_sec,omitempty"`
	MemorySampled       bool   `json:"memory_sampled,omitempty"`
	HostMemoryTotal     uint64 `json:"host_memory_total_bytes,omitempty"`
	HostMemoryAvailable uint64 `json:"host_memory_available_bytes,omitempty"`
	MatchFound bool   `json:"match_found,omitempty"`
	RingBytes  int64  `json:"ring_bytes,omitempty"`
	RingSHA256 string `json:"ring_sha256,omitempty"`
	Excerpt    string `json:"excerpt,omitempty"`
}
