use std::path::{Component, Path, PathBuf};

/// Boundary policy shared by every tool. `Unrestricted` preserves the
/// documented behavior (absolute paths and `..` are allowed);
/// `WorkspaceOnly` fails closed for anything that resolves outside the roots.
#[derive(Debug, Clone, Default)]
pub enum PathPolicy {
    #[default]
    Unrestricted,
    WorkspaceOnly(Vec<PathBuf>),
}

fn canonicalize_lossy_checked(path: &Path) -> Result<(PathBuf, bool), String> {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| {
                format!(
                    "Failed to determine current directory for {}: {e}",
                    path.display()
                )
            })?
    };

    if !anchored
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        if let Ok(canon) = anchored.canonicalize() {
            return Ok((canon, false));
        }
        let mut cur = anchored.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        while let Some(parent) = cur.parent() {
            if let Some(Component::Normal(seg)) = cur.components().next_back() {
                tail.push(seg);
            }
            if let Ok(mut canon) = parent.canonicalize() {
                for seg in tail.into_iter().rev() {
                    canon.push(seg);
                }
                return Ok((canon, false));
            }
            cur = parent;
        }
    }

    let mut acc = PathBuf::new();
    let mut escaped_above_root = false;

    for component in anchored.components() {
        match component {
            Component::Prefix(prefix) => {
                acc.push(prefix.as_os_str());
            }
            Component::RootDir => {
                acc.push(Component::RootDir);
                if let Ok(canon) = acc.canonicalize() {
                    acc = canon;
                }
            }
            Component::CurDir => {}
            Component::Normal(seg) => {
                acc.push(seg);
                if let Ok(canon) = acc.canonicalize() {
                    acc = canon;
                }
            }
            Component::ParentDir => {
                if matches!(acc.components().next_back(), Some(Component::Normal(_))) {
                    acc.pop();
                } else {
                    escaped_above_root = true;
                    acc.push(Component::ParentDir);
                }
            }
        }
    }

    Ok((acc, escaped_above_root))
}

/// Resolve an existing or not-yet-created path to an absolute form. Relative
/// paths are anchored to `current_dir()`, and each existing prefix is
/// canonicalized left-to-right before any subsequent `..` is folded.
pub fn canonicalize_lossy(path: &Path) -> PathBuf {
    canonicalize_lossy_checked(path)
        .map(|(resolved, _)| resolved)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Normalize a path for equality/prefix comparison: strip Windows verbatim
/// prefixes (`\\?\UNC\` -> `\\`, `\\?\` -> ``) and fold case on Windows/macOS.
pub fn normalize_path_for_comparison(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    let stripped: std::borrow::Cow<'_, str> = if raw
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case(r"\\?\UNC\") || p.eq_ignore_ascii_case("//?/UNC/"))
    {
        std::borrow::Cow::Owned(format!(r"\\{}", &raw[8..]))
    } else if raw
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case(r"\\?\") || p.eq_ignore_ascii_case("//?/"))
    {
        std::borrow::Cow::Borrowed(&raw[4..])
    } else {
        std::borrow::Cow::Borrowed(raw)
    };

    // macOS APFS is case-insensitive by default, like Windows NTFS.
    if cfg!(any(windows, target_os = "macos")) {
        PathBuf::from(stripped.to_lowercase())
    } else {
        PathBuf::from(stripped.as_ref())
    }
}

pub(crate) fn is_protected_credential_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".mehh")
        || name.eq_ignore_ascii_case(".ssh")
        || name.eq_ignore_ascii_case(".env")
        || (name.len() > 5
            && name
                .as_bytes()
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case(b".env.")))
        || name.eq_ignore_ascii_case("auth.json")
        || name.eq_ignore_ascii_case("mcp-auth.json")
        || name.eq_ignore_ascii_case("trust.json")
}

pub(crate) fn path_targets_protected_credential(raw_path: &str, resolved: &Path) -> bool {
    Path::new(raw_path).components().any(|c| {
        matches!(
            c,
            Component::Normal(s) if s.to_str().is_some_and(is_protected_credential_name)
        )
    }) || resolved
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(is_protected_credential_name)
}

impl PathPolicy {
    pub fn validate(&self, candidate: &Path) -> Result<(), String> {
        let roots = match self {
            PathPolicy::Unrestricted => return Ok(()),
            PathPolicy::WorkspaceOnly(roots) if roots.is_empty() => {
                return Err("WorkspaceOnly policy has no allowed roots".to_string());
            }
            PathPolicy::WorkspaceOnly(roots) => roots,
        };

        let (resolved_raw, escaped) = canonicalize_lossy_checked(candidate)?;
        if escaped {
            return Err(format!(
                "Path {} is outside the allowed workspace",
                candidate.display()
            ));
        }
        let resolved = normalize_path_for_comparison(&resolved_raw);
        if resolved.as_os_str().is_empty() {
            return Err(format!(
                "Path {} is outside the allowed workspace",
                candidate.display()
            ));
        }
        let allowed = roots.iter().any(|root| {
            if root.as_os_str().is_empty() {
                return false;
            }
            let Ok((root_raw, root_escaped)) = canonicalize_lossy_checked(root) else {
                return false;
            };
            if root_escaped {
                return false;
            }
            let normalized_root = normalize_path_for_comparison(&root_raw);
            if normalized_root.as_os_str().is_empty() {
                return false;
            }
            resolved.starts_with(&normalized_root)
        });
        if allowed {
            Ok(())
        } else {
            Err(format!(
                "Path {} is outside the allowed workspace",
                candidate.display()
            ))
        }
    }
}

