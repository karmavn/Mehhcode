use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const STALE_AGENTS_TOKENS: &[&str] = &[
    "packages/tui",
    "packages/coding-agent",
    "packages/ai",
    "models.generated.ts",
    "biome.json",
    "tsconfig.base.json",
    "vitest",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/mehh-cli must live two levels below workspace root")
        .to_path_buf()
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_internal_deps(cargo_toml: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let mut in_prod_deps = false;

    for raw_line in cargo_toml.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_prod_deps = line == "[dependencies]"
                || (line.starts_with("[target.") && line.ends_with(".dependencies]"));
            if let Some(rest) = line.strip_prefix("[dependencies.") {
                if let Some(name) = rest.strip_suffix(']') {
                    let name = name.trim().trim_matches('"').trim_matches('\'');
                    if name.starts_with("mehh-") {
                        deps.insert(name.to_string());
                    }
                }
            } else if line.starts_with("[target.") {
                if let Some((_, rest)) = line.split_once(".dependencies.") {
                    if let Some(name) = rest.strip_suffix(']') {
                        let name = name.trim().trim_matches('"').trim_matches('\'');
                        if name.starts_with("mehh-") {
                            deps.insert(name.to_string());
                        }
                    }
                }
            }
            continue;
        }

        if !in_prod_deps {
            continue;
        }

        if let Some((lhs, _)) = line.split_once('=') {
            let key = lhs.split('.').next().unwrap_or("").trim();
            let key = key.trim_matches('"').trim_matches('\'');
            if key.starts_with("mehh-") {
                deps.insert(key.to_string());
            }
        }
    }

    deps
}

fn has_lints_configured(cargo_toml: &str) -> bool {
    let mut section = String::new();

    for raw_line in cargo_toml.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_string();
            continue;
        }
        if let Some((lhs, rhs)) = line.split_once('=') {
            if section == "[lints]" && lhs.trim() == "workspace" && rhs.trim() == "true" {
                return true;
            }
        }
    }

    false
}

fn has_utf8_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xEF, 0xBB, 0xBF])
}

fn find_stale_agents_tokens(content: &str) -> Vec<&'static str> {
    STALE_AGENTS_TOKENS
        .iter()
        .copied()
        .filter(|token| content.contains(token))
        .collect()
}

fn extract_repo_paths_from_markdown(md: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    let mut rest = md;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let token = rest[..end].trim();
        rest = &rest[end + 1..];

        if token.contains('*') || token.contains(' ') || token.is_empty() {
            continue;
        }
        let cleaned = token
            .split('#')
            .next()
            .unwrap_or(token)
            .split(':')
            .next()
            .unwrap_or(token);
        let is_repo_path = cleaned.starts_with("crates/")
            || cleaned.starts_with(".github/")
            || matches!(
                cleaned,
                "Cargo.toml" | "clippy.toml" | "deny.toml" | "AGENTS.md" | "FEATURE_MAP.md"
            );
        if is_repo_path {
            paths.insert(cleaned.to_string());
        }
    }
    paths.into_iter().collect()
}

fn allowed_crate_layers() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut map = BTreeMap::new();
    // Layer 0: foundations (no internal mehh-* dependencies)
    map.insert("mehh-ai", BTreeSet::new());
    map.insert("mehh-execpolicy", BTreeSet::new());
    map.insert("mehh-sandbox", BTreeSet::new());
    map.insert("mehh-tui", BTreeSet::new());
    // Layer 1: core agent loop
    map.insert("mehh-agent", BTreeSet::from(["mehh-ai"]));
    // Layer 2: capabilities (no cross-dependencies, no mehh-tui or mehh-coding-agent)
    map.insert("mehh-codemode", BTreeSet::from(["mehh-agent", "mehh-ai"]));
    map.insert(
        "mehh-mcp",
        BTreeSet::from(["mehh-agent", "mehh-ai", "mehh-sandbox"]),
    );
    map.insert(
        "mehh-tools",
        BTreeSet::from(["mehh-agent", "mehh-ai", "mehh-sandbox"]),
    );
    // Layer 3: session/TUI/RPC orchestration
    map.insert(
        "mehh-coding-agent",
        BTreeSet::from([
            "mehh-agent",
            "mehh-ai",
            "mehh-codemode",
            "mehh-execpolicy",
            "mehh-mcp",
            "mehh-sandbox",
            "mehh-tools",
            "mehh-tui",
        ]),
    );
    // Layer 4: CLI binary entrypoint
    map.insert(
        "mehh-cli",
        BTreeSet::from(["mehh-agent", "mehh-ai", "mehh-coding-agent", "mehh-tools"]),
    );
    map
}

