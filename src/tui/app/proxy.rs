use std::path::PathBuf;

use crate::error::Result;

use super::App;

impl App {
    /// Spawn a detached background `ccs serve` process, writing its PID to ~/.ccs/proxy.pid.
    pub fn spawn_bg_proxy(&mut self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let child = std::process::Command::new(&exe)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let pid = child.id();
        drop(child);
        if let Some(path) = pid_file_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, pid.to_string());
        }
        self.bg_proxy_pid = Some(pid);
        Ok(())
    }

    /// Kill the background proxy process and remove the PID file.
    pub fn stop_bg_proxy(&mut self) {
        if let Some(pid) = self.bg_proxy_pid.take() {
            kill_process(pid);
        }
        self.remove_pid_file();
    }

    /// Called when the background proxy is found to have exited on its own.
    pub fn on_bg_proxy_died(&mut self) {
        self.bg_proxy_pid = None;
        self.remove_pid_file();
    }

    pub(super) fn remove_pid_file(&self) {
        if let Some(path) = pid_file_path() {
            remove_pid_file_at(&path);
        }
    }
}

// ── Background proxy helpers ──────────────────────────────────────────────────

pub fn pid_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ccs").join("proxy.pid"))
}

pub fn load_bg_proxy_pid() -> Option<u32> {
    let path = pid_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if is_process_alive(pid) {
        Some(pid)
    } else {
        remove_pid_file_at(&path);
        None
    }
}

fn remove_pid_file_at(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Verify comm name to guard against PID reuse (comm truncated to 15 chars).
        if std::fs::metadata(format!("/proc/{pid}")).is_err() {
            return false;
        }
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|comm| comm.trim().starts_with("ccs"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms use `kill -0` (no-op signal, just checks existence).
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn send_signal(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status();
}

pub fn kill_process(pid: u32) {
    send_signal(pid, "-TERM");
}

pub fn send_sighup(pid: u32) {
    send_signal(pid, "-HUP");
}
