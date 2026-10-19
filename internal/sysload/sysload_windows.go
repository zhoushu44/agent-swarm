//go:build windows

package sysload

import (
	"fmt"
	"syscall"
	"unsafe"
)

var (
	kernel32                 = syscall.NewLazyDLL("kernel32.dll")
	procGetSystemTimes       = kernel32.NewProc("GetSystemTimes")
	procGlobalMemoryStatusEx = kernel32.NewProc("GlobalMemoryStatusEx")
)

// filetime mirrors the Windows FILETIME structure (100ns ticks, two DWORDs).
type filetime struct {
	Low  uint32
	High uint32
}

// ticks folds the two halves into a single counter value.
func (f filetime) ticks() uint64 { return uint64(f.High)<<32 | uint64(f.Low) }

// memoryStatusEx mirrors MEMORYSTATUSEX from sysinfoapi.h.
type memoryStatusEx struct {
	Length               uint32
	MemoryLoad           uint32
	TotalPhys            uint64
	AvailPhys            uint64
	TotalPageFile        uint64
	AvailPageFile        uint64
	TotalVirtual         uint64
	AvailVirtual         uint64
	AvailExtendedVirtual uint64
}

// readCPUTimes returns cumulative idle and total CPU ticks reported by
// GetSystemTimes. Per the Win32 documentation the kernel time already
// includes idle time, so total = kernel + user.
func readCPUTimes() (cpuTimes, error) {
	if err := procGetSystemTimes.Find(); err != nil {
		return cpuTimes{}, fmt.Errorf("sysload: GetSystemTimes unavailable: %w", err)
	}
	var idle, kernel, user filetime
	r1, _, callErr := procGetSystemTimes.Call(
		uintptr(unsafe.Pointer(&idle)),
		uintptr(unsafe.Pointer(&kernel)),
		uintptr(unsafe.Pointer(&user)),
	)
	if r1 == 0 {
		return cpuTimes{}, fmt.Errorf("sysload: GetSystemTimes failed: %w", callErr)
	}
	return cpuTimes{
		Idle:  idle.ticks(),
		Total: kernel.ticks() + user.ticks(),
	}, nil
}

// readMem returns physical memory counters from GlobalMemoryStatusEx.
func readMem() (memInfo, error) {
	if err := procGlobalMemoryStatusEx.Find(); err != nil {
		return memInfo{}, fmt.Errorf("sysload: GlobalMemoryStatusEx unavailable: %w", err)
	}
	var status memoryStatusEx
	status.Length = uint32(unsafe.Sizeof(status))
	r1, _, callErr := procGlobalMemoryStatusEx.Call(uintptr(unsafe.Pointer(&status)))
	if r1 == 0 {
		return memInfo{}, fmt.Errorf("sysload: GlobalMemoryStatusEx failed: %w", callErr)
	}
	return memInfo{Total: status.TotalPhys, Available: status.AvailPhys}, nil
}
