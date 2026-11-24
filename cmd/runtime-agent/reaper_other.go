//go:build !linux

package main

func (r *childReaper) reapOrphans() {}
