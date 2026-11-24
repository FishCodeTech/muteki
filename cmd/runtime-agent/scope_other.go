//go:build !linux || (!amd64 && !arm64)

package main

import (
	"fmt"
	"time"
)

func enableRunSubreaper() error              { return fmt.Errorf("Run subreaper requires Linux amd64/arm64") }
func supportsRunSubreaper() bool             { return false }
func stopRunDescendants(time.Duration) error { return enableRunSubreaper() }
