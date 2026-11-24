package main

import "testing"

func TestHostMemoryRequiresAvailableAndDoesNotSubstituteFree(t *testing.T) {
	total, available, err := parseHostMemory("MemTotal: 2048 kB\nMemAvailable: 512 kB\nMemFree: 32 kB\n")
	if err != nil || total != 2097152 || available != 524288 {
		t.Fatalf("invalid parsed sample %d %d %v", total, available, err)
	}
	_, available, err = parseHostMemory("MemTotal: 2048 kB\nMemAvailable: 0 kB\n")
	if err != nil || available != 0 {
		t.Fatal("zero available memory must remain a valid pressure sample")
	}
	for _, data := range []string{
		"MemTotal: 2048 kB\nMemFree: 512 kB\n",
		"MemTotal: 2048 kB\nMemAvailable: 3000 kB\n",
		"MemTotal: 18446744073709551615 kB\nMemAvailable: 0 kB\n",
		"MemTotal: 2048 bytes\nMemAvailable: 512 kB\n",
	} {
		if _, _, err := parseHostMemory(data); err == nil {
			t.Fatalf("accepted invalid sample %q", data)
		}
	}
}
