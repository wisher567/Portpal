use serde::Serialize;
use std::process::Command;
use std::collections::HashSet;
use sysinfo::System;

#[derive(Serialize, Clone)]
pub struct PortInfo {
    pub port: u16,
    pub pid: u32,
    pub process_name: String,
    pub project_path: Option<String>,
    pub project_name: Option<String>,
    pub protocol: String,
    pub start_cmd: Option<String>,
}

// ─── Entry point (platform router) ───────────────────────────────────────────

pub fn scan_ports() -> Vec<PortInfo> {
    let mut sys = System::new();
    sys.refresh_processes();

    #[cfg(target_os = "windows")]
    return scan_windows(&sys);

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return scan_unix(&sys);
}

pub fn kill_pid(pid: u32) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    return kill_windows(pid);

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return kill_unix(pid);
}

// ─── Windows ─────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn scan_windows(sys: &System) -> Vec<PortInfo> {
    let output = Command::new("netstat")
        .args(["-ano"])
        .output()
        .expect("failed to run netstat");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ports: Vec<PortInfo> = Vec::new();
    let mut seen_entries: HashSet<(u16, u32)> = HashSet::new();

    for line in stdout.lines() {
        if !line.contains("LISTENING") { continue; }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 { continue; }

        let port: u16 = match parts[1].rsplit(':').next()
            .and_then(|p| p.parse().ok()) {
            Some(p) => p,
            None => continue,
        };

        let pid: u32 = match parts[4].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if pid == 0 || seen_entries.contains(&(port, pid)) { continue; }
        seen_entries.insert((port, pid));

        let mut process_name = format!("PID {}", pid);
        let mut project_path = None;
        let mut start_cmd = None;

        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            let p_name = process.name().to_string();
            if !p_name.trim().is_empty() {
                process_name = p_name;
            }

            let cmd_arr = process.cmd();
            let cmd_str = cmd_arr.join(" ");

            if !cmd_str.trim().is_empty() {
                start_cmd = Some(cmd_str.trim().to_string());
            }

            if let Some(cwd) = process.cwd() {
                project_path = find_project_root(cwd).map(|p| p.to_string_lossy().to_string());
            }

            if project_path.is_none() {
                if let Some(exe) = process.exe() {
                    if let Some(parent) = exe.parent() {
                        project_path = find_project_root(parent).map(|p| p.to_string_lossy().to_string());
                    }
                }
            }
        }

        let project_name = extract_project_name(&project_path);

        ports.push(PortInfo {
            port, pid, process_name,
            project_path, project_name,
            protocol: "TCP".into(),
            start_cmd,
        });
    }

    ports.sort_by_key(|p| p.port);
    ports
}

#[cfg(target_os = "windows")]
fn kill_windows(pid: u32) -> Result<(), String> {
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() { Ok(()) }
    else { Err(String::from_utf8_lossy(&output.stderr).to_string()) }
}

// ─── macOS + Linux (shared lsof path) ────────────────────────────────────────

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn scan_unix(sys: &System) -> Vec<PortInfo> {
    let output = Command::new("lsof")
        .args(["-iTCP", "-sTCP:LISTEN", "-n", "-P"])
        .output()
        .expect("failed to run lsof — is it installed?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ports: Vec<PortInfo> = Vec::new();
    let mut seen_entries: HashSet<(u16, u32)> = HashSet::new();

    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 9 { continue; }

        let mut process_name = parts[0].to_string();
        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let mut idx = parts.len() - 1;
        if parts[idx].starts_with('(') {
            idx -= 1;
        }
        let name = parts[idx];
        let port: u16 = match name.rsplit(':').next()
            .and_then(|p| p.parse().ok()) {
            Some(p) => p,
            None => continue,
        };

        if seen_entries.contains(&(port, pid)) { continue; }
        seen_entries.insert((port, pid));

        let mut project_path = None;
        let mut start_cmd = None;

        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            let sys_name = process.name().to_string();
            if !sys_name.is_empty() { process_name = sys_name; }

            if let Some(cwd) = process.cwd() {
                project_path = find_project_root(cwd).map(|p| p.to_string_lossy().to_string());
            }

            let cmd = process.cmd().join(" ");
            if !cmd.trim().is_empty() {
                start_cmd = Some(cmd);
            }
        }

        if project_path.is_none() {
            project_path = get_project_path_unix_fallback(pid);
        }

        let project_name = extract_project_name(&project_path);

        ports.push(PortInfo {
            port, pid, process_name,
            project_path, project_name,
            protocol: "TCP".into(),
            start_cmd,
        });
    }

    ports.sort_by_key(|p| p.port);
    ports
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn get_project_path_unix_fallback(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let cwd = std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()?;
        find_project_root(std::path::Path::new(&cwd)).map(|p| p.to_string_lossy().to_string())
    }

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("lsof")
            .args(["-p", &pid.to_string(), "-a", "-d", "cwd", "-Fn"])
            .output()
            .ok()?;

        let s = String::from_utf8_lossy(&output.stdout);
        let cwd = s.lines()
            .find(|l| l.starts_with('n') && l.len() > 1)
            .map(|l| l[1..].to_string())?;

        find_project_root(std::path::Path::new(&cwd))
            .map(|p| p.to_string_lossy().to_string())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn kill_unix(pid: u32) -> Result<(), String> {
    unsafe { libc::kill(pid as i32, libc::SIGTERM); }
    std::thread::sleep(std::time::Duration::from_secs(2));

    if process_exists_unix(pid) {
        unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_exists_unix(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

fn find_project_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let markers = [
        "package.json", "Cargo.toml", "go.mod",
        "pyproject.toml", "requirements.txt",
        "pom.xml", "build.gradle", ".git",
    ];
    let mut dir = start.to_path_buf();
    for _ in 0..6 {
        for marker in &markers {
            if dir.join(marker).exists() {
                return Some(dir);
            }
        }
        if !dir.pop() { break; }
    }
    None
}

fn extract_project_name(path: &Option<String>) -> Option<String> {
    path.as_ref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}
