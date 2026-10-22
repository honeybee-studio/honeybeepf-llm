use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::Command,
};

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

/// Resolve `path` under `root` as a process chrooted there would, symlinks included.
/// Absolute symlinks and `..` would otherwise escape `/proc/<pid>/root` to the agent's own root.
pub fn resolve_in_root(root: &Path, path: &Path) -> Option<PathBuf> {
    const MAX_SYMLINKS: usize = 40;

    let mut rest: VecDeque<OsString> = path.components().filter_map(component_name).collect();
    let mut resolved = PathBuf::new();
    let mut symlinks = 0;
    while let Some(name) = rest.pop_front() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        let meta = std::fs::symlink_metadata(root.join(&next)).ok()?;
        if !meta.file_type().is_symlink() {
            resolved = next;
            continue;
        }
        symlinks += 1;
        if symlinks > MAX_SYMLINKS {
            return None;
        }
        let target = std::fs::read_link(root.join(&next)).ok()?;
        if target.is_absolute() {
            resolved.clear();
        }
        for part in target.components().rev().filter_map(component_name) {
            rest.push_front(part);
        }
    }
    Some(root.join(resolved))
}

fn component_name(c: Component<'_>) -> Option<OsString> {
    match c {
        Component::Normal(s) => Some(s.to_os_string()),
        Component::ParentDir => Some(OsString::from("..")),
        _ => None,
    }
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

    /// Absolute symlinks and `..` stay inside the root, like the container's loader saw them.
    #[test]
    fn test_resolve_in_root_keeps_symlinks_inside_root() -> std::io::Result<()> {
        let root = std::env::temp_dir().join(format!("hb-resolve-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("opt/ssl/lib"))?;
        std::fs::create_dir_all(root.join("usr/lib"))?;
        std::fs::write(root.join("opt/ssl/lib/libssl.so.3"), b"lib")?;
        std::os::unix::fs::symlink("/opt/ssl/lib/libssl.so.3", root.join("usr/lib/libssl.so.3"))?;
        std::os::unix::fs::symlink("usr/lib", root.join("lib"))?;

        let want = root.join("opt/ssl/lib/libssl.so.3");
        let cases = [
            "/usr/lib/libssl.so.3",
            "/lib/libssl.so.3",
            "/usr/local/../lib/../lib/libssl.so.3",
            "/../../../usr/lib/libssl.so.3",
        ];
        for case in cases {
            let got = resolve_in_root(&root, Path::new(case));
            if case.contains("local") {
                assert_eq!(got, None, "{case}: missing directory must not resolve");
            } else {
                assert_eq!(got.as_deref(), Some(want.as_path()), "{case}");
            }
        }

        std::fs::remove_dir_all(&root)
    }
}