fn collect_files_with_extensions(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).expect("directory must be readable");
    for entry in entries {
        let entry = entry.expect("dir entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_files_with_extensions(&path, exts, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if exts.contains(&ext) {
                out.push(path);
            }
        }
    }
}

#[test]
fn crate_dependency_layers_are_strictly_bounded() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let allowed = allowed_crate_layers();

    let mut discovered = BTreeSet::new();
    for entry in fs::read_dir(&crates_dir).expect("crates/ directory must exist") {
        let entry = entry.expect("crate dir entry must be readable");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let crate_name = entry
            .file_name()
            .into_string()
            .expect("crate directory name must be valid UTF-8");
        discovered.insert(crate_name.clone());

        let allowed_deps = allowed.get(crate_name.as_str()).unwrap_or_else(|| {
            panic!(
                "Unlisted crate '{crate_name}' in crates/. Register its layer explicitly in allowed_crate_layers()."
            )
        });

        let cargo_toml = fs::read_to_string(path.join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("failed to read crates/{crate_name}/Cargo.toml: {e}"));
        let actual_deps = parse_internal_deps(&cargo_toml);

        for dep in &actual_deps {
            assert!(
                allowed_deps.contains(dep.as_str()),
                "Layer violation: crate '{crate_name}' depends on '{dep}', which is outside its allowed layer {allowed_deps:?}"
            );
        }
    }

    let expected_crates: BTreeSet<String> = allowed.keys().map(|s| (*s).to_string()).collect();
    assert_eq!(discovered, expected_crates, "workspace crates mismatch");
}

#[test]
fn all_workspace_crates_enable_guardrail_lints() {
    let root = workspace_root();
    let crates_dir = root.join("crates");

    for entry in fs::read_dir(&crates_dir).expect("crates/ directory must exist") {
        let entry = entry.expect("crate dir entry must be readable");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let cargo_path = path.join("Cargo.toml");
        let cargo_toml = fs::read_to_string(&cargo_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_path.display()));
        assert!(
            has_lints_configured(&cargo_toml),
            "{} must inherit `[lints] workspace = true`",
            cargo_path.display()
        );
    }
}

