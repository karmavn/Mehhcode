# Dump 05 - Tools va thuc thi

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-tools/src/lib.rs | 24 | 709 | 133b4a1d4a0e35fc4b0a78959e8c91078f11fa743d13d2e098b3b65e16b72717 |
| crates/mehh-tools/src/edit.rs | 24.5 | 719 | cbfe3912b512acce59154bab0244de41863cd37a96efd764eb85a313cf2c48af |
| crates/mehh-tools/src/write.rs | 4.9 | 153 | 5b45c31998ee9a310eae2bb2e7df3a3a4ff4d661cc0521e427bbd2cfb9bdd2bd |
| crates/mehh-tools/src/undo_edit.rs | 9.5 | 287 | 1260a191e74e8a8ac8c304df05fb1f51be9d52c69f04d2650c99a5125747bd5b |
| crates/mehh-tools/src/read.rs | 9.1 | 267 | a839604e2bdc2b14cc1393bc68ab8bcf0eb2389081c29f9b79caa001714506d5 |
| crates/mehh-tools/src/bash.rs | 5.7 | 180 | 7694bf634345c7aa50859c86c26cc7bc8c5da7665b9d901b9ab139c05fa0cf8a |
| crates/mehh-tools/src/core/shell.rs | 13.3 | 381 | 1807b4ed48f5a065883d2f9e757587fdb7cad7292f0d0971c3c40d6e2c9c6444 |
| crates/mehh-tools/src/analyze.rs | 22 | 658 | 132d140cfb7b953537db9620b31abb99759b85327a122dc1998f96305d860578 |

## crates/mehh-tools/src/lib.rs

