use std::{collections::HashSet, path::PathBuf, process::Command};

use anyhow::Result;
use log::debug;
use once_cell::sync::Lazy;

static SSL_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"libssl\.so\..*|libcrypto\.so\..*").unwrap());

/// Scans running processes to find unique paths to libssl and libcrypto libraries.
/// Also includes system default SSL libraries from ldconfig to pre-attach probes.
pub fn find_ssl_libraries() -> Result<Vec<String>> {
    let mut ssl_paths = HashSet::new();

    // Always include system default SSL libraries first (for pre-attachment)
    debug!("Finding system SSL libraries via ldconfig...");
    if let Ok(system_libs) = find_system_default_ssl() {
        for path in system_libs {
            ssl_paths.insert(path);
        }
    }

    // Also scan running processes for additional libraries (e.g., container-specific)
    debug!("Scanning processes for SSL libraries...");
    if let Ok(procs) = procfs::process::all_processes() {
        for p in procs {
            let process = match p {
                Ok(proc) => proc,
                Err(_) => continue,
            };

            let maps = match process.maps() {
                Ok(m) => m,
                Err(_) => continue,
            };

            for map in maps {
                if let procfs::process::MMapPath::Path(path_buf) = map.pathname
                    && let Some(file_name) = path_buf.file_name().and_then(|n| n.to_str())
                    && SSL_RE.is_match(file_name)
                {
                    // Resolve container paths to host paths
                    let host_path = resolve_host_path(process.pid, &path_buf);

                    // Check existence on host
                    if host_path.exists() {
                        let path_str = host_path.to_string_lossy().to_string();
                        if !ssl_paths.contains(&path_str) {
                            debug!("Found SSL Lib: {} (from PID: {})", path_str, process.pid);
                            ssl_paths.insert(path_str);
                        }
                    }
                }
            }
        }
    }

    Ok(ssl_paths.into_iter().collect())
}

/// Resolves a path from a process's namespace to the host filesystem.
fn resolve_host_path(pid: i32, container_path: &std::path::Path) -> PathBuf {
    if container_path.starts_with("/proc") {
        return container_path.to_path_buf();
    }

    // /proc/<PID>/root/<CONTAINER_PATH>
    // Note: This logic assumes Linux-style procfs layout.
    let mut root_path = PathBuf::from(format!("/proc/{}/root", pid));
    let relative_path = container_path.strip_prefix("/").unwrap_or(container_path);
    root_path.push(relative_path);

    root_path
}

/// Scans only the given PIDs for SSL libraries. Much cheaper than scanning all processes.
pub fn find_ssl_for_pids(pids: &[u32]) -> Result<Vec<String>> {
    let mut ssl_paths = HashSet::new();

    for &pid in pids {
        let process = match procfs::process::Process::new(pid as i32) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let maps = match process.maps() {
            Ok(m) => m,
            Err(_) => continue,
        };

        for map in maps {
            if let procfs::process::MMapPath::Path(path_buf) = map.pathname
                && let Some(file_name) = path_buf.file_name().and_then(|n| n.to_str())
                && SSL_RE.is_match(file_name)
            {
                let host_path = resolve_host_path(process.pid, &path_buf);
                if host_path.exists() {
                    let path_str = host_path.to_string_lossy().to_string();
                    if !ssl_paths.contains(&path_str) {
                        debug!("Found SSL Lib: {} (from PID: {})", path_str, process.pid);
                        ssl_paths.insert(path_str);
                    }
                }
            }
        }
    }

    Ok(ssl_paths.into_iter().collect())
}

/// Find system SSL libraries using ldconfig.
pub fn find_system_default_ssl() -> Result<Vec<String>> {
    let mut paths = Vec::new();

    if let Ok(output) = Command::new("ldconfig").arg("-p").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("libssl.so") {
                let parts: Vec<&str> = line.split("=>").collect();
                if parts.len() > 1 {
                    let path = parts[1].trim().to_string();
                    if !paths.contains(&path) {
                        debug!("Found system SSL via ldconfig: {}", path);
                        paths.push(path);
                    }
                }
            }
        }
    }

    Ok(paths)
}
