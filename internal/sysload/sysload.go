// Package sysload samples host CPU and memory load and enforces a "guard
// ceiling": before the swarm spawns new work it asks the guard whether the
// machine still has headroom. When sampled CPU or memory usage sits above the
// configured ceiling the guard refuses new work instead of piling on more
// processes.
//
// The package is dependency free: platform specific primitives live in
// build-tagged files (sysload_windows.go, sysload_linux.go, sysload_other.go)
// and expose only two functions, readCPUTimes and readMem. Everything else is
// portable, pure logic that can be unit tested with a fake sampler.
package sysload

import (
	"errors"
	"fmt"
	"sync"
	"time"
)

// Defaults applied when the matching Config field is left at its zero value.
const (
	DefaultMaxCPUPercent = 85.0
	DefaultMaxMemPercent = 90.0
	DefaultMinInterval   = 250 * time.Millisecond
)

// Sentinel errors. Results wrap these so callers can branch with errors.Is
// instead of parsing message text.
var (
	// ErrUnsupported is returned by the platform primitives when the current
	// OS has no implementation (the caller then degrades to "allow").
	ErrUnsupported = errors.New("sysload: load sampling unsupported on this platform")
	// ErrCPUAboveCeiling reports that CPU usage exceeded the guard ceiling.
	ErrCPUAboveCeiling = errors.New("sysload: cpu above ceiling")
	// ErrMemAboveCeiling reports that memory usage exceeded the guard ceiling.
	ErrMemAboveCeiling = errors.New("sysload: memory above ceiling")
)

// Config holds the guard ceilings. A zero (or negative) value selects the
// matching default; there is deliberately no way to disable a ceiling.
type Config struct {
	// MaxCPUPercent is the highest acceptable system-wide CPU utilisation as
	// a percentage of all cores combined (0-100).
	MaxCPUPercent float64 `json:"maxCpuPercent"`
	// MaxMemPercent is the highest acceptable physical memory utilisation
	// (0-100).
	MaxMemPercent float64 `json:"maxMemPercent"`
	// MinInterval is the minimum wall-clock spacing between two CPU samples;
	// a shorter gap makes the delta too noisy to trust. The sampler waits for
	// the remainder of the interval instead of returning a bogus value.
	MinInterval time.Duration `json:"minInterval"`
}

// Normalize returns a copy of c with every unset field replaced by its
// default, so the rest of the package never has to special-case zero values.
func (c Config) Normalize() Config {
	if c.MaxCPUPercent <= 0 {
		c.MaxCPUPercent = DefaultMaxCPUPercent
	}
	if c.MaxCPUPercent > 100 {
		c.MaxCPUPercent = 100
	}
	if c.MaxMemPercent <= 0 {
		c.MaxMemPercent = DefaultMaxMemPercent
	}
	if c.MaxMemPercent > 100 {
		c.MaxMemPercent = 100
	}
	if c.MinInterval <= 0 {
		c.MinInterval = DefaultMinInterval
	}
	return c
}

// Sample is one instantaneous reading of host load. Percentages are 0-100;
// the byte counters are only populated when the platform can report them.
type Sample struct {
	CPUPct        float64   `json:"cpuPct"`
	MemPct        float64   `json:"memPct"`
	MemUsedBytes  uint64    `json:"memUsedBytes,omitempty"`
	MemTotalBytes uint64    `json:"memTotalBytes,omitempty"`
	At            time.Time `json:"at"`
}

// String renders a sample in the compact form used by log lines.
func (s Sample) String() string {
	return fmt.Sprintf("cpu=%.1f%% mem=%.1f%%", s.CPUPct, s.MemPct)
}

// LoadError describes a refused guard check. It wraps ErrCPUAboveCeiling
// and/or ErrMemAboveCeiling so errors.Is keeps working, and carries the
// offending sample for logging.
type LoadError struct {
	Sample   Sample
	CPULimit float64
	MemLimit float64
	reasons  []error
}

// Error implements error.
func (e *LoadError) Error() string {
	switch len(e.reasons) {
	case 0:
		return "sysload: load above ceiling"
	case 1:
		return fmt.Sprintf("sysload: %v (cpu %.1f%%/%.0f%%, mem %.1f%%/%.0f%%)",
			e.reasons[0], e.Sample.CPUPct, e.CPULimit, e.Sample.MemPct, e.MemLimit)
	default:
		return fmt.Sprintf("sysload: %v and %v (cpu %.1f%%/%.0f%%, mem %.1f%%/%.0f%%)",
			e.reasons[0], e.reasons[1], e.Sample.CPUPct, e.CPULimit, e.Sample.MemPct, e.MemLimit)
	}
}

// Unwrap exposes the wrapped sentinels to errors.Is / errors.As.
func (e *LoadError) Unwrap() []error { return e.reasons }

// CheckCeilings compares sample against cfg and returns nil when the machine
// has headroom, or a *LoadError when it does not. It is pure: no sampling, no
// sleeping, no clock reads.
func CheckCeilings(sample Sample, cfg Config) error {
	cfg = cfg.Normalize()
	var reasons []error
	if sample.CPUPct > cfg.MaxCPUPercent {
		reasons = append(reasons, ErrCPUAboveCeiling)
	}
	if sample.MemPct > cfg.MaxMemPercent {
		reasons = append(reasons, ErrMemAboveCeiling)
	}
	if len(reasons) == 0 {
		return nil
	}
	return &LoadError{
		Sample:   sample,
		CPULimit: cfg.MaxCPUPercent,
		MemLimit: cfg.MaxMemPercent,
		reasons:  reasons,
	}
}