`````rust
pub mod analyze;
pub mod bash;
pub mod core;
pub mod edit;
pub mod file_mutation_queue;
pub mod find;
pub mod grep;
pub mod ls;
pub mod powershell;
pub mod read;
pub mod undo_edit;
pub mod write;

pub use analyze::AnalyzeTool;
pub use bash::BashTool;
pub use edit::EditTool;
pub use file_mutation_queue::with_file_mutation_queue;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use powershell::PowerShellTool;
pub use read::ReadTool;
pub use undo_edit::{EditUndoStore, FileSnapshot, UndoEditTool};
pub use write::WriteTool;

use std::path::PathBuf;
use std::sync::Arc;
use mehh_agent::AgentTool;

pub fn create_coding_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    let undo_store = Arc::new(EditUndoStore::new());
    vec![
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone()).with_undo_store(undo_store.clone())),
        Arc::new(EditTool::new(cwd.clone()).with_undo_store(undo_store.clone())),
        Arc::new(UndoEditTool::new(cwd.clone(), undo_store)),
        Arc::new(BashTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd.clone())),
        Arc::new(AnalyzeTool::new(cwd)),
    ]
}

pub fn create_all_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    let undo_store = Arc::new(EditUndoStore::new());
    vec![
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone()).with_undo_store(undo_store.clone())),
        Arc::new(EditTool::new(cwd.clone()).with_undo_store(undo_store.clone())),
        Arc::new(UndoEditTool::new(cwd.clone(), undo_store)),
        Arc::new(BashTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd.clone())),
        Arc::new(PowerShellTool::new(cwd.clone())),
        Arc::new(AnalyzeTool::new(cwd)),
    ]
}

const ALL_TOOL_NAMES: &[&str] = &[
    "read", "write", "edit", "undo_edit", "bash", "grep", "find", "ls", "analyze", "powershell",
];

/// Build only the requested tools and reject unknown names instead of silently
/// ignoring typos. PowerShell is opt-in when no explicit include list is given.
pub fn select_coding_tools(
    cwd: PathBuf,
    include: Option<&[String]>,
    exclude: &[String],
) -> anyhow::Result<Vec<Arc<dyn AgentTool>>> {
    for name in include.into_iter().flatten().chain(exclude.iter()) {
        if !ALL_TOOL_NAMES.contains(&name.as_str()) {
            anyhow::bail!(
                "Unknown tool name '{}'. Available tools: {}",
                name,
                ALL_TOOL_NAMES.join(", ")
            );
        }
    }

    let mut requested: std::collections::HashSet<String> = match include {
        Some(names) => names.iter().cloned().collect(),
        None => ALL_TOOL_NAMES
            .iter()
            .filter(|name| **name != "powershell")
            .map(|name| (*name).to_string())
            .collect(),
    };
    for name in exclude {
        requested.remove(name);
    }

    let undo_store = Arc::new(EditUndoStore::new());
    let mut selected: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(requested.len());
    let wants = |name: &str| requested.contains(name);

    if wants("read") {
        selected.push(Arc::new(ReadTool::new(cwd.clone())));
    }
    if wants("write") {
        selected.push(Arc::new(
            WriteTool::new(cwd.clone()).with_undo_store(undo_store.clone()),
        ));
    }
    if wants("edit") {
        selected.push(Arc::new(
            EditTool::new(cwd.clone()).with_undo_store(undo_store.clone()),
        ));
    }
    if wants("undo_edit") {
        selected.push(Arc::new(UndoEditTool::new(cwd.clone(), undo_store)));
    }
    if wants("bash") {
        selected.push(Arc::new(BashTool::new(cwd.clone())));
    }
    if wants("grep") {
        selected.push(Arc::new(GrepTool::new(cwd.clone())));
    }
    if wants("find") {
        selected.push(Arc::new(FindTool::new(cwd.clone())));
    }
    if wants("ls") {
        selected.push(Arc::new(LsTool::new(cwd.clone())));
    }
    if wants("powershell") {
        selected.push(Arc::new(PowerShellTool::new(cwd.clone())));
    }
    if wants("analyze") {
        selected.push(Arc::new(AnalyzeTool::new(cwd)));
    }

    // Keep a stable order regardless of include ordering.
    selected.sort_by_key(|tool| {
        ALL_TOOL_NAMES
            .iter()
            .position(|name| *name == tool.name())
            .unwrap_or(usize::MAX)
    });
    Ok(selected)
}

#[cfg(test)]
pub(crate) struct TempTestDir(PathBuf);

#[cfg(test)]
impl TempTestDir {
    pub(crate) fn new(prefix: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mehh_tools_test_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::prelude::*;
    use mehh_ai::ToolResultContentBlock;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_write_and_read_tool() {
        let temp_dir = TempTestDir::new("write_read");
        let write_tool = WriteTool::new(temp_dir.path().to_path_buf());
        let read_tool = ReadTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        // 1. Write file with parent directories
        let write_params = json!({
            "path": "nested/sub/sample.txt",
            "content": "line 1\nline 2\nline 3\nline 4"
        })
        .as_object()
        .unwrap()
        .clone();

        let write_res = write_tool
            .execute("tc1", write_params, cancel.clone(), None)
            .await
            .unwrap();
        let write_text = match &write_res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text content"),
        };
        assert!(write_text.contains("Successfully wrote"));

        // 2. Read full file
        let read_params = json!({
            "path": "nested/sub/sample.txt"
        })
        .as_object()
        .unwrap()
        .clone();

        let read_res = read_tool
            .execute("tc2", read_params, cancel.clone(), None)
            .await
            .unwrap();
        let read_text = match &read_res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text content"),
        };
        assert_eq!(read_text, "line 1\nline 2\nline 3\nline 4");

        // 3. Read with offset and limit
        let offset_params = json!({
            "path": "nested/sub/sample.txt",
            "offset": 2,
            "limit": 2
        })
        .as_object()
        .unwrap()
        .clone();

        let offset_res = read_tool
            .execute("tc3", offset_params, cancel.clone(), None)
            .await
            .unwrap();
        let offset_text = match &offset_res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text content"),
        };
        assert!(offset_text.contains("line 2\nline 3"));
        assert!(offset_text.contains("[Showing lines 2-3 of 4. Use offset=4 to read more.]"));

        // 4. Test image reading
        let dummy_png_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let image_path = temp_dir.path().join("photo.png");
        std::fs::write(&image_path, &dummy_png_bytes).unwrap();

        let img_params = json!({
            "path": "photo.png"
        })
        .as_object()
        .unwrap()
        .clone();

        let img_res = read_tool
            .execute("tc4", img_params, cancel.clone(), None)
            .await
            .unwrap();
        match &img_res.content[0] {
            ToolResultContentBlock::Image(img) => {
                assert_eq!(img.mime_type, "image/png");
                assert_eq!(img.data, BASE64_STANDARD.encode(&dummy_png_bytes));
            }
            _ => panic!("Expected image content block"),
        }
    }

    #[tokio::test]
    async fn test_edit_tool_and_crlf_preservation() {
        let temp_dir = TempTestDir::new("edit");
        let edit_tool = EditTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        // 1. Test CRLF preservation with single edit
        let file_path = temp_dir.path().join("crlf_test.txt");
        let crlf_content = "first line\r\nsecond line\r\nthird line\r\n";
        std::fs::write(&file_path, crlf_content).unwrap();

        let edit_params = json!({
            "path": "crlf_test.txt",
            "oldText": "second line",
            "newText": "replaced line"
        })
        .as_object()
        .unwrap()
        .clone();

        let res = edit_tool
            .execute("tc1", edit_params, cancel.clone(), None)
            .await
            .unwrap();
        let res_text = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(res_text.contains("Successfully edited"));
        assert!(res.details.is_some());

        let modified_bytes = std::fs::read(&file_path).unwrap();
        let modified_str = String::from_utf8(modified_bytes).unwrap();
        assert_eq!(
            modified_str,
            "first line\r\nreplaced line\r\nthird line\r\n"
        );
        assert!(modified_str.contains("\r\n"));

        // 2. Error on missing oldText
        let missing_params = json!({
            "path": "crlf_test.txt",
            "oldText": "not present in file",
            "newText": "something"
        })
        .as_object()
        .unwrap()
        .clone();

        let err_missing = edit_tool
            .execute("tc2", missing_params, cancel.clone(), None)
            .await;
        assert!(err_missing.is_err());
        assert!(err_missing.unwrap_err().contains("Could not find edits[0]"));

        // 3. Error on ambiguous (multiple) occurrences
        let dup_file = temp_dir.path().join("dup_test.txt");
        std::fs::write(&dup_file, "same\nsame\n").unwrap();

        let dup_params = json!({
            "path": "dup_test.txt",
            "oldText": "same",
            "newText": "diff"
        })
        .as_object()
        .unwrap()
        .clone();

        let err_dup = edit_tool
            .execute("tc3", dup_params, cancel.clone(), None)
            .await;
        assert!(err_dup.is_err());
        assert!(err_dup.unwrap_err().contains("Found 2 occurrences"));

        // 4. Multiple edits
        let multi_file = temp_dir.path().join("multi_test.txt");
        std::fs::write(&multi_file, "alpha\nbeta\ngamma\n").unwrap();

        let multi_params = json!({
            "path": "multi_test.txt",
            "edits": [
                { "oldText": "alpha", "newText": "AAA" },
                { "oldText": "gamma", "newText": "CCC" }
            ]
        })
        .as_object()
        .unwrap()
        .clone();

        let multi_res = edit_tool
            .execute("tc4", multi_params, cancel.clone(), None)
            .await
            .unwrap();
        assert!(match &multi_res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.contains("Successfully edited"),
            _ => false,
        });

        let multi_str = std::fs::read_to_string(&multi_file).unwrap();
        assert_eq!(multi_str, "AAA\nbeta\nCCC\n");
    }

    #[tokio::test]
    async fn test_grep_tool() {
        let temp_dir = TempTestDir::new("grep");
        let grep_tool = GrepTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        std::fs::write(temp_dir.path().join("a.txt"), "hello world\nsecond line").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), "HELLO WORLD\nthird line").unwrap();
        std::fs::write(temp_dir.path().join("c.rs"), "fn hello() {}").unwrap();

        // 1. Literal and case-sensitive
        let params1 = json!({
            "pattern": "hello",
            "literal": true,
            "ignoreCase": false
        })
        .as_object()
        .unwrap()
        .clone();

        let res1 = grep_tool
            .execute("tc1", params1, cancel.clone(), None)
            .await
            .unwrap();
        let text1 = match &res1.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(text1.contains("a.txt:1:hello world"));
        assert!(text1.contains("c.rs:1:fn hello() {}"));
        assert!(!text1.contains("b.txt"));

        // 2. Case-insensitive and glob filtered
        let params2 = json!({
            "pattern": "hello",
            "glob": "*.rs",
            "ignoreCase": true
        })
        .as_object()
        .unwrap()
        .clone();

        let res2 = grep_tool
            .execute("tc2", params2, cancel.clone(), None)
            .await
            .unwrap();
        let text2 = match &res2.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(text2.contains("c.rs:1:fn hello() {}"));
        assert!(!text2.contains("a.txt"));
    }

    #[tokio::test]
    async fn test_find_tool() {
        let temp_dir = TempTestDir::new("find");
        let find_tool = FindTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(temp_dir.path().join("src/main.rs"), "// main").unwrap();
        std::fs::write(temp_dir.path().join("README.md"), "# Readme").unwrap();

        // 1. Glob match
        let params1 = json!({
            "pattern": "*.rs"
        })
        .as_object()
        .unwrap()
        .clone();

        let res1 = find_tool
            .execute("tc1", params1, cancel.clone(), None)
            .await
            .unwrap();
        let text1 = match &res1.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(text1.contains("src/lib.rs"));
        assert!(text1.contains("src/main.rs"));
        assert!(!text1.contains("README.md"));

        // 2. Substring fallback match
        let params2 = json!({
            "pattern": "readme"
        })
        .as_object()
        .unwrap()
        .clone();

        let res2 = find_tool
            .execute("tc2", params2, cancel.clone(), None)
            .await
            .unwrap();
        let text2 = match &res2.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(text2.contains("README.md"));
    }

    #[tokio::test]
    async fn test_ls_tool() {
        let temp_dir = TempTestDir::new("ls");
        let ls_tool = LsTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        std::fs::create_dir_all(temp_dir.path().join("dir_b")).unwrap();
        std::fs::create_dir_all(temp_dir.path().join("dir_a")).unwrap();
        std::fs::write(temp_dir.path().join("file_z.txt"), "z").unwrap();
        std::fs::write(temp_dir.path().join("file_a.txt"), "a").unwrap();

        let params = json!({}).as_object().unwrap().clone();
        let res = ls_tool
            .execute("tc1", params, cancel.clone(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["dir_a/", "dir_b/", "file_a.txt", "file_z.txt"]);
    }

    #[tokio::test]
    async fn test_bash_tool() {
        let temp_dir = TempTestDir::new("bash");
        let bash_tool = BashTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        // 1. Success command
        let params1 = json!({
            "command": "echo test_bash_12345"
        })
        .as_object()
        .unwrap()
        .clone();

        let res1 = bash_tool
            .execute("tc1", params1, cancel.clone(), None)
            .await
            .unwrap();
        let text1 = match &res1.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };
        assert!(text1.contains("test_bash_12345"));

        // 2. Non-zero exit code error
        let params2 = json!({
            "command": "exit 42"
        })
        .as_object()
        .unwrap()
        .clone();

        let err2 = bash_tool
            .execute("tc2", params2, cancel.clone(), None)
            .await;
        assert!(err2.is_err());
        assert!(err2.unwrap_err().contains("42"));
    }

    #[test]
    fn test_create_coding_tools() {
        let tools = create_coding_tools(PathBuf::from("."));
        assert_eq!(tools.len(), 9);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "read", "write", "edit", "undo_edit", "bash", "grep", "find", "ls", "analyze"
            ]
        );
    }

    #[test]
    fn create_coding_tools_excludes_powershell_while_create_all_tools_includes_it() {
        let default_tools = create_coding_tools(PathBuf::from("."));
        assert_eq!(default_tools.len(), 9);
        assert!(!default_tools.iter().any(|t| t.name() == "powershell"));

        let all_tools = create_all_tools(PathBuf::from("."));
        assert_eq!(all_tools.len(), 10);
        assert!(all_tools.iter().any(|t| t.name() == "powershell"));

        // select_coding_tools with include
        let selected = select_coding_tools(
            PathBuf::from("."),
            Some(&["powershell".to_string(), "bash".to_string()]),
            &[],
        )
        .unwrap();
        assert_eq!(selected.len(), 2);

        // select_coding_tools with exclude
        let without_bash = select_coding_tools(
            PathBuf::from("."),
            None,
            &["bash".to_string()],
        )
        .unwrap();
        assert_eq!(without_bash.len(), 8);
        assert!(!without_bash.iter().any(|t| t.name() == "bash"));
    }

    #[tokio::test]
    async fn test_read_tool_bom_and_trailing_newline_line_count() {
        let temp_dir = TempTestDir::new("read_bom_nl");
        let file_path = temp_dir.path().join("sample_bom.txt");
        let bom_content = "\u{feff}first line\nsecond line\n";
        std::fs::write(&file_path, bom_content.as_bytes()).unwrap();

        let read_tool = ReadTool::new(temp_dir.path().to_path_buf());
        let params = json!({
            "path": "sample_bom.txt",
            "offset": 1,
            "limit": 10
        })
        .as_object()
        .unwrap()
        .clone();

        let res = read_tool
            .execute("tc_bom", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("Expected text"),
        };

        // File has 2 lines; trailing newline must not inflate line count to 3
        assert!(!text.contains("3 lines total"), "Trailing newline caused off-by-one line count: {text}");
        assert!(!text.contains("[Showing lines 1-2 of 3"), "Trailing newline caused off-by-one pagination hint: {text}");
        assert!(!text.starts_with('\u{feff}'), "BOM was not stripped from read tool output");
    }

    #[tokio::test]
    async fn test_edit_tool_preserves_bom_and_matches_first_line() {
        let temp_dir = TempTestDir::new("edit_bom");
        let file_path = temp_dir.path().join("bom_code.rs");
        // Trailing whitespace requires tier-2 fuzzy matching on a file with BOM
        let bom_code = "\u{feff}fn main() {   \n    let x = 1;  \n}\n";
        std::fs::write(&file_path, bom_code.as_bytes()).unwrap();

        let edit_tool = EditTool::new(temp_dir.path().to_path_buf());
        let params = json!({
            "path": "bom_code.rs",
            "oldText": "fn main() {\n    let x = 1;\n}",
            "newText": "fn main() {\n    let x = 42;\n}"
        })
        .as_object()
        .unwrap()
        .clone();

        let res = edit_tool
            .execute("tc_edit_bom", params, CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.contains("Successfully edited"),
            _ => false,
        });

        let updated_bytes = std::fs::read(&file_path).unwrap();
        let updated_str = String::from_utf8(updated_bytes).unwrap();
        assert!(updated_str.starts_with('\u{feff}'), "BOM must be preserved after edit");
        assert!(updated_str.contains("let x = 42;"));
    }

    #[tokio::test]
    async fn test_edit_tool_preserves_mixed_line_endings_without_wholesale_rewrite() {
        let temp_dir = TempTestDir::new("edit_mixed");
        let file_path = temp_dir.path().join("mixed.rs");
        // File is LF throughout, but contains an actual CRLF in one place
        let initial = "fn test() {\n    let crlf = \"\r\n\";\n    let y = 1;\n}\n";
        std::fs::write(&file_path, initial.as_bytes()).unwrap();

        let edit_tool = EditTool::new(temp_dir.path().to_path_buf());
        let params = json!({
            "path": "mixed.rs",
            "oldText": "let y = 1;",
            "newText": "let y = 2;"
        })
        .as_object()
        .unwrap()
        .clone();

        edit_tool
            .execute("tc_mixed", params, CancellationToken::new(), None)
            .await
            .unwrap();

        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "fn test() {\n    let crlf = \"\r\n\";\n    let y = 2;\n}\n");
    }

    #[tokio::test]
    async fn test_write_tool_honors_cancellation() {
        let temp_dir = TempTestDir::new("write_cancel");
        let write_tool = WriteTool::new(temp_dir.path().to_path_buf());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let params = json!({
            "path": "cancelled.txt",
            "content": "should not be written"
        })
        .as_object()
        .unwrap()
        .clone();

        let res = write_tool.execute("tc_cancel", params, cancel, None).await;
        assert!(res.is_err(), "WriteTool must fail when cancelled");
        assert!(!temp_dir.path().join("cancelled.txt").exists(), "Cancelled write must not create file");
    }

    #[test]
    fn test_mutation_queue_key_resolves_dot_dot_before_file_creation() {
        let temp_dir = TempTestDir::new("key_dotdot");
        let direct = temp_dir.path().join("target.txt");
        let via_dotdot = temp_dir.path().join("subdir").join("..").join("target.txt");

        let key_direct = file_mutation_queue::get_mutation_queue_key(&direct);
        let key_dotdot = file_mutation_queue::get_mutation_queue_key(&via_dotdot);

        assert_eq!(
            key_direct, key_dotdot,
            "Mutation queue keys must resolve '..' components identically before file creation"
        );
    }
}
`````

