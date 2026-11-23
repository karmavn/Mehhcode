use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to a temporary file in the same directory, copy the target's
/// permissions when it exists, then replace the target in one rename. A failed
/// write leaves the original file untouched, which lets callers record undo
/// snapshots only after a confirmed successful write.
/// Resolve a symlink target relative to the link's own directory.
fn resolve_link_target(link_path: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        link_path.parent().unwrap_or_else(|| Path::new(".")).join(target)
    }
}

/// Resolve the final component of `path` so writing through a symlink updates
/// its target instead of replacing the link.
pub async fn resolve_write_target(path: &Path) -> PathBuf {
    // Renaming onto a symlink replaces the link itself. Resolve the final
    // component so editing a symlinked file still updates its target. A
    // dangling link cannot be canonicalized, so read its raw target and keep
    // following: a chain of dangling links would otherwise rename onto the
    // intermediate link instead of the final target. Cap the walk so a
    // symlink cycle cannot hang the write.
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let is_symlink = tokio::fs::symlink_metadata(&current)
            .await
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !is_symlink {
            return current;
        }
        match tokio::fs::canonicalize(&current).await {
            Ok(target) => return target,
            Err(_) => match tokio::fs::read_link(&current).await {
                Ok(target) => current = resolve_link_target(&current, &target),
                Err(_) => return current,
            },
        }
    }
    current
}

/// Target approved by the security inspector for this tool call, or the
/// freshly resolved target when the tool was invoked without inspection.
/// Falls back to `resolve_write_target` so dangling symlinks use the same
/// resolution rule on both sides of the comparison.
pub async fn inspected_target(
    params: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> PathBuf {
    match params
        .get(mehh_agent::INSPECTED_TARGET_ARG)
        .and_then(|v| v.as_str())
    {
        Some(raw) => PathBuf::from(raw),
        None => resolve_write_target(path).await,
    }
}

/// Write `bytes` only if `path` still resolves to `expected_target`, which the
/// caller captured when it inspected the path.
pub async fn atomic_write_checked(
    path: &Path,
    expected_target: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    atomic_write_inner(path, Some(expected_target), bytes).await
}

pub(crate) fn targets_match(current: &Path, expected: &Path) -> bool {
    crate::core::context::canonicalize_lossy(current)
        == crate::core::context::canonicalize_lossy(expected)
}

async fn atomic_write_inner(
    requested: &Path,
    expected_target: Option<&Path>,
    bytes: &[u8],
) -> Result<(), String> {
    let resolved = resolve_write_target(requested).await;
    if let Some(expected) = expected_target {
        if !targets_match(&resolved, expected) {
            return Err(format!(
                "Refusing to write {}: target changed since it was inspected",
                requested.display()
            ));
        }
    }
    let path = resolved.as_path();

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("Invalid file path: {}", path.display()))?;
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(
        ".{}.mehh-tmp-{}-{}",
        file_name.to_string_lossy(),
        std::process::id(),
        id
    );
    let temp_path = parent.join(temp_name);

    let permissions = match tokio::fs::metadata(path).await {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Failed to stat {}: {}", path.display(), e)),
    };

    let write_result = async {
        tokio::fs::write(&temp_path, bytes)
            .await
            .map_err(|e| format!("Failed to write temporary file: {e}"))?;
        if let Some(perms) = permissions {
            tokio::fs::set_permissions(&temp_path, perms)
                .await
                .map_err(|e| format!("Failed to copy permissions: {e}"))?;
        }
        if let Some(expected) = expected_target {
            let current = resolve_write_target(requested).await;
            if !targets_match(&current, expected) {
                return Err(format!(
                    "Refusing to write {}: target changed since it was inspected",
                    requested.display()
                ));
            }
        }
        tokio::fs::rename(&temp_path, path)
            .await
            .map_err(|e| format!("Failed to replace {}: {e}", path.display()))
    }
    .await;

    if write_result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_link_target_is_relative_to_the_link_directory() {
        let link = Path::new("/work/config/link");
        assert_eq!(
            resolve_link_target(link, Path::new("target.cfg")),
            PathBuf::from("/work/config/target.cfg")
        );
        assert_eq!(
            resolve_link_target(link, Path::new("../shared/target.cfg")),
            PathBuf::from("/work/config/../shared/target.cfg")
        );
        assert_eq!(
            resolve_link_target(link, Path::new("/etc/app.cfg")),
            PathBuf::from("/etc/app.cfg")
        );
    }

    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (target, link);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symlinks are not supported on this platform",
            ))
        }
    }

    #[tokio::test]
    async fn checked_write_rejects_a_target_that_no_longer_matches() {
        let dir = crate::TempTestDir::new("fs_checked_mismatch");
        let intended = dir.path().join("intended.txt");
        let other = dir.path().join("other.txt");
        std::fs::write(&intended, b"intended").unwrap();
        std::fs::write(&other, b"other").unwrap();

        let expected = crate::core::context::canonicalize_lossy(&other);
        let err = atomic_write_checked(&intended, &expected, b"new")
            .await
            .unwrap_err();
        assert!(
            err.contains("changed since it was inspected"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&intended).unwrap(), b"intended");
    }

    #[tokio::test]
    async fn checked_write_rejects_symlink_swapped_after_inspection() {
        let dir = crate::TempTestDir::new("fs_checked_symlink");
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        if let Err(e) = create_file_symlink(&first, &link) {
            eprintln!("skipping symlink swap test: cannot create symlinks ({e})");
            return;
        }
        let expected = resolve_write_target(&link).await;

        std::fs::remove_file(&link).unwrap();
        if let Err(e) = create_file_symlink(&second, &link) {
            eprintln!("skipping symlink swap test: cannot recreate symlink ({e})");
            return;
        }

        let err = atomic_write_checked(&link, &expected, b"new")
            .await
            .unwrap_err();
        assert!(
            err.contains("changed since it was inspected"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[tokio::test]
    async fn checked_write_follows_a_dangling_symlink_target() {
        let dir = crate::TempTestDir::new("fs_checked_dangling");
        let target = dir.path().join("new.txt");
        let link = dir.path().join("link.txt");

        if let Err(e) = create_file_symlink(&target, &link) {
            eprintln!("skipping dangling symlink test: cannot create symlinks ({e})");
            return;
        }
        let expected = resolve_write_target(&link).await;

        atomic_write_checked(&link, &expected, b"data").await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"data");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    }
}
