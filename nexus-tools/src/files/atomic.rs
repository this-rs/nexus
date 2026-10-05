//! Writing a file so that an interruption never leaves it truncated (N19).
//!
//! The new content goes to a temporary file **in the same directory** (same filesystem, so
//! the final `rename` is atomic), is flushed to disk, then replaces the target in one step.
//! A crash before the rename leaves the old file whole; after it, the new one whole.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Replaces (or creates) `target` with `content`, keeping the permissions of an existing file.
/// If `backup_dir` is given, the previous content is copied there first.
pub fn write(target: &Path, content: &[u8], backup_dir: Option<&Path>) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "a file has a parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let existing = std::fs::metadata(target).ok();
    if let (Some(dir), Some(_)) = (backup_dir, &existing) {
        backup(target, dir)?;
    }
    let mut temp = tempfile_in(parent)?;
    temp.1.write_all(content)?;
    temp.1.sync_all()?;
    if let Some(meta) = &existing {
        std::fs::set_permissions(&temp.0, meta.permissions())?;
    }
    if let Err(error) = std::fs::rename(&temp.0, target) {
        let _ = std::fs::remove_file(&temp.0);
        return Err(error);
    }
    Ok(())
}

fn backup(target: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::fs::copy(target, dir.join(format!("{stamp}-{name}")))?;
    Ok(())
}

/// A uniquely named file next to the target, created exclusively.
fn tempfile_in(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let pid = std::process::id();
    for attempt in 0..100u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!(".nexus-tools-{pid}-{nanos}-{attempt}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("no free temporary file name"))
}
