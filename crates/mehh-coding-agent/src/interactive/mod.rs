//! Interactive full-screen TUI mode for mehh coding agent.
//! Provides conversational transcript, live markdown streaming, slash commands,
//! editor with history and autocomplete, and async agent loop turns.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, MouseEventKind};
use futures::FutureExt;
use mehh_agent::{run_agent_loop, AgentEvent, AgentTool, QueueMode};
use mehh_ai::oauth::{self, OAuthProviderKind, StoredCredential};
use mehh_ai::{
    AssistantContentBlock, AssistantMessageEvent, Message, StopReason, ThinkingLevel, UserContent,
    UserContentBlock,
};
use mehh_tui::{
    normalize_crossterm_key, truncate_to_width, wrap_text_with_ansi, BorderStyle, BorderedBox,
    Component, CrosstermTerminal, Editor, EditorOutput, Keybindings, Loader, SelectItem,
    SelectList, SettingItem, SettingsList, Terminal, Text, Theme, TuiAction, TuiRenderer,
    WheelScrollAccelerator,
};
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSession;
use crate::mcp_manager::{McpConnectionSummary, McpManager};
use crate::session::{SessionEntryKind, SessionManager};
use crate::settings::{Settings, SettingsManager};

mod commands;
mod events;
mod render;
mod state;
#[cfg(test)]
mod tests;

pub use state::{
    BusyInput, ClickAction, ClickRegion, ConnectInputKind, ConnectInputState,
    InteractiveApprovalBroker, InteractiveState, PendingApproval, Selector, SelectorKind,
    SlashCommandOutcome, SubagentChildRow, TranscriptItem, route_busy_input,
};
use state::{anchored_scroll_offset, scroll_down_anchored, scroll_up_anchored, slice_transcript_segments};
pub use commands::{builtin_slash_commands, copy_toggle, format_hotkeys, handle_slash_command, perform_login, perform_login_with_prompt, perform_logout, persist_settings, rebuild_transcript_items, refresh_session_api_key, theme_source_exists, thinking_level_label};
use commands::{ToolApprovalDecision, apply_interactive_model_switch, apply_setting_cycle_dir, apply_theme_choice, connected_providers_with_session, cycle_session_thinking, disconnect_provider, effective_permission_mode, effective_settings_snapshot, load_interactive_registry, open_connect_for_provider, open_connect_provider_selector, open_model_selector, open_session_switcher, parse_thinking_level_str, permission_mode_description, permission_rule_status_message, refresh_connected_state, refresh_settings_selector_with_status, set_session_thinking_level, settings_selected_id, sync_mcp_tools, sync_theme_preview, thinking_level_description, tool_approval_decision, update_autocomplete_with_context, update_selector_filter};
#[cfg(test)]
use commands::{apply_setting_cycle, build_settings_list, refresh_settings_selector, settings_selector_items, split_system_prompt, update_autocomplete_with_cwd};
pub use render::render_interactive_frame;
use render::render_now;
#[cfg(test)]
use render::{cap_preview_lines, compact_count, compact_path, ctx_bar, diff_visible_lines, elapsed_label, left_right, render_user_turn, scroll_pill_text, scrollbar_char, status_icon, thinking_preview, tool_duration_label};
pub use events::{apply_agent_event, run_interactive_mode};
pub(crate) use events::AgentSignal;
#[cfg(test)]
use events::{SignalOutcome, apply_agent_signal, drain_agent_signals, expand_prompt_with_files, handle_agent_signal_async, handle_connect_input_paste, handle_interactive_paste, handle_mouse_event, handle_non_approval_selector_key, start_turn};
