package main

import (
	"crypto/rand"
	"encoding/hex"
	"strconv"
)

func itoa(n int) string { return strconv.Itoa(n) }

// shortRand returns a short random hex token for worker-id uniqueness. crypto/rand
// is fine here (not perf-critical, once per worker) and avoids the math/rand global
// seed concerns.
func shortRand() string {
	var b [4]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "0000"
	}
	return hex.EncodeToString(b[:])
}