## crates/mehh-tools/src/edit.rs

`````rust
use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write;
use crate::core::output::truncate_head;
use crate::undo_edit::{EditUndoStore, FileSnapshot};

const MAX_DIFF_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct EditTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Option<Arc<EditUndoStore>>,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store: None,
        }
    }

    pub fn with_undo_store(mut self, undo_store: Arc<EditUndoStore>) -> Self {
        self.undo_store = Some(undo_store);
        self
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[derive(Debug, Clone)]
struct SingleEdit {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone)]
struct MatchedEdit {
    index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

fn split_bom(s: &str) -> (&str, &str) {
    if let Some(stripped) = s.strip_prefix('\u{feff}') {
        ("\u{feff}", stripped)
    } else {
        ("", s)
    }
}

fn apply_replacements_to_original(original: &str, replacements: &[MatchedEdit]) -> String {
    let mut crlf_before = 0usize;
    let crlf_indices: Vec<usize> = original
        .match_indices("\r\n")
        .map(|(idx, _)| idx)
        .collect();
    let mut next_crlf_idx = 0usize;

    let mut to_original = |offset: usize| -> usize {
        while next_crlf_idx < crlf_indices.len() && crlf_indices[next_crlf_idx] - crlf_before < offset {
            crlf_before += 1;
            next_crlf_idx += 1;
        }
        offset + crlf_before
    };

    // Find the terminator of the line containing `offset`, ignoring newline
    // bytes inside string/char literals. A plain `find('\n')` can pick up the
    // CRLF inside `let s = "\r\n";` and wrongly rewrite new_text to CRLF.
    let get_line_ending_at = |content: &str, offset: usize| -> &'static str {
        let line_start = content[..offset].rfind('\n').map_or(0, |pos| pos + 1);
        let bytes = content.as_bytes();
        let mut i = line_start;
        let mut quote: Option<u8> = None;
        let mut escaped = false;

        while i < bytes.len() {
            let byte = bytes[i];
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match byte {
                b'"' | b'\'' => {
                    quote = Some(byte);
                    i += 1;
                }
                b'\n' => {
                    return if i > line_start && bytes[i - 1] == b'\r' {
                        "\r\n"
                    } else {
                        "\n"
                    };
                }
                _ => i += 1,
            }
        }

        // Last line has no terminator: use the file's dominant style.
        let crlf_count = content.matches("\r\n").count();
        let lf_count = content.matches('\n').count();
        if crlf_count > 0 && crlf_count == lf_count {
            "\r\n"
        } else {
            "\n"
        }
    };

    let mut parts = String::with_capacity(original.len());
    let mut position = 0usize;

    for m in replacements {
        let start = to_original(m.start);
        parts.push_str(&original[position..start]);
        let ending = get_line_ending_at(original, start);
        if ending == "\n" {
            parts.push_str(&m.new_text);
        } else {
            parts.push_str(&m.new_text.replace('\n', ending));
        }
        position = to_original(m.end);
    }
    parts.push_str(&original[position..]);
    parts
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    aliases: &[&str],
    label: &str,
) -> Result<&'a str, String> {
    aliases
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .ok_or_else(|| format!("{label} must be a string"))
}

fn parse_edits(params: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<SingleEdit>, String> {
    let mut edits = Vec::new();

    if let Some(edits_val) = params.get("edits") {
        let items: Vec<serde_json::Value> = if let Some(arr) = edits_val.as_array() {
            arr.clone()
        } else if let Some(s) = edits_val.as_str() {
            serde_json::from_str(s)
                .map_err(|e| format!("Failed to parse 'edits' JSON string: {e}"))?
        } else {
            return Err("Parameter 'edits' must be an array or a JSON string".to_string());
        };

        for (i, item) in items.iter().enumerate() {
            if !item.is_object() {
                return Err(format!("edits[{i}] must be an object"));
            }
            let old_text = required_string(
                item,
                &["oldText", "old_text", "old_string"],
                &format!("edits[{i}].oldText"),
            )?;
            let new_text = required_string(
                item,
                &["newText", "new_text", "new_string"],
                &format!("edits[{i}].newText"),
            )?;
            edits.push(SingleEdit {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            });
        }
    }

    if edits.is_empty() {
        let old = params
            .get("oldText")
            .or_else(|| params.get("old_text"))
            .or_else(|| params.get("old_string"))
            .and_then(|v| v.as_str());
        let new = params
            .get("newText")
            .or_else(|| params.get("new_text"))
            .or_else(|| params.get("new_string"))
            .and_then(|v| v.as_str());

        if let (Some(old), Some(new)) = (old, new) {
            edits.push(SingleEdit {
                old_text: old.to_string(),
                new_text: new.to_string(),
            });
        } else if old.is_some() || new.is_some() {
            return Err("Both 'oldText' and 'newText' are required for a single edit".to_string());
        }
    }

    if edits.is_empty() {
        return Err("Edit tool requires either 'edits' array or 'oldText' and 'newText' parameters.".to_string());
    }

    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("edits[{}].oldText must not be empty.", i));
        }
    }

    Ok(edits)
}

use crate::file_mutation_queue::with_file_mutation_queue;

const CLOSEST_HINT_MIN_SCORE: f64 = 0.4;

/// Normalizes CRLF line endings and strips trailing horizontal whitespace per line.
pub fn normalize_for_fuzzy_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for line in s.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_end_matches([' ', '\t']);
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(trimmed);
    }
    out
}

fn split_lines_with_offsets(s: &str) -> Vec<(usize, usize, &str)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for part in s.split('\n') {
        let end = start + part.len();
        lines.push((start, end, part));
        start = end + 1;
    }
    lines
}

fn find_fuzzy_matches(content: &str, old_norm: &str) -> Vec<(usize, usize)> {
    let had_trailing_nl = old_norm.ends_with('\n');
    let pat_body = if had_trailing_nl {
        &old_norm[..old_norm.len() - 1]
    } else {
        old_norm
    };
    let pat_lines: Vec<&str> = pat_body
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect();
    let content_lines = split_lines_with_offsets(content);

    let mut matches = Vec::new();
    if pat_lines.is_empty() {
        return matches;
    }

    if pat_lines.len() == 1 && !had_trailing_nl {
        let pat = pat_lines[0];
        if pat.is_empty() {
            return matches;
        }
        for &(l_start, l_end, l_text) in &content_lines {
            let trimmed = l_text.trim_end_matches([' ', '\t']);
            for (rel_idx, _) in trimmed.match_indices(pat) {
                let abs_start = l_start + rel_idx;
                let abs_end = if rel_idx + pat.len() == trimmed.len() {
                    // Consume trailing whitespace on that line when matching up to line end
                    l_end
                } else {
                    abs_start + pat.len()
                };
                matches.push((abs_start, abs_end));
            }
        }
        return matches;
    }

    if pat_lines.len() > content_lines.len() {
        return matches;
    }

    for r in 0..=content_lines.len() - pat_lines.len() {
        let all_match = pat_lines.iter().enumerate().all(|(k, pat_line)| {
            let trimmed = content_lines[r + k].2.trim_end_matches([' ', '\t']);
            trimmed == *pat_line
        });
        if all_match {
            let start = content_lines[r].0;
            let last_line_end = content_lines[r + pat_lines.len() - 1].1;
            let end = if had_trailing_nl && last_line_end < content.len() {
                last_line_end + 1
            } else {
                last_line_end
            };
            matches.push((start, end));
        }
    }
    matches
}

fn char_bigrams(s: &str) -> std::collections::HashSet<(char, char)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        std::collections::HashSet::new()
    } else {
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    }
}

fn line_similarity(
    a: &str,
    b: &str,
    a_bigrams: &std::collections::HashSet<(char, char)>,
) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    if b.contains(a) || a.contains(b) {
        let a_len = if a.is_ascii() { a.len() } else { a.chars().count() };
        let b_len = if b.is_ascii() { b.len() } else { b.chars().count() };
        let min_len = a_len.min(b_len) as f64;
        let max_len = a_len.max(b_len) as f64;
        return (min_len / max_len).max(0.6);
    }
    if a_bigrams.is_empty() {
        return 0.0;
    }
    let b_bigrams = char_bigrams(b);
    if b_bigrams.is_empty() {
        return 0.0;
    }
    let shared = a_bigrams.intersection(&b_bigrams).count() as f64;
    (2.0 * shared) / ((a_bigrams.len() + b_bigrams.len()) as f64)
}

fn closest_line_hint(content: &str, old_norm: &str) -> Option<String> {
    let target = old_norm
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let target_bigrams = char_bigrams(target);
    let mut best: Option<(usize, f64, &str)> = None;
    for (idx, line) in content.lines().enumerate() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        let score = line_similarity(target, candidate, &target_bigrams);
        if score >= CLOSEST_HINT_MIN_SCORE
            && best.is_none_or(|(_, best_s, _)| score > best_s)
        {
            best = Some((idx + 1, score, candidate));
        }
    }
    best.map(|(line_num, _, preview)| {
        let clipped: String = preview.chars().take(120).collect();
        format!("Closest match near line {line_num}: `{clipped}`")
    })
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text (with trailing-whitespace fuzzy fallback). Every edits[].oldText must match a unique, non-overlapping region of the file."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact text to replace (for single edit)"
                },
                "newText": {
                    "type": "string",
                    "description": "Replacement text (for single edit)"
                },
                "edits": {
                    "type": "array",
                    "description": "List of targeted replacements",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text to replace"
                            },
                            "newText": {
                                "type": "string",
                                "description": "Replacement text"
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let edits = parse_edits(&params)?;

        let full_path = self.resolve_path(path_str)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();
        with_file_mutation_queue(&full_path, async {
            // The queue await may have been waiting while the caller cancelled.
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }
            if !full_path.exists() {
                return Err(format!("File not found: {}", path_str));
            }

            let raw_bytes = tokio::fs::read(&full_path)
                .await
                .map_err(|e| format!("Failed to read {}: {}", path_str, e))?;

            let raw_content = String::from_utf8(raw_bytes.clone())
                .map_err(|e| format!("File {} is not valid UTF-8: {}", path_str, e))?;

            let (bom, content) = split_bom(&raw_content);
            let normalized_content = content.replace("\r\n", "\n");

            let mut matched_edits: Vec<MatchedEdit> = Vec::with_capacity(edits.len());

            for (i, edit) in edits.iter().enumerate() {
                let old_norm = edit.old_text.replace("\r\n", "\n");
                let new_norm = edit.new_text.replace("\r\n", "\n");

                let exact_matches: Vec<(usize, usize)> = normalized_content
                    .match_indices(&old_norm)
                    .map(|(start, _)| (start, start + old_norm.len()))
                    .collect();

                let (start, end) = if exact_matches.len() == 1 {
                    exact_matches[0]
                } else if exact_matches.len() > 1 {
                    return Err(format!(
                        "Found {} occurrences of edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
                        exact_matches.len(), i, path_str
                    ));
                } else {
                    let fuzzy_matches = find_fuzzy_matches(&normalized_content, &old_norm);
                    if fuzzy_matches.len() == 1 {
                        fuzzy_matches[0]
                    } else if fuzzy_matches.len() > 1 {
                        return Err(format!(
                            "Found {} fuzzy matches for edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
                            fuzzy_matches.len(), i, path_str
                        ));
                    } else {
                        let hint = closest_line_hint(&normalized_content, &old_norm)
                            .map(|h| format!("\n{h}"))
                            .unwrap_or_default();
                        return Err(format!(
                            "Could not find edits[{}] in {}. The oldText must match exact or trailing-whitespace-normalized text.{hint}",
                            i, path_str
                        ));
                    }
                };

                matched_edits.push(MatchedEdit {
                    index: i,
                    start,
                    end,
                    new_text: new_norm,
                });
            }

            matched_edits.sort_by_key(|m| m.start);
            for i in 1..matched_edits.len() {
                let prev = &matched_edits[i - 1];
                let curr = &matched_edits[i];
                if prev.end > curr.start {
                    return Err(format!(
                        "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
                        prev.index, curr.index, path_str
                    ));
                }
            }

            let mut new_normalized = String::with_capacity(normalized_content.len());
            let mut last_idx = 0;
            for m in &matched_edits {
                new_normalized.push_str(&normalized_content[last_idx..m.start]);
                new_normalized.push_str(&m.new_text);
                last_idx = m.end;
            }
            new_normalized.push_str(&normalized_content[last_idx..]);

            if new_normalized == normalized_content {
                return Err(format!(
                    "No changes made to {}. Replacement produced identical content.",
                    path_str
                ));
            }

            let final_body = if content == normalized_content {
                new_normalized
            } else {
                apply_replacements_to_original(content, &matched_edits)
            };
            let final_content = format!("{bom}{final_body}");

            atomic_write(&full_path, final_content.as_bytes()).await?;

            // Record undo history only after the write actually succeeded;
            // otherwise a failed write would leave a phantom snapshot.
            if let Some(ref store) = undo_store {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                store.push_snapshot(
                    &full_path,
                    FileSnapshot::Existed {
                        bytes: raw_bytes,
                        timestamp_ms,
                    },
                );
            }

            let diff = similar::TextDiff::from_lines(&raw_content, &final_content)
                .unified_diff()
                .context_radius(3)
                .header(path_str, path_str)
                .to_string();
            let diff = truncate_head(
                &diff,
                MAX_DIFF_BYTES,
                "\n... [diff truncated]",
            );

            let details = json!({
                "diff": diff,
                "path": path_str,
            });

            Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: format!("Successfully edited {}.\n\n{}", path_str, diff),
                    text_signature: None,
                })],
                details: Some(details),
                ..Default::default()
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_tier2_fuzzy_trailing_whitespace_and_crlf() {
        let dir = crate::TempTestDir::new("edit_tier2");
        let file = dir.path().join("sample.rs");
        std::fs::write(&file, "fn a() {   \r\n    let x = 1;  \r\n}\r\n").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        let params = json!({
            "path": "sample.rs",
            "oldText": "fn a() {\n    let x = 1;\n}",
            "newText": "fn a() {\n    let x = 2;\n}"
        })
        .as_object()
        .unwrap()
        .clone();

        tool.execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let updated = std::fs::read_to_string(&file).unwrap();
        assert_eq!(updated, "fn a() {\r\n    let x = 2;\r\n}\r\n");
    }

    #[tokio::test]
    async fn test_edit_rejects_ambiguous_fuzzy_match_and_provides_hint_on_miss() {
        let dir = crate::TempTestDir::new("edit_ambig");
        let file = dir.path().join("dup.txt");
        std::fs::write(&file, "let value = 1;  \nlet value = 1;\n").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        let dup_params = json!({
            "path": "dup.txt",
            "oldText": "let value = 1;",
            "newText": "let value = 2;"
        })
        .as_object()
        .unwrap()
        .clone();
        let err_dup = tool
            .execute("tc1", dup_params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err_dup.contains("2"));

        std::fs::write(&file, "fn compute_total(items: &[i32]) -> i32 {\n    0\n}\n").unwrap();
        let miss_params = json!({
            "path": "dup.txt",
            "oldText": "fn compute_total(items: &[i64]) -> i64 {",
            "newText": "fn compute_total() {"
        })
        .as_object()
        .unwrap()
        .clone();
        let err_miss = tool
            .execute("tc2", miss_params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err_miss.contains("Closest match near line 1"));
    }

    #[tokio::test]
    async fn concurrent_edits_on_same_file_do_not_clobber() {
        let temp_dir = crate::TempTestDir::new("concurrent_edits");
        let file = temp_dir.path().join("concurrent.txt");
        std::fs::write(&file, "SECTION_A_START\nalpha\nSECTION_A_END\nSECTION_B_START\nbeta\nSECTION_B_END\n").unwrap();

        let tool = EditTool::new(temp_dir.path().to_path_buf());
        let tool_a = tool.clone();
        let tool_b = tool.clone();

        let task_a = tokio::spawn(async move {
            let params = serde_json::json!({
                "path": "concurrent.txt",
                "oldText": "alpha",
                "newText": "ALPHA_EDITED"
            })
            .as_object()
            .unwrap()
            .clone();
            tool_a.execute("tc_a", params, CancellationToken::new(), None).await
        });

        let task_b = tokio::spawn(async move {
            let params = serde_json::json!({
                "path": "concurrent.txt",
                "oldText": "beta",
                "newText": "BETA_EDITED"
            })
            .as_object()
            .unwrap()
            .clone();
            tool_b.execute("tc_b", params, CancellationToken::new(), None).await
        });

        let (res_a, res_b) = tokio::join!(task_a, task_b);
        assert!(res_a.unwrap().is_ok());
        assert!(res_b.unwrap().is_ok());

        let final_content = std::fs::read_to_string(&file).unwrap();
        assert!(final_content.contains("ALPHA_EDITED"));
        assert!(final_content.contains("BETA_EDITED"));
    }
}
`````