// Sampler produces host load samples. *SystemSampler is the production
// implementation; tests substitute their own.
type Sampler interface {
	Sample() (Sample, error)
}

// SystemSampler samples the real host. It keeps the previous CPU counter
// reading so it can report a delta instead of the useless since-boot average.
type SystemSampler struct {
	mu       sync.Mutex
	cfg      Config
	lastCPU  cpuTimes
	lastRead time.Time
	started  bool
	now      func() time.Time
	sleep    func(time.Duration)
}

// NewSampler returns a Sampler backed by the operating system.
func NewSampler() *SystemSampler { return NewSamplerWithConfig(Config{}) }

// NewSamplerWithConfig returns a system sampler honouring cfg.MinInterval.
func NewSamplerWithConfig(cfg Config) *SystemSampler {
	return &SystemSampler{
		cfg:   cfg.Normalize(),
		now:   time.Now,
		sleep: time.Sleep,
	}
}

// Sample reads CPU and memory counters. The first call establishes a CPU
// baseline and waits MinInterval before measuring, so even the very first
// reading is a real delta rather than 0%.
func (s *SystemSampler) Sample() (Sample, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	interval := s.cfg.MinInterval
	cpu, cpuErr := readCPUTimes()
	if cpuErr == nil {
		if !s.started {
			s.lastCPU = cpu
			s.lastRead = s.now()
			s.started = true
			s.sleep(interval)
			cpu, cpuErr = readCPUTimes()
		} else if elapsed := s.now().Sub(s.lastRead); elapsed < interval {
			s.sleep(interval - elapsed)
			cpu, cpuErr = readCPUTimes()
		}
	}

	sample := Sample{At: s.now()}
	if cpuErr == nil {
		sample.CPUPct = cpuPercent(s.lastCPU, cpu)
		s.lastCPU = cpu
		s.lastRead = sample.At
	}

	mem, memErr := readMem()
	if memErr == nil {
		sample.MemPct = mem.percent()
		sample.MemUsedBytes = mem.used()
		sample.MemTotalBytes = mem.Total
	}

	switch {
	case cpuErr != nil && memErr != nil:
		return sample, errors.Join(cpuErr, memErr)
	case cpuErr != nil:
		return sample, cpuErr
	case memErr != nil:
		return sample, memErr
	}
	return sample, nil
}

// Guard wraps a Sampler with a Config and answers "may I start new work?".
// A sampling failure is reported alongside the sample and does NOT by itself
// block work: an unreadable counter must not stall the swarm.
type Guard struct {
	cfg     Config
	sampler Sampler
}

// NewGuard builds a Guard around sampler, falling back to the system sampler
// when sampler is nil.
func NewGuard(sampler Sampler, cfg Config) *Guard {
	if sampler == nil {
		sampler = NewSamplerWithConfig(cfg)
	}
	return &Guard{cfg: cfg.Normalize(), sampler: sampler}
}

// Config returns the normalized guard configuration.
func (g *Guard) Config() Config { return g.cfg }

// Allow samples the host and reports whether new work may start. It returns
// the sample it judged so callers can log it, plus the sample error when the
// platform could not provide a reading.
func (g *Guard) Allow() (bool, Sample, error) {
	sample, sampleErr := g.sampler.Sample()
	if sampleErr != nil {
		return true, sample, sampleErr
	}
	if err := CheckCeilings(sample, g.cfg); err != nil {
		return false, sample, err
	}
	return true, sample, nil
}

// cpuTimes is a platform-neutral snapshot of cumulative CPU counters, in
// platform units (Windows: 100ns ticks, Linux: USER_HZ jiffies).
type cpuTimes struct {
	Idle  uint64
	Total uint64
}

// valid reports whether the counters advanced, i.e. whether a delta can be
// computed without dividing by zero.
func (c cpuTimes) valid() bool { return c.Total > 0 && c.Total >= c.Idle }

// cpuPercent derives utilisation from two cumulative snapshots. It never
// panics and clamps to [0,100]; a counter reset (Total going backwards) yields
// 0 rather than a nonsensical value.
func cpuPercent(prev, cur cpuTimes) float64 {
	if !prev.valid() || !cur.valid() {
		return 0
	}
	if cur.Total <= prev.Total {
		return 0
	}
	totalDelta := cur.Total - prev.Total
	idleDelta := uint64(0)
	if cur.Idle > prev.Idle {
		idleDelta = cur.Idle - prev.Idle
	}
	if idleDelta > totalDelta {
		idleDelta = totalDelta
	}
	busy := totalDelta - idleDelta
	pct := float64(busy) / float64(totalDelta) * 100
	return clampPct(pct)
}

// clampPct keeps a percentage inside [0,100]; NaN maps to 0.
func clampPct(v float64) float64 {
	if v != v {
		return 0
	}
	if v < 0 {
		return 0
	}
	if v > 100 {
		return 100
	}
	return v
}

// memInfo is a platform-neutral snapshot of physical memory in bytes.
type memInfo struct {
	Total     uint64
	Available uint64
}

// used returns the number of bytes in use, guarding against a bogus
// Available value larger than Total.
func (m memInfo) used() uint64 {
	if m.Available >= m.Total {
		return 0
	}
	return m.Total - m.Available
}

// percent returns memory utilisation as 0-100; a zero Total yields 0.
func (m memInfo) percent() float64 {
	if m.Total == 0 {
		return 0
	}
	return clampPct(float64(m.used()) / float64(m.Total) * 100)
}
