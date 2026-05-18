use std::time::Instant;

/// Snapshot of process resource usage for display in the title bar.
#[derive(Clone, Default)]
pub(crate) struct SysInfo {
    pub cpu_pct: f32,
    pub mem_mb: u32,
    pub net_in_kbps: f32,
    pub net_out_kbps: f32,
}

/// Samples CPU, memory, and network counters periodically and caches the
/// latest snapshot. App holds one `SysInfoSampler`; chrome reads `current()`.
pub(crate) struct SysInfoSampler {
    last_cpu_ticks: u64,
    last_sample: Instant,
    last_net_in: u64,
    last_net_out: u64,
    current: SysInfo,
}

// Linux kernel clock frequency; nearly universal on modern kernels.
const HZ: f64 = 100.0;

impl SysInfoSampler {
    pub(crate) fn new() -> Self {
        let (net_in, net_out) = crate::metrics::net_bytes_snapshot();
        Self {
            last_cpu_ticks: read_cpu_ticks().unwrap_or(0),
            last_sample: Instant::now(),
            last_net_in: net_in,
            last_net_out: net_out,
            current: SysInfo::default(),
        }
    }

    /// Recompute the snapshot from current `/proc` data and network counters.
    pub(crate) fn sample(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();

        let cpu_ticks = read_cpu_ticks().unwrap_or(self.last_cpu_ticks);
        let delta_ticks = cpu_ticks.saturating_sub(self.last_cpu_ticks) as f64;
        let cpu_pct = if elapsed > 0.0 {
            ((delta_ticks / HZ) / elapsed * 100.0) as f32
        } else {
            0.0
        };

        let mem_mb = read_vmrss_kb().unwrap_or(0) / 1024;

        let (net_in, net_out) = crate::metrics::net_bytes_snapshot();
        let net_in_kbps = delta_kbps(net_in, self.last_net_in, elapsed);
        let net_out_kbps = delta_kbps(net_out, self.last_net_out, elapsed);

        self.last_cpu_ticks = cpu_ticks;
        self.last_sample = now;
        self.last_net_in = net_in;
        self.last_net_out = net_out;
        self.current = SysInfo {
            cpu_pct,
            mem_mb,
            net_in_kbps,
            net_out_kbps,
        };
    }

    pub(crate) fn current(&self) -> &SysInfo {
        &self.current
    }
}

fn delta_kbps(new: u64, old: u64, elapsed: f64) -> f32 {
    if elapsed > 0.0 {
        (new.saturating_sub(old) as f64 / elapsed / 1024.0) as f32
    } else {
        0.0
    }
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
