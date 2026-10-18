//! Atomic file writes for the updater's settings: the new contents go to a
//! fresh file beside the target, are flushed to disk, then renamed over it.
//! A crash or a full disk leaves the old file or the new one, never half
//! of one. On unix the file is readable and writable by its owner only
//! (0600), as each app's own settings file was before it used this crate.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` atomically, making its folder if needed.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut f = options.open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return written;
    }
    // The rename itself reaches the disk once the folder is flushed. Not
    // every platform can open a folder for that; the write already stands.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep/updates.json");
        write_atomically(&file, b"one").unwrap();
        write_atomically(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["updates.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("updates.json");
        write_atomically(&file, b"one").unwrap();
        write_atomically(&file, b"two").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_failed_write_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("updates.json");
        write_atomically(&file, b"kept").unwrap();
        // A folder where the file should go: the rename fails.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(write_atomically(&blocked, b"x").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"kept");
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["blocked", "updates.json"]);
    }
}
