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

pub async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // Renaming onto a symlink replaces the link itself. Resolve the final
    // component so editing a symlinked file still updates its target. A
    // dangling link cannot be canonicalized, so read its raw target instead;
    // if that target's parent does not exist the write fails naturally rather
    // than silently destroying the link.
    let resolved = if tokio::fs::symlink_metadata(path)
        .await
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        match tokio::fs::canonicalize(path).await {
            Ok(target) => target,
            Err(_) => match tokio::fs::read_link(path).await {
                Ok(target) => resolve_link_target(path, &target),
                Err(_) => path.to_path_buf(),
            },
        }
    } else {
        path.to_path_buf()
    };
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
}
