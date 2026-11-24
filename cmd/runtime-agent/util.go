package main

import (
	"crypto/rand"
	"encoding/hex"
	"strconv"
)

func itoa(n int) string { return strconv.Itoa(n) }

func shortRand() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		return ""
	}
	return hex.EncodeToString(b[:])
}
