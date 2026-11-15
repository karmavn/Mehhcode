pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

use std::path::PathBuf;
use std::sync::Arc;
use mehh_agent::AgentTool;

pub fn create_coding_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone())),
        Arc::new(EditTool::new(cwd.clone())),
        Arc::new(BashTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd)),
    ]
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
        assert_eq!(tools.len(), 7);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec!["read", "write", "edit", "bash", "grep", "find", "ls"]
        );
    }
}
