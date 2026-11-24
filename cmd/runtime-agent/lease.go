package main

import (
	"errors"
	"sync"
	"time"
)

type executionLease struct {
	mu       sync.Mutex
	required bool
	active   bool
	deadline time.Time
}

func (l *executionLease) initialize(now time.Time) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.required = true
	l.deadline = now.Add(30 * time.Second)
}

func (l *executionLease) expired(now time.Time) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.required && !now.Before(l.deadline)
}

func (l *executionLease) admits(now time.Time) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	return !l.required || (l.active && now.Before(l.deadline))
}

func (l *executionLease) renew(now time.Time, millis, expiresAt int64) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if !l.required || !now.Before(l.deadline) {
		return errors.New("runtime lease is disabled or expired")
	}
	if millis <= 0 || millis > 30000 {
		return errors.New("invalid runtime lease duration")
	}
	duration := time.Duration(millis) * time.Millisecond
	remaining := time.UnixMilli(expiresAt).Sub(now)
	if remaining <= 0 {
		return errors.New("stale runtime lease renewal")
	}
	if remaining < duration {
		duration = remaining
	}
	l.deadline = now.Add(duration)
	l.active = true
	return nil
}
