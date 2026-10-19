package sysload

import (
	"errors"
	"strings"
	"testing"
	"time"
)

func TestConfigNormalize(t *testing.T) {
	got := Config{}.Normalize()
	if got.MaxCPUPercent != DefaultMaxCPUPercent {
		t.Fatalf("MaxCPUPercent = %v, want %v", got.MaxCPUPercent, DefaultMaxCPUPercent)
	}
	if got.MaxMemPercent != DefaultMaxMemPercent {
		t.Fatalf("MaxMemPercent = %v, want %v", got.MaxMemPercent, DefaultMaxMemPercent)
	}
	if got.MinInterval != DefaultMinInterval {
		t.Fatalf("MinInterval = %v, want %v", got.MinInterval, DefaultMinInterval)
	}

	got = Config{MaxCPUPercent: 40, MaxMemPercent: 50, MinInterval: time.Second}.Normalize()
	if got.MaxCPUPercent != 40 || got.MaxMemPercent != 50 || got.MinInterval != time.Second {
		t.Fatalf("explicit values were not preserved: %+v", got)
	}

	got = Config{MaxCPUPercent: 250, MaxMemPercent: 999}.Normalize()
	if got.MaxCPUPercent != 100 || got.MaxMemPercent != 100 {
		t.Fatalf("over-100 ceilings must clamp to 100, got %+v", got)
	}

	got = Config{MaxCPUPercent: -5, MaxMemPercent: -1}.Normalize()
	if got.MaxCPUPercent != DefaultMaxCPUPercent || got.MaxMemPercent != DefaultMaxMemPercent {
		t.Fatalf("negative ceilings must fall back to defaults, got %+v", got)
	}
}

func TestCheckCeilings(t *testing.T) {
	cfg := Config{MaxCPUPercent: 80, MaxMemPercent: 70}

	tests := []struct {
		name    string
		sample  Sample
		wantCPU bool
		wantMem bool
	}{
		{name: "headroom", sample: Sample{CPUPct: 10, MemPct: 20}},
		{name: "cpu above", sample: Sample{CPUPct: 80.5, MemPct: 20}, wantCPU: true},
		{name: "mem above", sample: Sample{CPUPct: 10, MemPct: 70.5}, wantMem: true},
		{name: "both above", sample: Sample{CPUPct: 99, MemPct: 99}, wantCPU: true, wantMem: true},
		{name: "exactly at ceiling is allowed", sample: Sample{CPUPct: 80, MemPct: 70}},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := CheckCeilings(tc.sample, cfg)
			if !tc.wantCPU && !tc.wantMem {
				if err != nil {
					t.Fatalf("expected nil error, got %v", err)
				}
				return
			}
			if err == nil {
				t.Fatal("expected an error, got nil")
			}
			if got := errors.Is(err, ErrCPUAboveCeiling); got != tc.wantCPU {
				t.Errorf("errors.Is(ErrCPUAboveCeiling) = %v, want %v", got, tc.wantCPU)
			}
			if got := errors.Is(err, ErrMemAboveCeiling); got != tc.wantMem {
				t.Errorf("errors.Is(ErrMemAboveCeiling) = %v, want %v", got, tc.wantMem)
			}
			var loadErr *LoadError
			if !errors.As(err, &loadErr) {
				t.Fatalf("error is not *LoadError: %T", err)
			}
			if loadErr.CPULimit != 80 || loadErr.MemLimit != 70 {
				t.Errorf("limits = %v/%v, want 80/70", loadErr.CPULimit, loadErr.MemLimit)
			}
			if loadErr.Sample != tc.sample {
				t.Errorf("sample = %+v, want %+v", loadErr.Sample, tc.sample)
			}
		})
	}
}

func TestLoadErrorMessage(t *testing.T) {
	err := CheckCeilings(Sample{CPUPct: 95, MemPct: 96}, Config{})
	if err == nil {
		t.Fatal("expected an error")
	}
	msg := err.Error()
	for _, want := range []string{"cpu", "mem", "95.0%", "96.0%"} {
		if !strings.Contains(msg, want) {
			t.Errorf("message %q missing %q", msg, want)
		}
	}
}

