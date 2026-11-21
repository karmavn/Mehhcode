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

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(parts.last(), Some(Component::Normal(_)) | Some(Component::RootDir)) =>
            {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            _ => parts.push(component),
        }
    }
    parts.into_iter().collect()
}

/// Resolve an existing or not-yet-created path to an absolute form. Symbolic
/// links are followed for the deepest existing ancestor.
pub fn canonicalize_lossy(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let normalized = lexical_normalize(path);
    let mut ancestors = Vec::new();
    let mut current = normalized.as_path();
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            ancestors.push(name.to_os_string());
        }
        if let Ok(canonical_parent) = parent.canonicalize() {
            let mut result = canonical_parent;
            for name in ancestors.into_iter().rev() {
                result.push(name);
            }
            return result;
        }
        current = parent;
    }
    normalized
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

        let resolved = canonicalize_lossy(candidate);
        let canonical_roots: Vec<PathBuf> = roots
            .iter()
            .map(|root| canonicalize_lossy(root))
            .collect();
        if canonical_roots
            .iter()
            .any(|root| resolved.starts_with(root))
        {
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
}
