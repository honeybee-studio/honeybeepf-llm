use std::{collections::HashSet, path::PathBuf, process::Command};

use anyhow::Result;
use log::{debug, warn};
use regex::Regex;

/// Resolves a path from a process's namespace to the host filesystem.
/// Handles containerized processes by looking through /proc/<PID>/root.
pub fn resolve_host_path(pid: i32, container_path: &std::path::Path) -> PathBuf {
    if container_path.starts_with("/proc") {
        return container_path.to_path_buf();
    }

    // /proc/<PID>/root/<CONTAINER_PATH>
    let mut root_path = PathBuf::from(format!("/proc/{}/root", pid));
    let relative_path = container_path.strip_prefix("/").unwrap_or(container_path);
    root_path.push(relative_path);

    root_path
}

/// Find libraries matching a regex pattern across all running processes.
/// Also includes system libraries found via ldconfig.
pub fn find_libraries_all(
    pattern: &Regex,
    ldconfig_substring: Option<&str>,
) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();

    // Include system libraries via ldconfig if a substring is provided
    if let Some(substring) = ldconfig_substring {
        debug!(
            "Finding system libraries via ldconfig for '{}'...",
            substring
        );
        if let Ok(system_libs) = find_system_libraries(substring) {
            for path in system_libs {
                paths.insert(path);
            }
        }
    }

    // Scan running processes
    debug!("Scanning processes for libraries matching pattern...");
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
                    && pattern.is_match(file_name)
                {
                    let host_path = resolve_host_path(process.pid, &path_buf);

                    if host_path.exists() {
                        let path_str = host_path.to_string_lossy().to_string();
                        if !paths.contains(&path_str) {
                            debug!("Found library: {} (from PID: {})", path_str, process.pid);
                            paths.insert(path_str);
                        }
                    }
                }
            }
        }
    }

    Ok(paths)
}

/// Find system libraries via ldconfig matching a substring.
pub fn find_system_libraries(substring: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();

    match Command::new("ldconfig").arg("-p").output() {
        Ok(output) => {
            if !output.status.success() {
                warn!("ldconfig exited with status: {}", output.status);
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains(substring) {
                    let parts: Vec<&str> = line.split("=>").collect();
                    if parts.len() > 1 {
                        let path = parts[1].trim().to_string();
                        if !paths.contains(&path) {
                            debug!("Found system library via ldconfig: {}", path);
                            paths.push(path);
                        }
                    }
                }
            }
        }
        Err(e) => {
            warn!("Failed to execute ldconfig: {}", e);
        }
    }

    Ok(paths)
}

/// Get all shared libraries loaded by a specific process.
/// This avoids rescanning /proc/PID/maps multiple times for different probes.
pub fn get_process_libraries(pid: u32) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();

    let process = match procfs::process::Process::new(pid as i32) {
        Ok(p) => p,
        Err(e) => {
            debug!("Failed to open process {}: {}", pid, e);
            return Ok(paths);
        }
    };

    let maps = match process.maps() {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to read maps for process {}: {}", pid, e);
            return Ok(paths);
        }
    };

    for map in maps {
        if let procfs::process::MMapPath::Path(path_buf) = map.pathname {
            let host_path = resolve_host_path(process.pid, &path_buf);
            if host_path.exists() {
                paths.insert(host_path.to_string_lossy().to_string());
            }
        }
    }

    Ok(paths)
}
