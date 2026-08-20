package main

import (
	"sync"
	"syscall"
)

// oomObservation records container-level cgroup evidence over one worker's
// lifetime. The cgroup counter does not identify the victim PID, so overlapping
// observations are permanently ambiguous: a delta must never be copied onto every
// concurrent worker as though it were per-process evidence.
type oomObservation struct {
	before    int
	ambiguous bool
}

type oomEvidence struct {
	delta     int
	ambiguous bool
}

type oomTracker struct {
	mu     sync.Mutex
	read   func() int
	active map[*oomObservation]struct{}
}

func newOOMTracker(read func() int) *oomTracker {
	return &oomTracker{read: read, active: make(map[*oomObservation]struct{})}
}

var runtimeOOMTracker = newOOMTracker(readOOMKill)

func (t *oomTracker) begin() *oomObservation {
	t.mu.Lock()
	defer t.mu.Unlock()
	obs := &oomObservation{before: t.read()}
	if len(t.active) != 0 {
		obs.ambiguous = true
		for other := range t.active {
			other.ambiguous = true
		}
	}
	t.active[obs] = struct{}{}
	return obs
}

func (t *oomTracker) cancel(obs *oomObservation) {
	t.mu.Lock()
	delete(t.active, obs)
	t.mu.Unlock()
}

func (t *oomTracker) finish(obs *oomObservation) oomEvidence {
	t.mu.Lock()
	defer t.mu.Unlock()
	after := t.read()
	delete(t.active, obs)
	delta := 0
	if obs.before >= 0 && after > obs.before {
		delta = after - obs.before
	}
	return oomEvidence{delta: delta, ambiguous: obs.ambiguous}
}

// attributable is deliberately conservative. A container counter delta is an OOM
// label only when one worker owned the whole observation window, that worker died
// by SIGKILL, and the supervisor did not itself issue the KILL (timeout/operator).
func (e oomEvidence) attributable(sig int, timedOut, killRequested bool) bool {
	return e.delta > 0 && !e.ambiguous && sig == int(syscall.SIGKILL) && !timedOut && !killRequested
}
