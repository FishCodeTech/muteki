package main

import (
	"testing"
	"time"
)

func TestExecutionLeaseFencesExpiredAndUninitializedWork(t *testing.T) {
	now := time.Now()
	var local, remote executionLease
	if !local.admits(now) || local.expired(now.Add(time.Hour)) {
		t.Fatal("local work needs no remote lease")
	}
	if local.renew(now, 1000, now.Add(time.Second).UnixMilli()) == nil {
		t.Fatal("local runtime adopted remote lease")
	}
	remote.initialize(now)
	if remote.admits(now) {
		t.Fatal("bootstrap period must not authorize workers")
	}
	if err := remote.renew(now, 1000, now.Add(time.Second).UnixMilli()); err != nil {
		t.Fatal(err)
	}
	if !remote.admits(now) || remote.expired(now) {
		t.Fatal("fresh renewal not admitted")
	}
	if !remote.expired(now.Add(time.Second)) || remote.admits(now.Add(time.Second)) {
		t.Fatal("expired authority remains active")
	}
	if remote.renew(now.Add(time.Second), 30000, now.Add(time.Minute).UnixMilli()) == nil {
		t.Fatal("expired runtime revived")
	}
}

func TestExecutionLeaseRejectsBufferedAndUnboundedRenewals(t *testing.T) {
	now := time.Now()
	var lease executionLease
	lease.initialize(now)
	for _, millis := range []int64{-1, 0, 30001} {
		if lease.renew(now, millis, now.Add(time.Minute).UnixMilli()) == nil {
			t.Fatalf("accepted duration %d", millis)
		}
	}
	if lease.renew(now, 30000, now.Add(-time.Second).UnixMilli()) == nil {
		t.Fatal("accepted buffered expired renewal")
	}
	if err := lease.renew(now, 30000, now.Add(time.Second).UnixMilli()); err != nil {
		t.Fatal(err)
	}
	if !lease.expired(now.Add(time.Second)) {
		t.Fatal("relative TTL extended absolute authority")
	}
}
