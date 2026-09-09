use std::path::Path;

/// Delete the rename-aside binaries a previous swap could not remove.
///
/// Windows refuses to delete an executable while a process still has it
/// loaded, so the swap below can only try. Nothing retried, and the orphans
/// accumulated indefinitely (9+ GB on a self-dev machine). Sweeping on every
/// swap collects each one as soon as the process holding it exits.
#[cfg(windows)]
fn remove_stale_rename_aside(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".jcode-launcher-old-") || name.contains(".exe.old-") {
            // Still-loaded binaries fail here and are retried on the next swap.
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Set file permissions to owner read/write/execute (0o755).
/// No-op on Windows (executability is determined by file extension).
pub fn set_permissions_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perms)
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

/// Atomically swap a symlink by creating a temp symlink and renaming.
///
/// On Unix: creates temp symlink, then renames over target (atomic).
/// On Windows: stages the source, renames the target aside, then moves the
/// staged file into place. This avoids the lock on a running executable but is
/// not fully atomic.
pub fn atomic_symlink_swap(src: &Path, dst: &Path, temp: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(temp);
        std::os::unix::fs::symlink(src, temp)?;
        std::fs::rename(temp, dst)?;
    }
    #[cfg(windows)]
    {
        // Windows keeps a loaded executable open, so removing or copying over
        // the PATH launcher fails with ERROR_SHARING_VIOLATION. It does allow
        // the directory entry to be renamed while the process keeps running
        // from its existing handle. Stage the new file, rename the old entry
        // aside, then put the staged file at the stable path. This is the same
        // rename-aside strategy used by the PowerShell installer.
        let _ = std::fs::remove_file(temp);
        std::fs::copy(src, temp)?;

        let operation_id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        );
        let old = dst.parent().unwrap_or_else(|| Path::new(".")).join(format!(
            ".jcode-launcher-old-{operation_id}{}",
            dst.extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
                .unwrap_or_default()
        ));
        let mut moved_old = false;
        if dst.exists() {
            match std::fs::rename(dst, &old) {
                Ok(()) => moved_old = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    let _ = std::fs::remove_file(temp);
                    return Err(error);
                }
            }
        }

        if let Err(error) = std::fs::rename(temp, dst) {
            if moved_old {
                let _ = std::fs::rename(&old, dst);
            }
            let _ = std::fs::remove_file(temp);
            return Err(error);
        }

        // An old loaded executable cannot be deleted until its process exits,
        // so this only succeeds when nothing holds it. The sweep below is what
        // actually collects the leftovers, on a later swap once they are free.
        if moved_old {
            let _ = std::fs::remove_file(old);
        }
        remove_stale_rename_aside(dst.parent().unwrap_or_else(|| Path::new(".")));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::atomic_symlink_swap;

    #[test]
    fn windows_swap_replaces_existing_launcher_via_staged_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let src = dir.path().join("source.exe");
        let dst = dir.path().join("jcode.exe");
        let temp = dir.path().join(".jcode-current");
        std::fs::write(&src, b"new binary").expect("source");
        std::fs::write(&dst, b"old binary").expect("destination");

        atomic_symlink_swap(&src, &dst, &temp).expect("swap succeeds");

        assert_eq!(std::fs::read(&dst).expect("new launcher"), b"new binary");
        assert!(!temp.exists());
        assert!(
            !dir.path()
                .join(format!(".jcode-launcher-old-{}.exe", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn windows_swap_sweeps_orphans_left_by_earlier_swaps() {
        // Orphans accumulate because a loaded .exe cannot be deleted at the
        // moment it is renamed aside. A later swap must collect them.
        let dir = tempfile::tempdir().expect("temporary directory");
        let src = dir.path().join("source.exe");
        let dst = dir.path().join("jcode.exe");
        let temp = dir.path().join(".jcode-current");
        std::fs::write(&src, b"new binary").expect("source");
        std::fs::write(&dst, b"old binary").expect("destination");

        let launcher_orphan = dir.path().join(".jcode-launcher-old-1234-5678.exe");
        let rotated_orphan = dir.path().join("jcode.exe.old-1786576183");
        let unrelated = dir.path().join("keep-me.exe");
        for path in [&launcher_orphan, &rotated_orphan, &unrelated] {
            std::fs::write(path, b"stale").expect("orphan");
        }

        atomic_symlink_swap(&src, &dst, &temp).expect("swap succeeds");

        assert!(!launcher_orphan.exists(), "launcher orphan should be swept");
        assert!(!rotated_orphan.exists(), "rotated orphan should be swept");
        assert!(unrelated.exists(), "unrelated files must survive the sweep");
        assert_eq!(std::fs::read(&dst).expect("new launcher"), b"new binary");
    }
}
