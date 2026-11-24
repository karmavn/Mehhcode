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
pub mod web;
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
pub use web::{
    ProviderId, UrlPolicyChecker, WebConfig, WebFetchTool, WebProviderChoice, WebRuntime,
    WebSearchTool,
};
pub use write::WriteTool;

use std::path::PathBuf;
use std::sync::Arc;
use mehh_agent::AgentTool;

pub fn create_coding_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    create_coding_tools_with_web(cwd, WebConfig::default())
}

pub fn create_coding_tools_with_web(cwd: PathBuf, web: WebConfig) -> Vec<Arc<dyn AgentTool>> {
    select_coding_tools_with_web(cwd, None, &[], web).expect("built-in tool list is valid")
}

pub fn create_all_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    let all: Vec<String> = ALL_TOOL_NAMES.iter().map(|s| (*s).to_string()).collect();
    select_coding_tools_with_web(cwd, Some(&all), &[], WebConfig::default())
        .expect("built-in tool list is valid")
}

const ALL_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "undo_edit",
    "bash",
    "grep",
    "find",
    "ls",
    "analyze",
    "powershell",
    "web_search",
    "web_fetch",
];

/// Build only the requested tools and reject unknown names instead of silently
/// ignoring typos. PowerShell is opt-in when no explicit include list is given.
pub fn select_coding_tools(
    cwd: PathBuf,
    include: Option<&[String]>,
    exclude: &[String],
) -> anyhow::Result<Vec<Arc<dyn AgentTool>>> {
    select_coding_tools_with_web(cwd, include, exclude, WebConfig::default())
}

/// Variant of [`select_coding_tools`] parameterized with [`WebConfig`].
pub fn select_coding_tools_with_web(
    cwd: PathBuf,
    include: Option<&[String]>,
    exclude: &[String],
    web: WebConfig,
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
            .filter(|name| match **name {
                "powershell" => false,
                "web_search" => web.enabled && web.provider != WebProviderChoice::Disabled,
                "web_fetch" => web.enabled,
                _ => true,
            })
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

    if wants("web_search") || wants("web_fetch") {
        let runtime = WebRuntime::try_new(web)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        if wants("web_search") {
            selected.push(Arc::new(WebSearchTool::new(runtime.clone())));
        }
        if wants("web_fetch") {
            selected.push(Arc::new(WebFetchTool::new(runtime)));
        }
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
        assert_eq!(tools.len(), 11);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "read",
                "write",
                "edit",
                "undo_edit",
                "bash",
                "grep",
                "find",
                "ls",
                "analyze",
                "web_search",
                "web_fetch",
            ]
        );
    }

    #[test]
    fn create_coding_tools_excludes_powershell_while_create_all_tools_includes_it() {
        let default_tools = create_coding_tools(PathBuf::from("."));
        assert_eq!(default_tools.len(), 11);
        assert!(!default_tools.iter().any(|t| t.name() == "powershell"));

        let all_tools = create_all_tools(PathBuf::from("."));
        assert_eq!(all_tools.len(), 12);
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
        assert_eq!(without_bash.len(), 10);
        assert!(!without_bash.iter().any(|t| t.name() == "bash"));
    }

    #[test]
    fn test_web_config_omits_disabled_tools() {
        let disabled_all = create_coding_tools_with_web(
            PathBuf::from("."),
            WebConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert_eq!(disabled_all.len(), 9);
        assert!(!disabled_all.iter().any(|t| t.name() == "web_search" || t.name() == "web_fetch"));

        let disabled_provider = create_coding_tools_with_web(
            PathBuf::from("."),
            WebConfig {
                provider: WebProviderChoice::Disabled,
                ..Default::default()
            },
        );
        assert_eq!(disabled_provider.len(), 10);
        assert!(!disabled_provider.iter().any(|t| t.name() == "web_search"));
        assert!(disabled_provider.iter().any(|t| t.name() == "web_fetch"));
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
