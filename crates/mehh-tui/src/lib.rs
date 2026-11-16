pub mod ansi;
pub mod components;
pub mod fuzzy;
pub mod keys;
pub mod theme;
pub mod tui;
pub mod wheel_scroll;

pub use ansi::*;
pub use components::*;
pub use fuzzy::*;
pub use keys::*;
pub use theme::*;
pub use tui::*;
pub use wheel_scroll::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn test_strip_ansi() {
        assert_eq!(
            strip_ansi("hello \x1b[31mred\x1b[0m world"),
            "hello red world"
        );
        assert_eq!(
            strip_ansi("\x1b]8;;https://example.com\x07Link\x1b]8;;\x07"),
            "Link"
        );
        assert_eq!(strip_ansi("test\x1b_mehh:c\x07cursor"), "testcursor");
    }

    #[test]
    fn test_visible_width() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("tab\tstop"), 3 + 3 + 4);
        assert_eq!(visible_width("hello\x1b[32m world\x1b[0m"), 11);
        assert_eq!(visible_width("🦀"), 2);
        assert_eq!(visible_width("你好"), 4);
    }

    #[test]
    fn test_truncate_to_width() {
        assert_eq!(
            truncate_to_width("hello world", 8, "...", false),
            "hello..."
        );
        assert_eq!(truncate_to_width("hi", 5, "...", true), "hi   ");
        let styled = "\x1b[31mhello world\x1b[0m";
        let trunc = truncate_to_width(styled, 8, "...", false);
        assert!(trunc.contains("hello"));
        assert!(trunc.ends_with("\x1b[0m"));
    }

    #[test]
    fn test_wrap_text_with_ansi() {
        let text = "hello brave new world";
        let wrapped = wrap_text_with_ansi(text, 12);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0], "hello brave");
        assert_eq!(wrapped[1], "new world");

        let long = "abcdefghijklmnopqrstuvwxyz";
        let wrapped_long = wrap_text_with_ansi(long, 10);
        assert_eq!(wrapped_long.len(), 3);
        assert_eq!(wrapped_long[0], "abcdefghij");
        assert_eq!(wrapped_long[1], "klmnopqrst");
        assert_eq!(wrapped_long[2], "uvwxyz");

        let styled = "\x1b[1mhello world this is bold\x1b[0m";
        let wrapped_styled = wrap_text_with_ansi(styled, 12);
        assert!(wrapped_styled.len() >= 2);
        assert!(wrapped_styled[1].starts_with("\x1b[1m"));
    }

    #[test]
    fn test_slice_by_column() {
        let line = "abcdefgh";
        assert_eq!(slice_by_column(line, 2, 5), "cde");

        let styled = "\x1b[32mabcdef\x1b[0m";
        let sliced = slice_by_column(styled, 1, 4);
        assert_eq!(strip_ansi(&sliced), "bcd");
    }

    #[test]
    fn test_normalize_crossterm_key() {
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(normalize_crossterm_key(ev), Some("ctrl+c".to_string()));

        let ev_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(normalize_crossterm_key(ev_enter), Some("enter".to_string()));

        let ev_shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            normalize_crossterm_key(ev_shift_enter),
            Some("shift+enter".to_string())
        );

        let ev_alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(
            normalize_crossterm_key(ev_alt_left),
            Some("alt+left".to_string())
        );

        let ev_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(normalize_crossterm_key(ev_tab), Some("tab".to_string()));

        let ev_backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(
            normalize_crossterm_key(ev_backtab),
            Some("backtab".to_string())
        );
    }

    #[test]
    fn test_matches_key() {
        assert!(matches_key("ctrl+c", "ctrl+c"));
        assert!(matches_key("enter", "return"));
        assert!(matches_key("escape", "esc"));
        assert!(matches_key("shift+ctrl+p", "ctrl+shift+p"));
        assert!(matches_key("shift+tab", "backtab"));
        assert!(!matches_key("ctrl+c", "ctrl+v"));
    }

    #[test]
    fn test_keybindings() {
        let kb = Keybindings::default_bindings();
        assert!(kb.matches("ctrl+c", TuiAction::Interrupt));
        assert!(kb.matches("escape", TuiAction::Interrupt));
        assert!(kb.matches("enter", TuiAction::Submit));
        assert!(kb.matches("shift+enter", TuiAction::Newline));
        assert!(kb.matches("ctrl+w", TuiAction::DeleteWordBackward));
        assert!(kb.matches("ctrl+y", TuiAction::Yank));
        assert!(kb.matches("ctrl+z", TuiAction::Undo));
    }

    #[test]
    fn test_composite_tui_line() {
        let base = "Hello World! This is a base line.";
        let overlay = "[POPUP]";
        let composite = composite_tui_line(base, overlay, 6, 30);
        let clean = strip_ansi(&composite);
        assert!(clean.starts_with("Hello [POPUP]"));
    }

    #[test]
    fn test_mock_terminal_and_renderer() {
        let mut term = MockTerminal::new(40, 10);
        let mut renderer = TuiRenderer::new();

        let lines = vec![
            format!("Line 1{CURSOR_MARKER}"),
            "Line 2".to_string(),
        ];
        renderer.render_frame(&mut term, lines, true).unwrap();

        let out = term.full_output();
        assert!(out.contains("\x1b[?2026h"));
        assert!(out.contains("Line 1"));
        assert!(out.contains("Line 2"));
        assert!(out.contains("\x1b[?2026l"));
    }

    #[test]
    fn test_components_basic() {
        let text = Text::new("Hello Text").with_padding(1);
        let lines = text.render(20);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], " Hello Text ");

        let spacer = Spacer::new(3);
        assert_eq!(spacer.render(20).len(), 3);

        let bbox = BorderedBox::new(Box::new(Text::new("Inner")))
            .with_title("Title")
            .with_style(BorderStyle::Rounded);
        let box_lines = bbox.render(20);
        assert!(box_lines.len() >= 3);
        assert!(box_lines[0].contains("Title"));
        assert!(box_lines[1].contains("Inner"));

        let mut loader = Loader::new("Loading...");
        assert!(loader.render(20)[0].contains("Loading..."));
        loader.tick();
        assert!(loader.render(20)[0].contains("Loading..."));

        let items = vec![
            SelectItem::new("1", "Apple").with_description("Fruit"),
            SelectItem::new("2", "Banana").with_description("Fruit"),
        ];
        let mut list = SelectList::new(items, 5);
        let list_rendered = list.render(30);
        assert!(list_rendered[0].contains('>'));
        assert!(list_rendered[0].contains("Apple"));
        list.move_down();
        let list_rendered2 = list.render(30);
        assert!(list_rendered2[1].contains('>'));
        assert!(list_rendered2[1].contains("Banana"));

        // Fuzzy filter: "bnn" matches Banana
        list.set_filter("bnn");
        assert_eq!(list.filtered_indices(), vec![1]);
        assert_eq!(list.selected_item().map(|it| it.label.as_str()), Some("Banana"));

        // Out-of-bounds selected should not panic when rendering
        list.selected = 100;
        let safe_lines = list.render(30);
        assert!(!safe_lines.is_empty());
    }

    #[test]
    fn test_editor_operations_and_paste_collapse() {
        let mut editor = Editor::new();
        editor.insert_text("hello");
        assert_eq!(editor.text(), "hello");

        // Test multiline paste collapsing (> 10 lines)
        let large_paste = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12";
        editor.clear();
        editor.insert_text(large_paste);

        // Render buffer contains collapsed token
        let rendered = editor.render(80);
        assert!(rendered[0].contains("[paste #1 +12 lines]"));

        // But .text() expands to the full original payload!
        assert_eq!(editor.text(), large_paste);

        // Test undo
        assert!(editor.undo());
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn test_editor_autocomplete() {
        let mut editor = Editor::new();
        editor.set_text("/mo");
        editor.set_autocomplete(vec![
            SelectItem::new("model", "model"),
            SelectItem::new("mode", "mode"),
        ]);
        let rendered = editor.render(40);
        assert!(rendered.iter().any(|l| l.contains("model")));

        let kb = Keybindings::default_bindings();
        let out = editor.handle_editor_key("enter", &kb);
        assert!(matches!(out, EditorOutput::Changed(_)));
        assert!(editor.autocomplete.is_none());
    }

    #[test]
    fn test_markdown_view() {
        let md_src = "# Heading 1\n\nThis is a paragraph with **bold** and *italic* and `code`.\n\n- item 1\n- item 2\n\n```rust\nfn main() {}\n```";
        let view = MarkdownView::new(md_src);
        let lines = view.render(50);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.contains("Heading 1")));
        assert!(lines.iter().any(|l| l.contains("item 1")));
        assert!(lines.iter().any(|l| l.contains("main()")));
    }

    #[test]
    fn test_theme_dark_and_light() {
        let dark = Theme::dark();
        assert_eq!(dark.name, "dark");
        assert!(!dark.accent.is_empty());
        assert!(!dark.border.is_empty());
        assert!(!dark.success.is_empty());
        assert_eq!(dark.fg(&dark.accent, "text"), format!("{}text\x1b[0m", dark.accent));
        assert_eq!(dark.fg("", "plain"), "plain");

        let light = Theme::light();
        assert_eq!(light.name, "light");
        assert_ne!(dark, light);
        assert_eq!(Theme::from_name("light"), light);
        assert_eq!(Theme::from_name("DARK"), dark);
        assert_eq!(Theme::from_name("unknown"), dark);
    }

    #[test]
    fn test_hex_to_ansi() {
        assert_eq!(
            hex_to_fg_ansi("#00ff88"),
            Some("\x1b[38;2;0;255;136m".to_string())
        );
        assert_eq!(
            hex_to_bg_ansi("#00ff88"),
            Some("\x1b[48;2;0;255;136m".to_string())
        );
        assert_eq!(
            hex_to_fg_ansi("00ff88"),
            Some("\x1b[38;2;0;255;136m".to_string())
        );
        assert_eq!(
            hex_to_fg_ansi("#f00"),
            Some("\x1b[38;2;255;0;0m".to_string())
        );
        assert_eq!(hex_to_fg_ansi("not-a-color"), None);
        assert_eq!(hex_to_fg_ansi(""), None);
    }

    #[test]
    fn test_theme_json_load_and_override() {
        let temp_dir = std::env::temp_dir();
        let test_theme_path = temp_dir.join("mehh_test_theme_override.json");

        let json_content = r##"{
            "name": "neon",
            "accent": "#00ff88",
            "border": "\u001b[35m",
            "selectedBg": "#112233",
            "mdHeading": "#ff5500"
        }"##;
        std::fs::write(&test_theme_path, json_content).unwrap();

        let loaded = Theme::load(Some(test_theme_path.to_str().unwrap()));
        assert_eq!(loaded.name, "neon");
        assert_eq!(loaded.accent, "\x1b[38;2;0;255;136m");
        assert_eq!(loaded.border, "\x1b[35m");
        assert_eq!(loaded.selected_bg, "\x1b[48;2;17;34;51m");
        assert_eq!(loaded.md_heading, "\x1b[38;2;255;85;0m");
        // Non-specified fields fall back to dark defaults
        assert_eq!(loaded.success, Theme::dark().success);

        let _ = std::fs::remove_file(&test_theme_path);

        // Fallbacks for missing files or None
        assert_eq!(Theme::load(None), Theme::dark());
        assert_eq!(Theme::load(Some("non_existent_theme_98765")), Theme::dark());
    }

    #[test]
    fn test_markdown_view_with_theme() {
        let mut custom_theme = Theme::dark();
        custom_theme.md_heading = "\x1b[38;2;255;0;128m".to_string();
        custom_theme.md_code = "\x1b[38;2;0;200;255m".to_string();

        let md_src = "# Custom Title\n\nInline `code_snippet` here.";
        let view = MarkdownView::new(md_src).with_theme(custom_theme);
        let lines = view.render(60);

        assert!(lines.iter().any(|l| l.contains("\x1b[38;2;255;0;128m# Custom Title")));
        assert!(lines.iter().any(|l| l.contains("\x1b[38;2;0;200;255mcode_snippet\x1b[0m")));
    }
}
