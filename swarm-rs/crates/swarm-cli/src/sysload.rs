//! Host load sampling.
//!
//! Mirrors Go `internal/sysload`: sample CPU and memory, and answer "does the
//! host still have headroom to spawn more work?". Two thresholds matter, and
//! they are deliberately different:
//!
//! * **85% CPU / 90% memory** — refuse to *grow* the pool.
//! * **92% either** — *shrink* the pool by one.
//!
//! The package is dependency-free by design: platform primitives live behind
//! `#[cfg]`, and everything else is portable. An unsupported platform returns
//! `None` and the caller degrades to "no pressure", which is what the Go
//! version does too.

use std::time::Duration;

/// CPU percentage above which the pool refuses to grow.
pub const MAX_CPU_PERCENT: f64 = 85.0;
/// Memory percentage above which the pool refuses to grow.
pub const MAX_MEM_PERCENT: f64 = 90.0;
/// CPU percentage above which the pool must shrink.
pub const CRITICAL_CPU_PERCENT: f64 = 92.0;
/// Memory percentage above which the pool must shrink.
pub const CRITICAL_MEM_PERCENT: f64 = 92.0;
/// Minimum spacing between two CPU samples: a shorter gap makes the delta noise.
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// One instantaneous reading of host load.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadSample {
    /// 0-100, system-wide across all cores.
    pub cpu_pct: f64,
    /// 0-100 of physical memory.
    pub mem_pct: f64,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
}

impl LoadSample {
    /// Whether the pool may grow.
    pub fn allows_growth(&self) -> bool {
        self.cpu_pct <= MAX_CPU_PERCENT && self.mem_pct <= MAX_MEM_PERCENT
    }

    /// Whether the pool must shrink.
    pub fn is_critical(&self) -> bool {
        self.cpu_pct > CRITICAL_CPU_PERCENT || self.mem_pct > CRITICAL_MEM_PERCENT
    }
}

/// Sample host load, or `None` on an unsupported platform.
///
/// The CPU figure needs two reads spaced by [`MIN_SAMPLE_INTERVAL`] to be
/// meaningful, so this call blocks for that long. Callers on a 1s autoscaler
/// tick are unaffected; callers that need a non-blocking read should cache the
/// result and refresh it from a background task.
pub fn sample() -> Option<LoadSample> {
    let mem = read_mem()?;
    let cpu = read_cpu_pct()?;
    Some(LoadSample {
        cpu_pct: cpu,
        mem_pct: mem.0,
        mem_total_bytes: mem.1,
        mem_used_bytes: mem.2,
    })
}

/// A non-blocking sample: memory only, CPU reported as 0.
///
/// Used where blocking for 250ms is unacceptable (a status line, a test).
pub fn sample_memory_only() -> Option<LoadSample> {
    let (pct, total, used) = read_mem()?;
    Some(LoadSample {
        cpu_pct: 0.0,
        mem_pct: pct,
        mem_total_bytes: total,
        mem_used_bytes: used,
    })
}

// ------------------------------------------------------------------ platform

/// Returns `(percent_used, total_bytes, used_bytes)`.
#[cfg(unix)]
fn read_mem() -> Option<(f64, u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut available = 0u64;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value: u64 = parts.next()?.parse().ok()?;
        match key {
            "MemTotal:" => total = value * 1024,
            "MemAvailable:" => available = value * 1024,
            _ => {}
        }
        if total > 0 && available > 0 {
            break;
        }
    }
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some((used as f64 / total as f64 * 100.0, total, used))
}

#[cfg(windows)]
fn read_mem() -> Option<(f64, u64, u64)> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: MEMORYSTATUSEX is a plain-old-data struct; zeroing it and setting
    // dwLength is exactly what the API requires before the call.
    let mut st: MEMORYSTATUSEX = unsafe { MaybeUninit::zeroed().assume_init() };
    st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut st) };
    if ok == 0 {
        return None;
    }
    let total = st.ullTotalPhys;
    let avail = st.ullAvailPhys;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(avail);
    Some((used as f64 / total as f64 * 100.0, total, used))
}

#[cfg(not(any(unix, windows)))]
fn read_mem() -> Option<(f64, u64, u64)> {
    None
}

