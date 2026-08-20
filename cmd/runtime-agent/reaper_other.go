//go:build !linux

package main

// runtime-agent is built for Linux. Keep host-side unit tests portable without
// falling back to Wait4(-1), which would recreate the managed-child race.
func (r *childReaper) reapOrphans() {}
