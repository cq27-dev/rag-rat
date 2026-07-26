use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Write `contents` to `path` atomically: write a sibling temp file in the same directory,
/// flush + fsync it, then `rename` it over the target. `rename(2)` is atomic on POSIX, so a
/// crash, full disk, or `kill` mid-write leaves the existing file intact rather than truncated.
///
/// Used for files we must never clobber on a partial write — the user-authored, non-regenerable
/// `.claude/settings.json`, and the managed git hook scripts. See #52: the previous `fs::write`
/// truncates first, so an interrupted write destroyed the original.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let dir = parent.unwrap_or_else(|| Path::new("."));

    // Temp sibling in the same directory so the final rename never crosses a filesystem
    // boundary (cross-device rename is not atomic and errors). PID keeps concurrent writers
    // from colliding on the temp name.
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.rag-rat.{}.tmp", std::process::id()));

    let write_result = (|| -> anyhow::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> rag_rat_base::test_scratch::ScratchDir {
        rag_rat_base::test_scratch::ScratchDir::new("atomic")
    }

    #[test]
    fn writes_new_file_with_exact_contents() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        write_atomic(&path, b"hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        fs::write(&path, b"old contents").unwrap();
        write_atomic(&path, b"new contents").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new contents");
    }

    #[test]
    fn leaves_no_temp_residue_behind() {
        let dir = temp_dir();
        let path = dir.join("hook");
        write_atomic(&path, b"#!/bin/sh\n").unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "hook")
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files left behind: {leftovers:?}");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir();
        let path = dir.join("nested").join(".claude").join("settings.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }
}