## crates/mehh-tools/src/write.rs

`````rust
use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write;
use crate::file_mutation_queue::with_file_mutation_queue;
use crate::undo_edit::{EditUndoStore, FileSnapshot};

#[derive(Debug, Clone)]
pub struct WriteTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Option<Arc<EditUndoStore>>,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store: None,
        }
    }

    pub fn with_undo_store(mut self, undo_store: Arc<EditUndoStore>) -> Self {
        self.undo_store = Some(undo_store);
        self
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'content'".to_string())?;

        let full_path = self.resolve_path(path_str)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();

        with_file_mutation_queue(&full_path, async {
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("Failed to create directories for {}: {}", path_str, e))?;
            }

            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            // Snapshot must be captured before the replacement, but only
            // recorded after the atomic write succeeds.
            let snapshot = if undo_store.is_some() {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if full_path.exists() {
                    tokio::fs::read(&full_path)
                        .await
                        .ok()
                        .map(|bytes| FileSnapshot::Existed { bytes, timestamp_ms })
                } else {
                    Some(FileSnapshot::DidNotExist)
                }
            } else {
                None
            };

            atomic_write(&full_path, content.as_bytes()).await?;

            if let (Some(store), Some(snapshot)) = (undo_store.as_ref(), snapshot) {
                store.push_snapshot(&full_path, snapshot);
            }

            let byte_count = content.len();
            let line_count = content.lines().count();

            Ok(AgentToolResult::text(format!(
                "Successfully wrote {} bytes ({} lines) to {}",
                byte_count, line_count, path_str
            )))
        })
        .await
    }
}
`````

