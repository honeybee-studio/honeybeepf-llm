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

/// Identity of a library file; per-process and per-container paths to one library share it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileId {
    Device {
        dev: u64,
        ino: u64,
    },
    /// overlayfs gives each container mount its own st_dev but reports the lower file's inode
    /// and times, and uprobes attach to that lower inode; size and times stand in for dev.
    Overlay {
        ino: u64,
        size: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    },
}

impl FileId {
    pub fn ino(&self) -> u64 {
        match self {
            FileId::Device { ino, .. } | FileId::Overlay { ino, .. } => *ino,
        }
    }
}

/// Identity of the file behind `path`, keyed without st_dev only on overlayfs.
pub fn file_identity(path: &str) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(path).ok()?;
    if fs_type(path).as_deref() == Some("overlay") {
        return Some(FileId::Overlay {
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        });
    }
    Some(FileId::Device {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Filesystem type of the innermost mount holding `path`, from the owning process's mountinfo.
fn fs_type(path: &str) -> Option<String> {
    let (process, inner) = match path
        .strip_prefix("/proc/")
        .and_then(|rest| rest.split_once("/root/"))
    {
        Some((pid, rel)) => (
            procfs::process::Process::new(pid.parse().ok()?).ok()?,
            PathBuf::from("/").join(rel),
        ),
        None => (
            procfs::process::Process::myself().ok()?,
            PathBuf::from(path),
        ),
    };
    // Mount points are relative to that process's root; on ties the later (top) mount wins.
    process
        .mountinfo()
        .ok()?
        .into_iter()
        .filter(|m| inner.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.components().count())
        .map(|m| m.fs_type)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/<pid>/root` path resolves against that process's mounts, innermost first.
    #[test]
    fn test_fs_type_uses_innermost_mount() {
        let via_root = format!("/proc/{}/root/proc/cpuinfo", std::process::id());
        assert_eq!(fs_type(&via_root).as_deref(), Some("proc"));
        assert_eq!(fs_type("/proc/cpuinfo").as_deref(), Some("proc"));
    }
}
