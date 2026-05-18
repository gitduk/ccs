use std::time::Instant;

/// Snapshot of process resource usage for display in the title bar.
#[derive(Clone, Default)]
pub(crate) struct SysInfo {
    pub cpu_pct: f32,
    pub mem_mb: u32,
}

/// Samples CPU and memory periodically and caches the latest snapshot.
pub(crate) struct SysInfoSampler {
    last_cpu_ticks: u64,
    last_sample: Instant,
    current: SysInfo,
}

// Linux kernel clock frequency; nearly universal on modern kernels.
const HZ: f64 = 100.0;

impl SysInfoSampler {
    pub(crate) fn new() -> Self {
        Self {
            last_cpu_ticks: read_cpu_ticks().unwrap_or(0),
            last_sample: Instant::now(),
            current: SysInfo::default(),
        }
    }

    /// Recompute the snapshot from current `/proc` data.
    pub(crate) fn sample(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();

        let cpu_ticks = read_cpu_ticks().unwrap_or(self.last_cpu_ticks);
        let delta_ticks = cpu_ticks.saturating_sub(self.last_cpu_ticks) as f64;
        let cpu_pct = if elapsed > 0.0 {
            ((delta_ticks / HZ) / elapsed * 100.0) as f32
        } else {
            0.0
        };
        let next = SysInfo {
            cpu_pct,
            mem_mb: read_vmrss_kb().unwrap_or(0) / 1024,
        };
        let changed = display_key(&self.current) != display_key(&next);

        self.last_cpu_ticks = cpu_ticks;
        self.last_sample = now;
        if changed {
            self.current = next;
        }
        changed
    }

    pub(crate) fn current(&self) -> &SysInfo {
        &self.current
    }
}

fn display_key(info: &SysInfo) -> (u32, u32) {
    ((info.cpu_pct * 10.0).round() as u32, info.mem_mb)
}

fn read_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Format: pid (comm) state ppid ... utime(14) stime(15) ...
    // comm may contain spaces, so skip past the closing ')'.
    let after_comm = stat.rfind(')')?.saturating_add(2);
    let mut fields = stat[after_comm..].split_whitespace();
    // Fields after comm (0-indexed): state(0) ppid(1) ... utime(11) stime(12)
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

fn read_vmrss_kb() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}
