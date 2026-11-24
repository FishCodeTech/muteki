package main

import (
	"fmt"
	"math"
	"os"
	"strconv"
	"strings"
)

func hostMemory() (uint64, uint64, bool) {
	data, err := os.ReadFile("/proc/meminfo")
	if err != nil {
		return 0, 0, false
	}
	total, available, err := parseHostMemory(string(data))
	return total, available, err == nil
}
func parseHostMemory(data string) (uint64, uint64, error) {
	values := make(map[string]uint64)
	for _, line := range strings.Split(data, "\n") {
		fields := strings.Fields(line)
		if len(fields) == 0 || (fields[0] != "MemTotal:" && fields[0] != "MemAvailable:") {
			continue
		}
		if len(fields) != 3 || fields[2] != "kB" {
			return 0, 0, fmt.Errorf("invalid memory sample units")
		}
		value, err := strconv.ParseUint(fields[1], 10, 64)
		if err != nil || value > math.MaxUint64/1024 {
			return 0, 0, fmt.Errorf("invalid memory sample value")
		}
		values[fields[0]] = value * 1024
	}
	total, totalOK := values["MemTotal:"]
	available, availableOK := values["MemAvailable:"]
	if !totalOK || !availableOK || total == 0 || available > total {
		return 0, 0, fmt.Errorf("memory sample unavailable or inconsistent")
	}
	return total, available, nil
}