func TestCPUPercent(t *testing.T) {
	tests := []struct {
		name string
		prev cpuTimes
		cur  cpuTimes
		want float64
	}{
		{name: "zero previous", prev: cpuTimes{}, cur: cpuTimes{Idle: 50, Total: 100}, want: 0},
		{name: "no advance", prev: cpuTimes{Idle: 10, Total: 20}, cur: cpuTimes{Idle: 10, Total: 20}, want: 0},
		{name: "counter reset", prev: cpuTimes{Idle: 10, Total: 20}, cur: cpuTimes{Idle: 1, Total: 2}, want: 0},
		{name: "fully busy", prev: cpuTimes{Idle: 10, Total: 20}, cur: cpuTimes{Idle: 10, Total: 120}, want: 100},
		{name: "fully idle", prev: cpuTimes{Idle: 10, Total: 20}, cur: cpuTimes{Idle: 110, Total: 120}, want: 0},
		{name: "half", prev: cpuTimes{Idle: 0, Total: 100}, cur: cpuTimes{Idle: 50, Total: 200}, want: 50},
		{name: "idle delta larger than total delta is clamped to idle", prev: cpuTimes{Idle: 0, Total: 100}, cur: cpuTimes{Idle: 500, Total: 200}, want: 0},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := cpuPercent(tc.prev, tc.cur); got != tc.want {
				t.Fatalf("cpuPercent = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestClampPct(t *testing.T) {
	nan := 0.0
	nan = nan / nan
	tests := map[float64]float64{nan: 0, -1: 0, 0: 0, 42.5: 42.5, 100: 100, 100.1: 100}
	for in, want := range tests {
		if got := clampPct(in); got != want {
			t.Errorf("clampPct(%v) = %v, want %v", in, got, want)
		}
	}
}

func TestMemInfo(t *testing.T) {
	m := memInfo{Total: 1000, Available: 250}
	if got := m.used(); got != 750 {
		t.Fatalf("used = %d, want 750", got)
	}
	if got := m.percent(); got != 75 {
		t.Fatalf("percent = %v, want 75", got)
	}

	zero := memInfo{}
	if zero.used() != 0 || zero.percent() != 0 {
		t.Fatalf("zero memInfo must report 0/0, got %d/%v", zero.used(), zero.percent())
	}

	bogus := memInfo{Total: 100, Available: 500}
	if bogus.used() != 0 || bogus.percent() != 0 {
		t.Fatalf("available > total must clamp to 0, got %d/%v", bogus.used(), bogus.percent())
	}
}

// fakeSampler lets the guard tests run without touching the host.
type fakeSampler struct {
	sample Sample
	err    error
	calls  int
}

func (f *fakeSampler) Sample() (Sample, error) {
	f.calls++
	return f.sample, f.err
}

func TestGuardAllow(t *testing.T) {
	cfg := Config{MaxCPUPercent: 60, MaxMemPercent: 60}

	fs := &fakeSampler{sample: Sample{CPUPct: 5, MemPct: 5}}
	guard := NewGuard(fs, cfg)
	ok, sample, err := guard.Allow()
	if !ok || err != nil {
		t.Fatalf("expected allow, got ok=%v err=%v", ok, err)
	}
	if sample.CPUPct != 5 {
		t.Fatalf("sample not returned to caller: %+v", sample)
	}
	if fs.calls != 1 {
		t.Fatalf("sampler calls = %d, want 1", fs.calls)
	}

	fs = &fakeSampler{sample: Sample{CPUPct: 61, MemPct: 5}}
	ok, _, err = NewGuard(fs, cfg).Allow()
	if ok || !errors.Is(err, ErrCPUAboveCeiling) {
		t.Fatalf("expected cpu refusal, got ok=%v err=%v", ok, err)
	}

	fs = &fakeSampler{sample: Sample{CPUPct: 5, MemPct: 61}}
	ok, _, err = NewGuard(fs, cfg).Allow()
	if ok || !errors.Is(err, ErrMemAboveCeiling) {
		t.Fatalf("expected mem refusal, got ok=%v err=%v", ok, err)
	}
}

func TestGuardAllowDegradesOnSamplingError(t *testing.T) {
	boom := errors.New("boom")
	guard := NewGuard(&fakeSampler{err: boom}, Config{})
	ok, _, err := guard.Allow()
	if !ok {
		t.Fatal("a sampling failure must not block new work")
	}
	if !errors.Is(err, boom) {
		t.Fatalf("sampling error must be surfaced, got %v", err)
	}
}

func TestGuardDefaultsToSystemSampler(t *testing.T) {
	guard := NewGuard(nil, Config{MaxCPUPercent: 99.9, MaxMemPercent: 99.9})
	if guard.Config().MaxCPUPercent != 99.9 {
		t.Fatalf("config not stored: %+v", guard.Config())
	}
	// Allow must never panic, even when the platform cannot sample.
	if _, _, _ = guard.Allow(); true {
	}
}

// TestSystemSamplerRealHost exercises the platform primitives. It only checks
// invariants (no panics, sane ranges), never machine-specific values.
func TestSystemSamplerRealHost(t *testing.T) {
	s := NewSamplerWithConfig(Config{MinInterval: time.Millisecond})
	sample, err := s.Sample()
	if errors.Is(err, ErrUnsupported) {
		t.Skip("load sampling unsupported on this platform")
	}
	if err != nil {
		t.Fatalf("Sample() error: %v", err)
	}
	if sample.CPUPct < 0 || sample.CPUPct > 100 {
		t.Errorf("CPUPct out of range: %v", sample.CPUPct)
	}
	if sample.MemPct <= 0 || sample.MemPct > 100 {
		t.Errorf("MemPct out of range: %v", sample.MemPct)
	}
	if sample.MemTotalBytes == 0 {
		t.Error("MemTotalBytes must be reported on a supported platform")
	}
	if sample.At.IsZero() {
		t.Error("At must be set")
	}
	if sample.String() == "" {
		t.Error("String() must not be empty")
	}

	// A second call must reuse the baseline instead of sleeping a full
	// interval again, and must still produce a sane reading.
	if _, err := s.Sample(); err != nil {
		t.Fatalf("second Sample() error: %v", err)
	}
}
