//! Private, atomic file writes shared by the snapshot, caches and state files.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Creates `dir` (and missing parents) with mode 0700. An existing
/// directory is left as it is.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)
}

/// Like [`ensure_dir`], and also tightens an existing `dir` to 0700. Only
/// for OpenCUT's own directories (snapshot, cache), never for a directory
/// the user chose, such as the parent of `--config`.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    ensure_dir(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(dir)?;
        if meta.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn open_private(p: &Path) -> std::io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let f = o.mode(0o600).open(p)?;
        // `mode` only applies on creation; fix a leftover temp file too.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(f)
    }
    #[cfg(not(unix))]
    o.open(p)
}

/// Writes `bytes` to `path` atomically: a 0600 temp file in the same
/// directory (created 0700 if missing; an existing directory's mode is not
/// changed), fsync, then rename. The temp file is removed on failure.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = open_private(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Permission bits of `path` (`None` if it cannot be stat'ed).
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        fs::metadata(path).ok().map(|_| 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_private_and_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("opencut");
        let f = dir.join("usage.json");
        write_private_atomic(&f, b"{}").unwrap();
        write_private_atomic(&f, b"{\"x\":1}").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"{\"x\":1}");
        assert_eq!(mode_of(&f), Some(0o600));
        assert_eq!(mode_of(&dir), Some(0o700));
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn tightens_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&dir).unwrap();
        assert_eq!(mode_of(&dir), Some(0o700));
    }

    #[cfg(unix)]
    #[test]
    fn write_keeps_existing_dir_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("user-chosen");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        write_private_atomic(&dir.join("config.toml"), b"x").unwrap();
        assert_eq!(mode_of(&dir), Some(0o755));
        assert_eq!(mode_of(&dir.join("config.toml")), Some(0o600));
    }
}