## crates/mehh-tools/src/undo_edit.rs

`````rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write;
use crate::core::output::truncate_head;
use crate::file_mutation_queue::{get_mutation_queue_key, with_file_mutation_queue};

const MAX_DIFF_BYTES: usize = 64 * 1024;
const MAX_SNAPSHOTS_PER_FILE: usize = 20;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL_UNDO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum FileSnapshot {
    Existed {
        bytes: Vec<u8>,
        timestamp_ms: i64,
    },
    DidNotExist,
}

#[derive(Debug, Clone)]
struct SnapshotEntry {
    snapshot: FileSnapshot,
    size: usize,
}

#[derive(Debug, Default)]
struct EditUndoStoreInner {
    snapshots: HashMap<String, Vec<SnapshotEntry>>,
    total_bytes: usize,
    order: Vec<String>,
}

#[derive(Debug, Default)]
pub struct EditUndoStore {
    inner: Mutex<EditUndoStoreInner>,
}

impl EditUndoStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(EditUndoStoreInner::default()),
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, EditUndoStoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_snapshot(&self, path: &Path, snapshot: FileSnapshot) {
        let size = match &snapshot {
            FileSnapshot::Existed { bytes, .. } => {
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return;
                }
                bytes.len()
            }
            FileSnapshot::DidNotExist => 0,
        };

        let key = get_mutation_queue_key(path);
        let mut inner = self.lock_inner();

        while inner.total_bytes + size > MAX_TOTAL_UNDO_BYTES && !inner.order.is_empty() {
            let oldest_key = inner.order.remove(0);
            if let Some(history) = inner.snapshots.get_mut(&oldest_key) {
                if !history.is_empty() {
                    let evicted = history.remove(0);
                    inner.total_bytes = inner.total_bytes.saturating_sub(evicted.size);
                }
            }
            inner.snapshots.retain(|_, v| !v.is_empty());
        }

        let history = inner.snapshots.entry(key.clone()).or_default();
        let evicted_size = if history.len() >= MAX_SNAPSHOTS_PER_FILE {
            Some(history.remove(0).size)
        } else {
            None
        };
        history.push(SnapshotEntry { snapshot, size });

        if let Some(es) = evicted_size {
            inner.total_bytes = inner.total_bytes.saturating_sub(es);
        }
        inner.total_bytes += size;
        inner.order.push(key);
    }

    /// Read the most recent snapshot without consuming it. Callers that write
    /// first and pop afterwards keep the undo history if the write fails.
    pub fn peek_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        let inner = self.lock_inner();
        inner
            .snapshots
            .get(&key)
            .and_then(|history| history.last())
            .map(|entry| entry.snapshot.clone())
    }

    pub fn pop_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        let mut inner = self.lock_inner();
        let (entry, is_empty) = {
            if let Some(history) = inner.snapshots.get_mut(&key) {
                let popped = history.pop();
                let empty = history.is_empty();
                (popped, empty)
            } else {
                (None, false)
            }
        };

        if let Some(entry) = entry {
            inner.total_bytes = inner.total_bytes.saturating_sub(entry.size);
            if let Some(pos) = inner.order.iter().rposition(|k| k == &key) {
                inner.order.remove(pos);
            }
            if is_empty {
                inner.snapshots.remove(&key);
            }
            Some(entry.snapshot)
        } else {
            None
        }
    }

    pub fn has_snapshot(&self, path: &Path) -> bool {
        let key = get_mutation_queue_key(path);
        let inner = self.lock_inner();
        inner.snapshots.get(&key).is_some_and(|h| !h.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct UndoEditTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Arc<EditUndoStore>,
}

impl UndoEditTool {
    pub fn new(cwd: PathBuf, undo_store: Arc<EditUndoStore>) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[async_trait]
impl AgentTool for UndoEditTool {
    fn name(&self) -> &str {
        "undo_edit"
    }

    fn description(&self) -> &str {
        "Revert the most recent edit or write applied to a file, restoring its previous byte-accurate content (preserving exact line endings and BOM)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to revert (relative or absolute)"
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let full_path = self.resolve_path(path_str)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();

        with_file_mutation_queue(&full_path, async {
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            let snapshot = undo_store
                .peek_snapshot(&full_path)
                .ok_or_else(|| format!("No previous edit history found for {}", path_str))?;

            match snapshot {
                FileSnapshot::DidNotExist => {
                    if full_path.exists() {
                        tokio::fs::remove_file(&full_path)
                            .await
                            .map_err(|e| format!("Failed to remove {}: {}", path_str, e))?;
                    }
                    undo_store.pop_snapshot(&full_path);
                    Ok(AgentToolResult::text(format!(
                        "Reverted creation of {}: file deleted.",
                        path_str
                    )))
                }
                FileSnapshot::Existed { bytes: original_bytes, .. } => {
                    let current_str = if full_path.exists() {
                        tokio::fs::read(&full_path)
                            .await
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok())
                    } else {
                        None
                    };

                    atomic_write(&full_path, &original_bytes).await?;
                    undo_store.pop_snapshot(&full_path);

                    let restored_str = String::from_utf8_lossy(&original_bytes);
                    let diff = if let Some(ref curr) = current_str {
                        let raw_diff =
                            similar::TextDiff::from_lines(curr.as_str(), restored_str.as_ref())
                                .unified_diff()
                                .context_radius(3)
                                .header(path_str, path_str)
                                .to_string();
                        truncate_head(&raw_diff, MAX_DIFF_BYTES, "\n... [diff truncated]")
                    } else {
                        format!("Restored {} bytes", original_bytes.len())
                    };

                    let details = json!({
                        "path": path_str,
                        "diff": diff,
                    });

                    Ok(AgentToolResult {
                        content: vec![ToolResultContentBlock::Text(TextContent {
                            text: format!("Successfully reverted {}.\n\n{}", path_str, diff),
                            text_signature: None,
                        })],
                        details: Some(details),
                        ..Default::default()
                    })
                }
            }
        })
        .await
    }
}
`````

## crates/mehh-tools/src/read.rs