/// Resolve `raw` against `cwd` and enforce `policy` in one place.
pub fn resolve_path(cwd: &Path, policy: &PathPolicy, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // Do not rewrite the path for the caller: lexical `..` folding changes
    // meaning when a symlink is involved. Normalization is only for policy
    // validation; Unrestricted passes the original joined path through.
    policy.validate(&candidate)?;
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_allows_absolute_and_parent_paths() {
        let cwd = Path::new("/work/project");
        let policy = PathPolicy::Unrestricted;
        assert_eq!(resolve_path(cwd, &policy, "src/lib.rs").unwrap(), cwd.join("src/lib.rs"));
        assert_eq!(resolve_path(cwd, &policy, "../other").unwrap(), cwd.join("../other"));
    }

    #[test]
    fn workspace_only_rejects_escape_and_empty_roots_fail_closed() {
        let root = std::env::temp_dir().join("mehh_policy_root");
        std::fs::create_dir_all(&root).unwrap();
        let policy = PathPolicy::WorkspaceOnly(vec![root.clone()]);
        assert!(resolve_path(&root, &policy, "inside.txt").is_ok());
        assert!(resolve_path(&root, &policy, "../outside.txt").is_err());

        let empty = PathPolicy::WorkspaceOnly(Vec::new());
        assert!(resolve_path(&root, &empty, "inside.txt").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn workspace_only_rejects_parent_traversal_through_symlink() {
        let ws = crate::TempTestDir::new("policy_symlink_ws");
        let outside = crate::TempTestDir::new("policy_symlink_outside");
        let outside_sub = outside.path().join("sub");
        std::fs::create_dir_all(&outside_sub).unwrap();
        let link = ws.path().join("symlink");

        #[cfg(unix)]
        let link_res = std::os::unix::fs::symlink(&outside_sub, &link);
        #[cfg(windows)]
        let link_res = std::os::windows::fs::symlink_dir(&outside_sub, &link);
        #[cfg(not(any(unix, windows)))]
        let link_res: std::io::Result<()> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported",
        ));

        if let Err(e) = link_res {
            eprintln!("skipping symlink traversal test: {e}");
            return;
        }

        let policy = PathPolicy::WorkspaceOnly(vec![ws.path().to_path_buf()]);
        let err = resolve_path(ws.path(), &policy, "symlink/../new.txt").unwrap_err();
        assert!(
            err.contains("outside the allowed workspace"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn workspace_only_handles_relative_cwd_dot() {
        let cwd_abs = std::env::current_dir().unwrap();
        let policy_dot = PathPolicy::WorkspaceOnly(vec![PathBuf::from(".")]);
        assert!(resolve_path(Path::new("."), &policy_dot, "inside.txt").is_ok());
        assert!(resolve_path(Path::new("."), &policy_dot, "../outside.txt").is_err());

        let policy_abs = PathPolicy::WorkspaceOnly(vec![cwd_abs]);
        assert!(resolve_path(Path::new("."), &policy_abs, "inside.txt").is_ok());
    }

    #[test]
    fn workspace_only_rejects_parent_dir_above_root() {
        #[cfg(unix)]
        {
            let policy = PathPolicy::WorkspaceOnly(vec![PathBuf::from("/")]);
            assert!(policy.validate(Path::new("/../outside.txt")).is_err());
        }
        #[cfg(windows)]
        {
            let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
            let root = PathBuf::from(format!(r"{sys_drive}\"));
            let policy = PathPolicy::WorkspaceOnly(vec![root.clone()]);
            assert!(policy.validate(&root.join(r"..\outside.txt")).is_err());
        }
    }

    #[test]
    fn workspace_only_matches_case_insensitively_on_windows_and_macos() {
        let dir = crate::TempTestDir::new("policy_case");
        let sub = dir.path().join("CamelCaseDir");
        std::fs::create_dir_all(&sub).unwrap();

        let swapped_root = PathBuf::from(
            dir.path()
                .to_string_lossy()
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect::<String>(),
        );
        let policy = PathPolicy::WorkspaceOnly(vec![swapped_root]);
        let candidate = sub.join("NewFile.txt");

        if cfg!(any(windows, target_os = "macos")) {
            assert!(
                policy.validate(&candidate).is_ok(),
                "expected case-insensitive match on Windows/macOS"
            );
        }

        assert_eq!(
            normalize_path_for_comparison(Path::new(r"\\?\UNC\Server\Share\File.txt")),
            if cfg!(any(windows, target_os = "macos")) {
                PathBuf::from(r"\\server\share\file.txt")
            } else {
                PathBuf::from(r"\\Server\Share\File.txt")
            }
        );
        assert_eq!(
            normalize_path_for_comparison(Path::new(r"\\?\C:\Work\File.txt")),
            if cfg!(any(windows, target_os = "macos")) {
                PathBuf::from(r"c:\work\file.txt")
            } else {
                PathBuf::from(r"C:\Work\File.txt")
            }
        );
    }

    #[test]
    fn workspace_only_rejects_empty_root_path() {
        let dir = crate::TempTestDir::new("policy_empty_root");
        let policy = PathPolicy::WorkspaceOnly(vec![PathBuf::from("")]);
        assert!(
            policy.validate(&dir.path().join("file.txt")).is_err(),
            "empty root must never match arbitrary paths"
        );
    }
}
