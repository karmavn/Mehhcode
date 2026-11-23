use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mehh_agent::AgentTool;
use mehh_tools::core::context::PathPolicy;
use mehh_tools::{
    select_coding_tools, AnalyzeTool, EditTool, EditUndoStore, FindTool, GrepTool, ReadTool,
    UndoEditTool, WriteTool,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mehh_tools_regression_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text_of(res: &mehh_agent::AgentToolResult) -> String {
    match &res.content[0] {
        mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text block"),
    }
}

#[tokio::test]
async fn analyze_truncate_is_utf8_safe() {
    let dir = TempDir::new("analyze_utf8");
    let file = dir.path().join("huge.rs");

    // Build a single Rust symbol line whose 3-byte character starts exactly at
    // output byte 23999, so byte 24000 falls in the middle of that character.
    let header = format!("Symbols in {} (total: 1):\n  L1: ", file.display());
    let mut line = String::from("fn a() { /* ");
    let ascii = 23_999usize - header.len() - line.len();
    line.push_str(&"a".repeat(ascii));
    line.push('\u{251c}');
    line.push_str(" */ }");
    std::fs::write(&file, format!("{line}\n")).unwrap();

    let tool = AnalyzeTool::new(dir.path().to_path_buf());
    let params = json!({
        "path": file.to_string_lossy(),
        "mode": "symbols"
    })
    .as_object()
    .unwrap()
    .clone();

    // Before the fix this panics at output.truncate(24_000).
    let res = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("analyze must not panic and must return output");
    assert!(text_of(&res).contains("truncated"));
}

#[tokio::test]
async fn edit_rejects_missing_new_text_in_edits_array() {
    let dir = TempDir::new("edit_missing_new");
    let file = dir.path().join("code.rs");
    let original = "fn main() {\n    let x = 1;\n}\n";
    std::fs::write(&file, original).unwrap();

    let tool = EditTool::new(dir.path().to_path_buf());
    let params = json!({
        "path": "code.rs",
        "edits": [{ "oldText": "let x = 1;" }]
    })
    .as_object()
    .unwrap()
    .clone();

    // Before the fix newText defaults to "" and silently deletes the code.
    assert!(tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[tokio::test]
async fn edit_ignores_crlf_inside_string_literal_when_choosing_line_ending() {
    let dir = TempDir::new("edit_literal_crlf");
    let file = dir.path().join("mixed.rs");
    // The line contains a CRLF byte pair inside a string literal. A naive
    // "next \n with preceding \r" check turns the new_text into CRLF.
    let initial = "fn test() {\n    let y = 1; let s = \"\r\n\";\n}\n";
    std::fs::write(&file, initial).unwrap();

    let tool = EditTool::new(dir.path().to_path_buf());
    let params = json!({
        "path": "mixed.rs",
        "oldText": "let y = 1;",
        "newText": "let y = 2;\nlet z = 3;"
    })
    .as_object()
    .unwrap()
    .clone();

    tool.execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("edit");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn test() {\n    let y = 2;\nlet z = 3; let s = \"\r\n\";\n}\n"
    );
}

fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[tokio::test]
async fn write_through_symlink_preserves_the_link() {
    let dir = TempDir::new("symlink_write");
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    std::fs::write(&target, "old").unwrap();
    // Symlink creation needs extra privileges on some Windows setups.
    if make_symlink(Path::new("target.txt"), &link).is_err() {
        return;
    }

    let tool = WriteTool::new(dir.path().to_path_buf());
    let params = json!({ "path": "link.txt", "content": "new" })
        .as_object()
        .unwrap()
        .clone();
    tool.execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("write through symlink");

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn write_through_dangling_symlink_preserves_the_link() {
    let dir = TempDir::new("dangling_symlink_write");
    let link = dir.path().join("link.txt");
    if make_symlink(Path::new("missing.txt"), &link).is_err() {
        return;
    }

    let tool = WriteTool::new(dir.path().to_path_buf());
    let params = json!({ "path": "link.txt", "content": "created" })
        .as_object()
        .unwrap()
        .clone();
    tool.execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("write through dangling symlink");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("missing.txt")).unwrap(),
        "created"
    );
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn undo_survives_a_failed_restore() {
    let dir = TempDir::new("undo_retry");
    let file = dir.path().join("code.txt");
    std::fs::write(&file, "original").unwrap();

    let store = Arc::new(EditUndoStore::new());
    let edit = EditTool::new(dir.path().to_path_buf()).with_undo_store(store.clone());
    let edit_params = json!({
        "path": "code.txt",
        "oldText": "original",
        "newText": "changed"
    })
    .as_object()
    .unwrap()
    .clone();
    edit.execute("edit", edit_params, CancellationToken::new(), None)
        .await
        .expect("edit");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed");

    // Force the restore to fail by putting a directory at the target path.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let undo = UndoEditTool::new(dir.path().to_path_buf(), store);
    let params = json!({ "path": "code.txt" }).as_object().unwrap().clone();
    assert!(undo
        .execute("undo1", params.clone(), CancellationToken::new(), None)
        .await
        .is_err());

    // The history must still be there, so a retry works.
    std::fs::remove_dir(&file).unwrap();
    std::fs::write(&file, "changed").unwrap();
    undo.execute("undo2", params, CancellationToken::new(), None)
        .await
        .expect("retry must succeed");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
}

#[tokio::test]
async fn read_rejects_binary_file() {
    let dir = TempDir::new("read_binary");
    let file = dir.path().join("blob.bin");
    std::fs::write(&file, [0x00, 0x01, 0x02, 0x00, b'h', b'i']).unwrap();

    let tool = ReadTool::new(dir.path().to_path_buf());
    let params = json!({ "path": "blob.bin" }).as_object().unwrap().clone();

    // Before the fix this returns lossy-decoded garbage as text.
    let err = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect_err("binary files must be rejected");
    assert!(err.contains("binary"), "unexpected error: {err}");
}

#[tokio::test]
async fn read_caps_total_output_bytes() {
    let dir = TempDir::new("read_cap");
    let file = dir.path().join("large.txt");
    let line = "x".repeat(1000);
    std::fs::write(&file, vec![format!("{line}\n"); 2500].concat()).unwrap();

    let tool = ReadTool::new(dir.path().to_path_buf());
    let params = json!({ "path": "large.txt", "limit": 2500 })
        .as_object()
        .unwrap()
        .clone();

    let res = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("read");
    // Before the fix this is ~2.5 MB.
    assert!(text_of(&res).len() <= 128 * 1024);
}

#[tokio::test]
async fn find_returns_files_only() {
    let dir = TempDir::new("find_files_only");
    std::fs::create_dir_all(dir.path().join("subdir")).unwrap();
    std::fs::write(dir.path().join("file.txt"), "x").unwrap();

    let tool = FindTool::new(dir.path().to_path_buf());
    let params = json!({ "pattern": "*" }).as_object().unwrap().clone();
    let res = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("find");
    let text = text_of(&res);
    // Before the fix subdir/ is mixed into a tool documented to find files.
    assert!(!text.contains("subdir/"), "unexpected output: {text}");
    assert!(text.contains("file.txt"));
}

#[tokio::test]
async fn grep_glob_is_relative_to_search_path() {
    let dir = TempDir::new("grep_glob_rel");
    std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
    std::fs::write(dir.path().join("src/sub/foo.rs"), "needle_here").unwrap();

    let tool = GrepTool::new(dir.path().to_path_buf());
    let params = json!({
        "pattern": "needle",
        "path": "src",
        "glob": "sub/*.rs"
    })
    .as_object()
    .unwrap()
    .clone();

    // Before the fix rel_path is "src/sub/foo.rs" so "sub/*.rs" never matches.
    let res = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("grep");
    assert!(text_of(&res).contains("needle_here"));
}

#[tokio::test]
async fn grep_context_limit_counts_matches_not_context_lines() {
    let dir = TempDir::new("grep_context_limit");
    std::fs::write(
        dir.path().join("hits.txt"),
        "hit_one\nmid\nhit_two\nmid\nhit_three\n",
    )
    .unwrap();

    let tool = GrepTool::new(dir.path().to_path_buf());
    let params = json!({
        "pattern": "hit_",
        "context": 1,
        "limit": 2
    })
    .as_object()
    .unwrap()
    .clone();

    let res = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("grep");
    let text = text_of(&res);
    assert!(text.contains("hit_one"), "first match missing: {text}");
    assert!(text.contains("hit_two"), "second match missing: {text}");
    assert!(!text.contains("hit_three"), "third match exceeded limit: {text}");
}

#[tokio::test]
async fn grep_reports_binary_file_instead_of_no_matches() {
    let dir = TempDir::new("grep_binary");
    let file = dir.path().join("blob.bin");
    std::fs::write(&file, [b'n', b'e', b'e', b'd', b'l', b'e', 0x00, b'x']).unwrap();

    let tool = GrepTool::new(dir.path().to_path_buf());
    let params = json!({
        "pattern": "needle",
        "path": "blob.bin"
    })
    .as_object()
    .unwrap()
    .clone();

    // Before the fix this silently returns "No matches found".
    let err = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect_err("binary file must be reported");
    assert!(err.contains("binary"), "unexpected error: {err}");
}

#[test]
fn select_coding_tools_rejects_unknown_names() {
    match select_coding_tools(
        PathBuf::from("."),
        Some(&["bash".to_string(), "bogus_tool".to_string()]),
        &[],
    ) {
        Ok(_) => panic!("unknown tool names must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("bogus_tool"),
            "unexpected error: {err}"
        ),
    }
}

#[tokio::test]
async fn write_through_two_hop_dangling_symlink_creates_final_target() {
    let dir = TempDir::new("two_hop_symlink");
    let final_target = dir.path().join("missing.txt");
    let link2 = dir.path().join("link2.txt");
    let link1 = dir.path().join("link1.txt");

    // link1 -> link2 -> missing.txt; both links are dangling.
    if let Err(e) = make_symlink(Path::new("missing.txt"), &link2) {
        eprintln!("skipping two-hop symlink test: cannot create symlinks ({e})");
        return;
    }
    if let Err(e) = make_symlink(Path::new("link2.txt"), &link1) {
        eprintln!("skipping two-hop symlink test: cannot create symlinks ({e})");
        return;
    }

    let tool = WriteTool::new(dir.path().to_path_buf());
    let params = json!({ "path": "link1.txt", "content": "created" })
        .as_object()
        .unwrap()
        .clone();
    tool.execute("tc", params, CancellationToken::new(), None)
        .await
        .expect("write through two-hop dangling symlink");

    assert_eq!(
        std::fs::read_to_string(&final_target).unwrap(),
        "created",
        "write must land on the final target, not on link2"
    );
    assert!(
        std::fs::symlink_metadata(&link2)
            .unwrap()
            .file_type()
            .is_symlink(),
        "link2 must still be a symlink"
    );
}

#[tokio::test]
async fn undo_creation_refuses_symlink_swap_to_unrelated_file() {
    let dir = TempDir::new("undo_symlink_swap");
    let original_target = dir.path().join("original.txt");
    let link = dir.path().join("P.txt");
    let unrelated = dir.path().join("Q.txt");
    std::fs::write(&unrelated, b"unrelated").unwrap();

    if let Err(e) = make_symlink(Path::new("original.txt"), &link) {
        eprintln!("skipping undo symlink-swap test: cannot create symlinks ({e})");
        return;
    }

    let store = Arc::new(EditUndoStore::new());
    let write = WriteTool::new(dir.path().to_path_buf()).with_undo_store(store.clone());
    let write_params = json!({ "path": "P.txt", "content": "created" })
        .as_object()
        .unwrap()
        .clone();
    write
        .execute("write", write_params, CancellationToken::new(), None)
        .await
        .expect("write through dangling symlink");
    assert_eq!(std::fs::read_to_string(&original_target).unwrap(), "created");

    // Replace P with a symlink to an existing, unrelated file Q.
    std::fs::remove_file(&link).unwrap();
    if let Err(e) = make_symlink(Path::new("Q.txt"), &link) {
        eprintln!("skipping undo symlink-swap test: cannot recreate symlink ({e})");
        return;
    }

    let undo = UndoEditTool::new(dir.path().to_path_buf(), store.clone());
    let mut undo_params = serde_json::Map::new();
    undo_params.insert("path".to_string(), json!("P.txt"));
    undo_params.insert(
        mehh_agent::INSPECTED_TARGET_ARG.to_string(),
        json!(original_target.to_string_lossy()),
    );
    let err = undo
        .execute("undo", undo_params, CancellationToken::new(), None)
        .await
        .expect_err("undo must refuse after the target changed");
    assert!(
        err.contains("changed since it was inspected"),
        "unexpected error: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&unrelated).unwrap(),
        "unrelated",
        "undo must not delete the unrelated file Q"
    );
    assert!(
        store.has_snapshot(&link),
        "a refused undo must keep the snapshot for a later retry"
    );
}

#[tokio::test]
async fn write_rejects_resolved_target_outside_workspace() {
    let ws = TempDir::new("policy_ws");
    let outside_dir = TempDir::new("policy_outside");
    let outside = outside_dir.path().join("outside.txt");
    let link = ws.path().join("escape.txt");

    if let Err(e) = make_symlink(&outside, &link) {
        eprintln!("skipping workspace policy test: cannot create symlinks ({e})");
        return;
    }

    let tool = WriteTool::new(ws.path().to_path_buf())
        .with_path_policy(PathPolicy::WorkspaceOnly(vec![ws.path().to_path_buf()]));
    let params = json!({ "path": "escape.txt", "content": "bad" })
        .as_object()
        .unwrap()
        .clone();
    let err = tool
        .execute("tc", params, CancellationToken::new(), None)
        .await
        .expect_err("resolved target outside the workspace must be rejected");
    assert!(
        err.contains("outside the allowed workspace"),
        "unexpected error: {err}"
    );
    assert!(
        !outside.exists(),
        "write must not create the file outside the workspace"
    );
}