`````rust
use std::path::PathBuf;

use async_trait::async_trait;
use base64::prelude::*;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, ToolResultContentBlock};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};

const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TEXT_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_READ_LINES: usize = 2000;
const MAX_READ_OUTPUT_BYTES: usize = 100 * 1024;
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct ReadTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file contents. Supports text files and images (png, jpg, jpeg, gif, webp). Text output is bounded by line count and total bytes; binary files are rejected."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (max 2000)"
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let offset = params
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(MAX_READ_LINES as i64)
            .clamp(1, MAX_READ_LINES as i64) as usize;

        let full_path = self.resolve_path(path_str)?;

        let meta = match tokio::fs::metadata(&full_path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("File not found: {}", path_str));
            }
            Err(e) => return Err(format!("Failed to stat file {}: {}", path_str, e)),
        };
        if meta.is_dir() {
            return Err(format!("Path is a directory: {}", path_str));
        }
        if meta.len() == 0 {
            return Ok(AgentToolResult::text(""));
        }

        let ext = full_path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let image_mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };

        if let Some(mime_type) = image_mime {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                return Err(format!(
                    "Image file {} is too large ({} bytes exceeds {} byte limit)",
                    path_str,
                    meta.len(),
                    MAX_IMAGE_FILE_BYTES
                ));
            }
            let bytes = tokio::fs::read(&full_path)
                .await
                .map_err(|e| format!("Failed to read image file {}: {}", path_str, e))?;
            return Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Image(ImageContent {
                    data: BASE64_STANDARD.encode(&bytes),
                    mime_type: mime_type.to_string(),
                })],
                details: None,
                ..Default::default()
            });
        }

        if meta.len() > MAX_TEXT_FILE_BYTES {
            return Err(format!(
                "File {} is too large ({} bytes exceeds {} byte limit)",
                path_str,
                meta.len(),
                MAX_TEXT_FILE_BYTES
            ));
        }

        // Sniff for NUL bytes before decoding the whole file; a 9 MB binary
        // must not be turned into a multi-million-character text block.
        let mut first = tokio::fs::File::open(&full_path)
            .await
            .map_err(|e| format!("Failed to open {}: {e}", path_str))?;
        let mut sniff = [0u8; 1024];
        let sniff_len = first
            .read(&mut sniff)
            .await
            .map_err(|e| format!("Failed to read {}: {e}", path_str))?;
        if sniff[..sniff_len].contains(&0) {
            return Err(format!("File {} appears to be binary", path_str));
        }
        drop(first);

        let file = tokio::fs::File::open(&full_path)
            .await
            .map_err(|e| format!("Failed to open {}: {e}", path_str))?;
        let mut reader = BufReader::new(file);

        let start_line = offset - 1;
        let stop_line = start_line + limit;
        let mut line_no = 0usize;
        let mut total_lines = 0usize;
        let mut shown_lines = 0usize;
        let mut output_bytes = 0usize;
        let mut output_capped = false;
        let mut output = String::new();
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            let read = reader.read_line(&mut line_buf).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    format!("File {} is not valid UTF-8 and appears to be binary", path_str)
                } else {
                    format!("Failed to read {}: {e}", path_str)
                }
            })?;
            if read == 0 {
                break;
            }
            total_lines += 1;

            if line_no >= start_line && line_no < stop_line {
                let mut line = line_buf.strip_suffix('\n').unwrap_or(&line_buf);
                line = line.strip_suffix('\r').unwrap_or(line);
                if line_no == 0 {
                    line = line.strip_prefix('\u{feff}').unwrap_or(line);
                }
                let formatted = match line.char_indices().nth(MAX_LINE_CHARS) {
                    Some((cut, _)) => format!("{}... [truncated]", &line[..cut]),
                    None => line.to_string(),
                };
                if !output_capped && output_bytes + formatted.len() + 1 > MAX_READ_OUTPUT_BYTES {
                    output_capped = true;
                } else if !output_capped {
                    if shown_lines > 0 {
                        output.push('\n');
                    }
                    output.push_str(&formatted);
                    output_bytes += formatted.len() + 1;
                    shown_lines += 1;
                }
            }

            line_no += 1;
            if line_no.is_multiple_of(1024) && cancel.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }
        }

        if start_line >= total_lines && total_lines > 0 {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                offset, total_lines
            ));
        }

        let end_line = start_line + shown_lines;
        if end_line < total_lines || start_line > 0 {
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to read more.]",
                start_line + 1,
                end_line,
                total_lines,
                end_line + 1
            ));
        }
        if output_capped {
            output.push_str(&format!(
                "\n\n[Output truncated at {} byte limit]",
                MAX_READ_OUTPUT_BYTES
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}
`````

## crates/mehh-tools/src/bash.rs

`````rust
use std::path::PathBuf;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::PathPolicy;
use mehh_sandbox::{SandboxPlan, SandboxPolicy};
use crate::core::shell::{parse_timeout_ms, run_shell, ShellKind};

#[derive(Debug, Clone)]
pub struct BashTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub sandbox_policy: SandboxPolicy,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            sandbox_policy: SandboxPolicy::WorkspaceWrite,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    pub fn with_sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_policy = policy;
        self
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the current working directory. Returns combined stdout and stderr. Output is truncated if it exceeds 30,000 characters. The timeout parameter is in milliseconds."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000; alias: timeoutMs; max: 600000)"
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "Explicit timeout in milliseconds (alias of timeout)"
                }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Command aborted".to_string());
        }

        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'command'".to_string())?;
        let timeout_ms = parse_timeout_ms(&params);
        let kind = if cfg!(windows) {
            ShellKind::WindowsPowerShellCompat
        } else {
            ShellKind::PosixSh
        };

        let plan = SandboxPlan::new(self.sandbox_policy, self.cwd.clone());
        let outcome = run_shell(
            kind,
            &self.cwd,
            command,
            timeout_ms,
            cancel,
            on_update,
            Some(&plan),
        )
        .await?;

        if let Some(reason) = outcome.aborted_reason {
            return Err(if outcome.text == "(no output)" {
                reason
            } else {
                format!("{reason}:\n{}", outcome.text)
            });
        }

        let exit_code = outcome.exit_status.as_ref().and_then(|s| s.code());
        let mut text = outcome.text;
        if mehh_sandbox::detect_violation(&text, exit_code)
            && self.sandbox_policy != SandboxPolicy::DangerFullAccess
        {
            text.push_str(
                "\n[sandbox denied] The sandbox likely blocked this command. Ask the user to approve a one-time re-run outside the sandbox if it is required.",
            );
        }

        match outcome.exit_status {
            Some(status) if !status.success() => Err(format!(
                "Command exited with code {}:\n{}",
                status.code().unwrap_or(-1),
                text
            )),
            Some(_) => Ok(AgentToolResult::text(text)),
            None => Err("Command finished without exit status".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_bash_streams_updates_and_preserves_partial_output_on_timeout() {
        let dir = crate::TempTestDir::new("bash_stream");
        let tool = BashTool::new(dir.path().to_path_buf());
        let updates: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let updates_clone = updates.clone();
        let cb: AgentToolUpdateCallback = Arc::new(move |res| {
            if let Some(mehh_ai::ToolResultContentBlock::Text(t)) = res.content.first() {
                updates_clone.lock().unwrap().push(t.text.clone());
            }
        });

        let cmd = if cfg!(windows) {
            "Write-Output 'partial_marker'; Start-Sleep -Seconds 10"
        } else {
            "echo partial_marker; sleep 10"
        };
        let params = json!({
            "command": cmd,
            "timeout": 2000
        })
        .as_object()
        .unwrap()
        .clone();

        let err = tool
            .execute("tc", params, CancellationToken::new(), Some(cb))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "unexpected err: {err}");
        assert!(err.contains("partial_marker"), "missing partial output in: {err}");
        assert!(!updates.lock().unwrap().is_empty());
    }
}
`````

## crates/mehh-tools/src/core/shell.rs

`````rust
use std::future::pending;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mehh_agent::AgentToolUpdateCallback;
use serde_json::{Map, Value};
use tokio::process::Child;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::output::RollingBytes;

pub(crate) const CHUNK_READ_SIZE: usize = 4096;
pub(crate) const MAX_STREAMING_PREVIEW_BYTES: usize = 256 * 1024;
pub(crate) const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
pub(crate) const MAX_OUTPUT_CHARS: usize = 30_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 600_000;
const PIPE_GRACE: Duration = Duration::from_millis(500);

pub(crate) const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";
pub(crate) const WINDOWS_POWERSHELL_EXIT_SUFFIX: &str =
    "\n;\nif ($?) { exit 0 }\nif (($LASTEXITCODE -eq 0) -and (\"$($Error[0].FullyQualifiedErrorId)\" -like \"NativeCommandError*\")) { exit 0 }\nif ($LASTEXITCODE) { exit $LASTEXITCODE } else { exit 1 }";

/// Timeouts are unambiguous milliseconds. `timeoutMs` is an explicit alias;
/// zero or negative values fall back to the default instead of expiring at
/// once.
pub(crate) fn parse_timeout_ms(params: &Map<String, Value>) -> u64 {
    params
        .get("timeoutMs")
        .or_else(|| params.get("timeout"))
        .and_then(|v| v.as_i64())
        .map(|t| {
            if t <= 0 {
                DEFAULT_TIMEOUT_MS
            } else {
                (t as u64).min(MAX_TIMEOUT_MS)
            }
        })
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

pub(crate) fn format_bash_output(bytes: &[u8]) -> String {
    let combined = String::from_utf8_lossy(bytes);
    let char_count = combined.chars().count();
    if char_count > MAX_OUTPUT_CHARS {
        let half = MAX_OUTPUT_CHARS / 2;
        let head_end = combined
            .char_indices()
            .nth(half)
            .map(|(i, _)| i)
            .unwrap_or(combined.len());
        let tail_start = combined
            .char_indices()
            .rev()
            .nth(half - 1)
            .map(|(i, _)| i)
            .unwrap_or(0);
        return format!(
            "{}\n\n... [truncated {} characters] ...\n\n{}",
            &combined[..head_end],
            char_count - MAX_OUTPUT_CHARS,
            &combined[tail_start..]
        );
    }
    if combined.trim().is_empty() {
        "(no output)".to_string()
    } else {
        combined.into_owned()
    }
}

/// `Child::id()` becomes `None` after `wait()` completes, so callers that kill
/// after the shell has exited must pass the PID captured at spawn time.
pub(crate) async fn kill_process_tree(child: &mut Child, pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let taskkill_bin = std::env::var("SystemRoot")
            .map(|r| format!("{r}\\System32\\taskkill.exe"))
            .unwrap_or_else(|_| "taskkill".to_string());
        let _ = tokio::process::Command::new(taskkill_bin)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    // The shell was spawned in its own process group, so a negative PID kills
    // background children too instead of leaving them holding the output pipe.
    #[cfg(unix)]
    if let Some(pid) = pid {
        let script = format!("kill -KILL -{pid}");
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::process::Command::new("sh")
                .args(["-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        })
        .await;
    }

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShellKind {
    PosixSh,
    /// PowerShell with the resolved System32 binary (PowerShellTool).
    WindowsPowerShell,
    /// Plain `powershell`, used by BashTool on Windows for compatibility.
    WindowsPowerShellCompat,
}

struct CommandSpec {
    program: String,
    args: Vec<String>,
}

fn build_command_spec(kind: ShellKind, command: &str) -> CommandSpec {
    match kind {
        ShellKind::PosixSh => CommandSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command.to_string()],
        },
        ShellKind::WindowsPowerShell | ShellKind::WindowsPowerShellCompat => {
            let full_command = if !command.trim_end().ends_with('`') {
                format!(
                    "{}{}{}",
                    UTF8_OUTPUT_PREFIX, command, WINDOWS_POWERSHELL_EXIT_SUFFIX
                )
            } else {
                format!("{}{}", UTF8_OUTPUT_PREFIX, command)
            };
            let program = match kind {
                ShellKind::WindowsPowerShell => std::env::var("SystemRoot")
                    .ok()
                    .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
                    .filter(|p| Path::new(p).is_file())
                    .unwrap_or_else(|| "powershell".to_string()),
                _ => "powershell".to_string(),
            };
            CommandSpec {
                program,
                args: vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-Command".to_string(),
                    full_command,
                ],
            }
        }
    }
}

