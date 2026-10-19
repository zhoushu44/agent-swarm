//go:build linux

package sysload

import (
	"bufio"
	"fmt"
	"os"
	"strconv"
	"strings"
)

// readCPUTimes parses the aggregate "cpu" line of /proc/stat, which reports
// cumulative jiffies: user nice system idle iowait irq softirq steal guest
// guest_nice. Idle is idle+iowait; total is the sum of the first eight fields
// (guest time is already counted inside user/nice, so it must not be added).
func readCPUTimes() (cpuTimes, error) {
	f, err := os.Open("/proc/stat")
	if err != nil {
		return cpuTimes{}, fmt.Errorf("sysload: open /proc/stat: %w", err)
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		line := scanner.Text()
		if !strings.HasPrefix(line, "cpu ") {
			continue
		}
		fields := strings.Fields(line)[1:]
		if len(fields) < 4 {
			return cpuTimes{}, fmt.Errorf("sysload: malformed /proc/stat cpu line: %q", line)
		}
		var vals [8]uint64
		for i := 0; i < len(vals) && i < len(fields); i++ {
			v, convErr := strconv.ParseUint(fields[i], 10, 64)
			if convErr != nil {
				return cpuTimes{}, fmt.Errorf("sysload: parse /proc/stat field %d: %w", i, convErr)
			}
			vals[i] = v
		}
		var total uint64
		for _, v := range vals {
			total += v
		}
		return cpuTimes{Idle: vals[3] + vals[4], Total: total}, nil
	}
	if err := scanner.Err(); err != nil {
		return cpuTimes{}, fmt.Errorf("sysload: read /proc/stat: %w", err)
	}
	return cpuTimes{}, fmt.Errorf("sysload: /proc/stat has no aggregate cpu line")
}

// readMem parses MemTotal and MemAvailable (kB) from /proc/meminfo.
func readMem() (memInfo, error) {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		return memInfo{}, fmt.Errorf("sysload: open /proc/meminfo: %w", err)
	}
	defer f.Close()

	var info memInfo
	var haveTotal, haveAvail bool
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) < 2 {
			continue
		}
		kb, convErr := strconv.ParseUint(fields[1], 10, 64)
		if convErr != nil {
			continue
		}
		switch strings.TrimSuffix(fields[0], ":") {
		case "MemTotal":
			info.Total = kb * 1024
			haveTotal = true
		case "MemAvailable":
			info.Available = kb * 1024
			haveAvail = true
		}
	}
	if err := scanner.Err(); err != nil {
		return memInfo{}, fmt.Errorf("sysload: read /proc/meminfo: %w", err)
	}
	if !haveTotal || !haveAvail {
		return memInfo{}, fmt.Errorf("sysload: /proc/meminfo missing MemTotal or MemAvailable")
	}
	return info, nil
}