#[test]
fn workspace_source_files_have_no_utf8_bom() {
    let root = workspace_root();
    let mut files = Vec::new();
    for rel in ["Cargo.toml", "clippy.toml", "deny.toml", "AGENTS.md", "FEATURE_MAP.md"] {
        let p = root.join(rel);
        if p.exists() {
            files.push(p);
        }
    }
    let workflows = root.join(".github").join("workflows");
    if workflows.is_dir() {
        collect_files_with_extensions(&workflows, &["yml", "yaml"], &mut files);
    }
    collect_files_with_extensions(&root.join("crates"), &["rs", "toml"], &mut files);

    let mut bom_files = Vec::new();
    for file in files {
        let bytes = fs::read(&file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        if has_utf8_bom(&bytes) {
            bom_files.push(file);
        }
    }
    assert!(
        bom_files.is_empty(),
        "UTF-8 BOM detected in files: {bom_files:?}"
    );
}

#[test]
fn agents_md_has_no_stale_typescript_workspace_references() {
    let root = workspace_root();
    let agents_md = fs::read_to_string(root.join("AGENTS.md"))
        .expect("AGENTS.md must exist at workspace root");
    let stale = find_stale_agents_tokens(&agents_md);
    assert!(
        stale.is_empty(),
        "AGENTS.md still contains stale TypeScript workspace references: {stale:?}"
    );
}

#[test]
fn ci_enforces_production_unwrap_and_panic_guard() {
    let root = workspace_root();
    let ci_yml = fs::read_to_string(root.join(".github").join("workflows").join("ci.yml"))
        .expect(".github/workflows/ci.yml must exist");
    for required in [
        "--workspace",
        "--no-deps",
        "--lib",
        "--bins",
        "clippy::unwrap_used",
        "clippy::panic",
        "cargo-deny-action",
    ] {
        assert!(
            ci_yml.contains(required),
            "ci.yml must enforce '{required}'"
        );
    }
    let guard_line = ci_yml
        .lines()
        .find(|l| l.contains("clippy::unwrap_used") && l.contains("clippy::panic"))
        .expect("workspace unwrap/panic guard line must exist");
    assert!(
        guard_line.contains("run:")
            && guard_line.contains("--workspace")
            && guard_line.contains("--no-deps")
            && guard_line.contains("--lib")
            && guard_line.contains("--bins"),
        "workspace unwrap/panic guard must be a single-line step with --workspace --no-deps --lib --bins: {guard_line}"
    );
}

#[test]
fn feature_map_exists_and_all_referenced_paths_exist() {
    let root = workspace_root();
    let map_path = root.join("FEATURE_MAP.md");
    let content = fs::read_to_string(&map_path)
        .expect("FEATURE_MAP.md must exist at workspace root");
    let refs = extract_repo_paths_from_markdown(&content);
    assert!(
        refs.len() >= 15,
        "FEATURE_MAP.md should reference at least 15 real workspace paths, found {}",
        refs.len()
    );
    let missing: Vec<_> = refs
        .into_iter()
        .filter(|rel| !root.join(rel).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "FEATURE_MAP.md references non-existent paths: {missing:?}"
    );
}

#[test]
fn helper_detects_upward_or_cross_layer_dependency() {
    let manifest = r#"
[package]
name = "mehh-tools"

[dependencies]
mehh-ai = { workspace = true }
# mehh-tui = { workspace = true }

[target.'cfg(windows)'.dependencies]
mehh-coding-agent.workspace = true

[target.'cfg(unix)'.dependencies.mehh-execpolicy]
workspace = true

[dev-dependencies]
mehh-tui = { workspace = true }
"#;
    let deps = parse_internal_deps(manifest);
    assert_eq!(
        deps,
        BTreeSet::from([
            "mehh-ai".to_string(),
            "mehh-coding-agent".to_string(),
            "mehh-execpolicy".to_string(),
        ])
    );
}

#[test]
fn helper_detects_missing_lints_section() {
    let missing = "[package]\nname = \"mehh-foo\"\n[dependencies]\n";
    assert!(!has_lints_configured(missing));

    let inherited = "[package]\nname = \"mehh-foo\"\n[lints]\nworkspace = true\n";
    assert!(has_lints_configured(inherited));

    let local_only = "[lints.clippy]\ndbg_macro = \"deny\"\n";
    assert!(!has_lints_configured(local_only));
}

#[test]
fn helper_detects_utf8_bom() {
    assert!(has_utf8_bom(b"\xEF\xBB\xBFfn main() {}\n"));
    assert!(!has_utf8_bom(b"fn main() {}\n"));
}

#[test]
fn helper_detects_stale_agents_md_tokens() {
    assert_eq!(
        find_stale_agents_tokens("Run vitest in packages/tui and check biome.json"),
        vec!["packages/tui", "biome.json", "vitest"]
    );
    assert!(find_stale_agents_tokens("Run cargo test -p mehh-cli").is_empty());
}

#[test]
fn helper_extracts_feature_map_paths() {
    let sample = "See `crates/mehh-cli/src/main.rs#L656` and `crates/*/Cargo.toml` and `Cargo.toml:10` and `other/file.txt`.";
    assert_eq!(
        extract_repo_paths_from_markdown(sample),
        vec![
            "Cargo.toml".to_string(),
            "crates/mehh-cli/src/main.rs".to_string(),
        ]
    );
}