pub(crate) struct ShellOutcome {
    pub text: String,
    pub exit_status: Option<std::process::ExitStatus>,
    pub aborted_reason: Option<String>,
}

async fn wait_grace(grace: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(sleep) = grace.as_mut() {
        sleep.await;
    } else {
        pending::<()>().await;
    }
}

/// Shared subprocess engine: detached stdin, platform-correct process-tree
/// termination, UTF-8-safe bounded capture, and a short grace period for
/// background children that keep the output pipe open after the shell exits.
pub(crate) async fn run_shell(
    kind: ShellKind,
    cwd: &Path,
    command: &str,
    timeout_ms: u64,
    cancel: CancellationToken,
    on_update: Option<AgentToolUpdateCallback>,
    sandbox: Option<&mehh_sandbox::SandboxPlan>,
) -> Result<ShellOutcome, String> {
    let spec = build_command_spec(kind, command);
    let (program, args, degraded) = match sandbox {
        Some(plan) => {
            let invocation = mehh_sandbox::wrap(plan, &spec.program, &spec.args)
                .map_err(|e| format!("Sandbox setup failed: {e}"))?;
            let degraded = invocation
                .status
                .degraded_reason()
                .map(str::to_string);
            (invocation.program, invocation.args, degraded)
        }
        None => (spec.program, spec.args, None),
    };
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args);
    cmd.current_dir(cwd);
    // Build scripts and shell commands must not inherit agent credentials.
    for (key, _) in std::env::vars_os() {
        if mehh_sandbox::is_secret_env_key(&key.to_string_lossy()) {
            cmd.env_remove(&key);
        }
    }
    // Never let a child consume the agent's stdin (CLI interactive / MCP stdio).
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn command: {e}"))?;
    // Capture before any wait(): tokio clears id() once the child is reaped.
    let spawn_pid = child.id();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let out_handle = child.stdout.take().map(|mut pipe| {
        let tx_out = tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; CHUNK_READ_SIZE];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if tx_out.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        })
    });
    let err_handle = child.stderr.take().map(|mut pipe| {
        let tx_err = tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; CHUNK_READ_SIZE];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if tx_err.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        })
    });
    drop(tx);

    let mut captured = RollingBytes::new(MAX_STREAMING_PREVIEW_BYTES);
    let mut last_update = Instant::now()
        .checked_sub(UPDATE_THROTTLE)
        .unwrap_or_else(Instant::now);
    let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(deadline);

    let mut exit_status = None;
    let mut aborted_reason = None;
    let mut rx_closed = false;
    let mut grace: Option<Pin<Box<Sleep>>> = None;
    let mut grace_expired = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled(), if aborted_reason.is_none() => {
                kill_process_tree(&mut child, spawn_pid).await;
                aborted_reason = Some("Command aborted".to_string());
                break;
            }
            _ = &mut deadline, if aborted_reason.is_none() && exit_status.is_none() => {
                kill_process_tree(&mut child, spawn_pid).await;
                aborted_reason = Some(format!("Command timed out after {timeout_ms}ms"));
                break;
            }
            maybe_chunk = rx.recv(), if !rx_closed => {
                match maybe_chunk {
                    Some(chunk) => {
                        captured.extend(&chunk);
                        if let Some(cb) = on_update.as_ref() {
                            if last_update.elapsed() >= UPDATE_THROTTLE {
                                cb(mehh_agent::AgentToolResult::text(format_bash_output(
                                    &captured.to_vec(),
                                )));
                                last_update = Instant::now();
                            }
                        }
                    }
                    None => rx_closed = true,
                }
            }
            status = child.wait(), if exit_status.is_none() => {
                match status {
                    Ok(s) => {
                        exit_status = Some(s);
                        grace = Some(Box::pin(tokio::time::sleep(PIPE_GRACE)));
                    }
                    Err(e) => return Err(format!("Failed to wait for command: {e}")),
                }
            }
            _ = wait_grace(&mut grace), if exit_status.is_some() && !rx_closed => {
                // The shell exited but a background child still holds the
                // pipe. Stop waiting and reap the whole process group.
                kill_process_tree(&mut child, spawn_pid).await;
                grace_expired = true;
                break;
            }
        }

        if rx_closed && exit_status.is_some() {
            break;
        }
    }

    let mut text = format_bash_output(&captured.to_vec());
    if let Some(reason) = degraded {
        text = format!("[sandbox degraded: {reason}]\n{text}");
    }

    if aborted_reason.is_some() || grace_expired {
        if let Some(cb) = on_update.as_ref() {
            if !captured.is_empty() {
                cb(mehh_agent::AgentToolResult::text(text.clone()));
            }
        }
        if let Some(handle) = out_handle {
            handle.abort();
        }
        if let Some(handle) = err_handle {
            handle.abort();
        }
    } else {
        if let Some(handle) = out_handle {
            let _ = handle.await;
        }
        if let Some(handle) = err_handle {
            let _ = handle.await;
        }
    }

    Ok(ShellOutcome {
        text,
        exit_status,
        aborted_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timeout_is_always_milliseconds() {
        let params = json!({ "timeout": 500 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), 500);
        let params = json!({ "timeoutMs": 2000 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), 2000);
    }

    #[test]
    fn zero_timeout_falls_back_to_default() {
        let params = json!({ "timeout": 0 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
        let params = json!({}).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
    }
}
`````

## crates/mehh-tools/src/analyze.rs

`````rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use ignore::WalkBuilder;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use regex::Regex;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::output::truncate_head;

const MAX_ANALYZE_CHARS: usize = 24_000;
const MAX_FOCUS_LINES: usize = 250;
const MAX_ANALYZE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 5_000;

static RUST_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:pub(?:\([^)]+\))?[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+(?:\x22[^\x22]+\x22[ \t]+)?)?(fn|struct|enum|trait|impl|type|const|static)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    )
    .unwrap()
});
static TS_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(function\*?|class|interface|type|const|let|var)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    )
    .unwrap()
});
static PY_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([ \t]*)(?:async[ \t]+)?(def|class)[ \t]+([A-Za-z0-9_]+)").unwrap()
});
static GO_FUNC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*func\b(?:[ \t]+\([^)]+\))?[ \t]+([A-Za-z0-9_]+)").unwrap()
});
static GO_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*type[ \t]+([A-Za-z0-9_]+)[ \t]+(struct|interface)").unwrap()
});

#[derive(Debug, Clone)]
pub struct AnalyzeTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl AnalyzeTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[derive(Debug, Clone)]
struct SymbolSpan {
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    signature: String,
}

/// Find the closing brace of a declaration without counting braces inside
/// strings, character literals or comments.
fn track_brace_end(lines: &[&str], start_idx: usize) -> usize {
    let mut depth: i32 = 0;
    let mut saw_open = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut block_comment = false;

    for (i, line) in lines.iter().enumerate().skip(start_idx) {
        let chars: Vec<char> = line.chars().collect();
        let mut k = 0usize;
        let mut line_comment = false;

        while k < chars.len() {
            let ch = chars[k];
            let next = chars.get(k + 1).copied();

            if line_comment {
                break;
            }
            if block_comment {
                if ch == '*' && next == Some('/') {
                    block_comment = false;
                    k += 2;
                } else {
                    k += 1;
                }
                continue;
            }
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
                k += 1;
                continue;
            }

            match ch {
                '/' if next == Some('/') => {
                    line_comment = true;
                    k += 2;
                }
                '/' if next == Some('*') => {
                    block_comment = true;
                    k += 2;
                }
                '"' | '\'' => {
                    quote = Some(ch);
                    k += 1;
                }
                '{' => {
                    depth += 1;
                    saw_open = true;
                    k += 1;
                }
                '}' => {
                    depth -= 1;
                    k += 1;
                    if saw_open && depth <= 0 {
                        return i + 1;
                    }
                }
                _ => k += 1,
            }
        }

        if !saw_open && line.trim().ends_with(';') {
            return i + 1;
        }
    }

    if saw_open {
        lines.len()
    } else {
        start_idx + 1
    }
}

fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn track_python_indent_end(lines: &[&str], start_idx: usize, base_indent: usize) -> usize {
    let mut end_line = start_idx + 1;
    let mut in_docstring = false;
    let mut marker = "";

    for (i, line) in lines.iter().enumerate().skip(start_idx + 1) {
        let trimmed = line.trim();
        if in_docstring {
            end_line = i + 1;
            if trimmed.contains(marker) {
                in_docstring = false;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            marker = if trimmed.starts_with("\"\"\"") {
                "\"\"\""
            } else {
                "'''"
            };
            if trimmed.matches(marker).count() < 2 {
                in_docstring = true;
            }
            end_line = i + 1;
            continue;
        }
        if indent_width(line) <= base_indent {
            return end_line;
        }
        end_line = i + 1;
    }
    end_line
}

fn extract_rust_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = RUST_SYMBOL_RE.captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(2).map_or_else(
                || {
                    line.split_whitespace()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                |m| m.as_str().to_string(),
            );
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_ts_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = TS_SYMBOL_RE.captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let trimmed = line.trim_start();
            // Local `const`/`let`/`var` are not high-level declarations.
            if matches!(kind.as_str(), "const" | "let" | "var")
                && trimmed.len() != line.len()
                && !trimmed.starts_with("export")
            {
                continue;
            }
            let name = caps.get(2).map_or_else(
                || {
                    trimmed
                        .split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                |m| m.as_str().to_string(),
            );
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_py_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = PY_SYMBOL_RE.captures(line) {
            let indent_str = caps.get(1).map_or("", |m| m.as_str());
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(3).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_python_indent_end(&lines, idx, indent_width(indent_str)),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_go_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = GO_FUNC_RE.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind: "func".to_string(),
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx),
                signature: line.trim().to_string(),
            });
        } else if let Some(caps) = GO_TYPE_RE.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_symbols_for_file(path: &Path, content: &str) -> Vec<SymbolSpan> {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => extract_rust_symbols(content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => extract_ts_symbols(content),
        "py" => extract_py_symbols(content),
        "go" => extract_go_symbols(content),
        _ => Vec::new(),
    }
}

fn analyze_structure(
    dir_path: &Path,
    max_depth: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    if !dir_path.exists() {
        return Err(format!("Directory not found: {}", dir_path.display()));
    }
    if !dir_path.is_dir() {
        return Err(format!("Path is not a directory: {}", dir_path.display()));
    }

    let mut total_files: usize = 0;
    let mut total_dirs: usize = 0;
    let mut ext_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tree_lines = Vec::new();
    let mut truncated_walk = false;

    let walker = WalkBuilder::new(dir_path)
        .max_depth(Some(max_depth))
        .standard_filters(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    let root_canon = dir_path.canonicalize().unwrap_or_else(|_| dir_path.to_path_buf());

    for result in walker {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = entry.path();
        if p == dir_path {
            continue;
        }

        let rel = match p.strip_prefix(&root_canon) {
            Ok(r) => r,
            Err(_) => p.strip_prefix(dir_path).unwrap_or(p),
        };

        let depth = rel.components().count();
        let indent = "  ".repeat(depth.saturating_sub(1));
        let file_name = p.file_name().and_then(|f| f.to_str()).unwrap_or("");

        if tree_lines.len() >= MAX_WALK_ENTRIES {
            truncated_walk = true;
            break;
        }

        if p.is_dir() {
            total_dirs += 1;
            tree_lines.push(format!("{indent}├── {file_name}/"));
        } else {
            total_files += 1;
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("<none>");
            *ext_counts.entry(ext.to_string()).or_insert(0) += 1;
            tree_lines.push(format!("{indent}├── {file_name}"));
        }
    }

    let mut out = format!(
        "Directory: {}\nTotal: {} files, {} directories\n",
        dir_path.display(),
        total_files,
        total_dirs
    );

    if !ext_counts.is_empty() {
        let mut sorted_exts: Vec<(&String, &usize)> = ext_counts.iter().collect();
        sorted_exts.sort_by(|a, b| b.1.cmp(a.1));
        let ext_summary: Vec<String> = sorted_exts
            .into_iter()
            .take(8)
            .map(|(ext, count)| format!(".{ext} ({count})"))
            .collect();
        out.push_str(&format!("Extensions: {}\n", ext_summary.join(", ")));
    }

    out.push_str("\nTree hierarchy:\n");
    for line in tree_lines.iter().take(300) {
        out.push_str(line);
        out.push('\n');
    }
    if tree_lines.len() > 300 {
        out.push_str(&format!("... [{} more items not shown]\n", tree_lines.len() - 300));
    }
    if truncated_walk {
        out.push_str(&format!("... [stopped after {MAX_WALK_ENTRIES} entries]\n"));
    }

    Ok(out)
}

fn analyze_symbols(path: &Path, content: &str) -> Result<String, String> {
    let symbols = extract_symbols_for_file(path, content);
    if symbols.is_empty() {
        return Ok(format!(
            "No high-level declarations recognized for {}. Total lines: {}",
            path.display(),
            content.lines().count()
        ));
    }

    let mut out = format!("Symbols in {} (total: {}):\n", path.display(), symbols.len());
    for sym in symbols {
        if sym.start_line == sym.end_line {
            out.push_str(&format!("  L{}: {}\n", sym.start_line, sym.signature));
        } else {
            out.push_str(&format!(
                "  L{}-{}: {}\n",
                sym.start_line, sym.end_line, sym.signature
            ));
        }
    }
    Ok(out)
}

fn analyze_focus(path: &Path, content: &str, symbol: &str) -> Result<String, String> {
    if symbol.trim().is_empty() {
        return Err("Parameter 'symbol' is required for focus mode".to_string());
    }

    let symbols = extract_symbols_for_file(path, content);
    // An exact declaration name wins over a coincidental signature substring.
    let matched = symbols
        .iter()
        .find(|s| s.name == symbol)
        .or_else(|| symbols.iter().find(|s| s.signature.contains(symbol)));

    let lines: Vec<&str> = content.lines().collect();

    if let Some(target) = matched {
        let start = target.start_line.saturating_sub(1);
        let end = target.end_line.min(lines.len());
        let count = end.saturating_sub(start);
        let slice_len = count.min(MAX_FOCUS_LINES);

        let mut out = format!(
            "Focus on '{}' ({}) in {} (L{}-L{}):\n\n",
            target.name, target.kind, path.display(), target.start_line, end
        );
        for i in 0..slice_len {
            out.push_str(&format!("{:4} | {}\n", start + i + 1, lines[start + i]));
        }
        if count > MAX_FOCUS_LINES {
            out.push_str(&format!(
                "\n... [Truncated remaining {} lines of symbol '{}']\n",
                count - MAX_FOCUS_LINES,
                target.name
            ));
        }
        Ok(out)
    } else {
        let matching_lines: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(symbol))
            .take(10)
            .map(|(idx, line)| (idx + 1, *line))
            .collect();

        if matching_lines.is_empty() {
            Err(format!(
                "Symbol or keyword '{}' not found in {}",
                symbol,
                path.display()
            ))
        } else {
            let mut out = format!(
                "Symbol '{}' not recognized as declaration. Matching occurrences in {}:\n",
                symbol,
                path.display()
            );
            for (line_no, text) in matching_lines {
                out.push_str(&format!("{:4} | {}\n", line_no, text.trim()));
            }
            Ok(out)
        }
    }
}

async fn read_source_for_analysis(path: &Path, path_str: &str) -> Result<String, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Failed to stat file {}: {}", path_str, e))?;
    if meta.len() > MAX_ANALYZE_FILE_BYTES {
        return Err(format!(
            "File {} is too large to analyze ({} bytes exceeds {} byte limit)",
            path_str,
            meta.len(),
            MAX_ANALYZE_FILE_BYTES
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path_str, e))?;
    if bytes.iter().take(1024).any(|&b| b == 0) {
        return Err(format!("File {} appears to be binary", path_str));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[async_trait]
impl AgentTool for AnalyzeTool {
    fn name(&self) -> &str {
        "analyze"
    }

    fn description(&self) -> &str {
        "High-level structural code analyzer with zero external dependencies. Supports 'symbols' (class/function/trait signatures and line spans), 'structure' (directory tree, file counts, and extension distribution), and 'focus' (extract implementation of a specific symbol)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file or directory to analyze (relative or absolute)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["symbols", "structure", "focus"],
                    "description": "Analysis mode: 'symbols', 'structure', or 'focus'"
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol name to focus on (required when mode is 'focus')"
                },
                "focus": {
                    "type": "string",
                    "description": "Alias for 'symbol'"
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum tree depth for 'structure' mode (1-5, default 2)"
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        cancellation_token: CancellationToken,
        _update_callback: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancellation_token.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let resolved_path = self.resolve_path(path_str)?;
        let mode = arguments
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                if resolved_path.is_dir() {
                    "structure"
                } else {
                    "symbols"
                }
            });

        let output = match mode {
            "structure" => {
                let depth = arguments
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2)
                    .clamp(1, 5) as usize;
                let path = resolved_path.clone();
                let token = cancellation_token.clone();
                tokio::task::spawn_blocking(move || analyze_structure(&path, depth, &token))
                    .await
                    .map_err(|e| format!("Analyze task failed: {e}"))??
            }
            "symbols" => {
                if !resolved_path.exists() {
                    return Err(format!("File not found: {}", resolved_path.display()));
                }
                if resolved_path.is_dir() {
                    return Err(format!(
                        "Path {} is a directory. Use mode 'structure' instead.",
                        resolved_path.display()
                    ));
                }
                let content = read_source_for_analysis(&resolved_path, path_str).await?;
                analyze_symbols(&resolved_path, &content)?
            }
            "focus" => {
                if !resolved_path.exists() {
                    return Err(format!("File not found: {}", resolved_path.display()));
                }
                let symbol = arguments
                    .get("symbol")
                    .or_else(|| arguments.get("focus"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = read_source_for_analysis(&resolved_path, path_str).await?;
                analyze_focus(&resolved_path, &content, symbol)?
            }
            unknown => {
                return Err(format!(
                    "Unknown analysis mode '{}'. Supported modes: 'symbols', 'structure', 'focus'",
                    unknown
                ));
            }
        };

        let output = truncate_head(
            &output,
            MAX_ANALYZE_CHARS,
            "\n... [Output truncated to 24000 characters. Use focus mode or specific file paths]",
        );
        Ok(AgentToolResult::text(output))
    }
}
`````