/// CPU utilisation over the last [`MIN_SAMPLE_INTERVAL`].
#[cfg(unix)]
fn read_cpu_pct() -> Option<f64> {
    let a = read_proc_stat()?;
    std::thread::sleep(MIN_SAMPLE_INTERVAL);
    let b = read_proc_stat()?;

    let total = b.0.saturating_sub(a.0);
    let idle = b.1.saturating_sub(a.1);
    if total == 0 {
        return None;
    }
    Some((total - idle) as f64 / total as f64 * 100.0)
}

/// `(total_jiffies, idle_jiffies)` from `/proc/stat`'s first line.
#[cfg(unix)]
fn read_proc_stat() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let mut total = 0u64;
    let mut idle = 0u64;
    for (i, field) in it.enumerate() {
        let v: u64 = field.parse().ok()?;
        total += v;
        // Fields: user nice system idle iowait irq softirq steal ...
        if i == 3 || i == 4 {
            idle += v;
        }
    }
    Some((total, idle))
}

#[cfg(windows)]
fn read_cpu_pct() -> Option<f64> {
    let (a_idle, a_total) = read_system_times_win()?;
    std::thread::sleep(MIN_SAMPLE_INTERVAL);
    let (b_idle, b_total) = read_system_times_win()?;

    let total = b_total.saturating_sub(a_total);
    let idle = b_idle.saturating_sub(a_idle);
    if total == 0 {
        return None;
    }
    Some((total - idle) as f64 / total as f64 * 100.0)
}

/// `(idle_100ns, total_100ns)` across all CPUs.
#[cfg(windows)]
fn read_system_times_win() -> Option<(u64, u64)> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    // SAFETY: three plain-old-data out-params, zeroed before the call.
    #[allow(clippy::uninit_assume_init)]
    let mut idle: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
    let mut kernel: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
    let mut user: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let to_u64 = |ft: FILETIME| ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    let idle = to_u64(idle);
    // kernel time includes idle, so total = kernel + user.
    let total = to_u64(kernel) + to_u64(user);
    Some((idle, total))
}

#[cfg(not(any(unix, windows)))]
fn read_cpu_pct() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_ordered() {
        assert!(
            MAX_CPU_PERCENT < CRITICAL_CPU_PERCENT,
            "grow-refusal must be below shrink-force"
        );
        assert!(
            MAX_MEM_PERCENT < CRITICAL_MEM_PERCENT,
            "grow-refusal must be below shrink-force"
        );
    }

    #[test]
    fn zero_load_allows_growth() {
        let s = LoadSample::default();
        assert!(s.allows_growth());
        assert!(!s.is_critical());
    }

    #[test]
    fn eighty_eight_cpu_refuses_growth_but_does_not_shrink() {
        let s = LoadSample {
            cpu_pct: 88.0,
            ..Default::default()
        };
        assert!(!s.allows_growth(), "88 > 85");
        assert!(!s.is_critical(), "88 < 92");
    }

    #[test]
    fn ninety_five_memory_forces_shrink() {
        let s = LoadSample {
            mem_pct: 95.0,
            ..Default::default()
        };
        assert!(!s.allows_growth());
        assert!(s.is_critical());
    }

    #[test]
    fn boundary_values_are_inclusive_for_growth() {
        let s = LoadSample {
            cpu_pct: 85.0,
            mem_pct: 90.0,
            ..Default::default()
        };
        assert!(s.allows_growth(), "exactly at the ceiling still allows");
    }

    #[test]
    fn memory_sample_is_available_on_this_platform() {
        // Both Windows and Linux have a memory read; only exotic targets do not.
        if cfg!(any(windows, unix)) {
            let s = sample_memory_only().expect("memory sampling must work here");
            assert!(s.mem_pct >= 0.0 && s.mem_pct <= 100.0, "got {}", s.mem_pct);
            assert!(s.mem_total_bytes > 0);
        }
    }

    #[test]
    fn full_sample_blocks_at_most_the_min_interval() {
        if cfg!(any(windows, unix)) {
            let start = std::time::Instant::now();
            let _ = sample();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "the CPU sample must not block longer than the interval plus slack"
            );
        }
    }
}
