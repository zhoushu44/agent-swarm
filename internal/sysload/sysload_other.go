//go:build !windows && !linux

package sysload

// Platforms without an implementation report ErrUnsupported. Guard.Allow then
// degrades to "allow" so an unreadable counter never stalls the swarm.

func readCPUTimes() (cpuTimes, error) { return cpuTimes{}, ErrUnsupported }

func readMem() (memInfo, error) { return memInfo{}, ErrUnsupported }
