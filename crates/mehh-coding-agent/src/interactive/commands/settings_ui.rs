use super::*;

/// Copies a single interactive toggle from one settings set into another.
///
/// The remaining keys (model, provider, limits) belong to the user's machine or
/// project and are never overwritten by an interactive toggle.
pub fn copy_toggle(target: &mut Settings, source: &Settings, key: &str) {
    match key {
        "default_provider" => target.default_provider = source.default_provider.clone(),
        "default_model" => target.default_model = source.default_model.clone(),
        "permission_mode" => target.permission_mode = source.permission_mode,
        "theme" => target.theme = source.theme.clone(),
        "steering_mode" => target.steering_mode = source.steering_mode,
        "follow_up_mode" => target.follow_up_mode = source.follow_up_mode,
        "default_thinking_level" => target.default_thinking_level = source.default_thinking_level,
        "hide_thinking_block" => target.hide_thinking_block = source.hide_thinking_block,
        "auto_compact" => target.auto_compact = source.auto_compact,
        _ => {}
    }
}

/// Persists one or more interactive settings in a single read-modify-write cycle.
///
/// The target file is loaded first, so a malformed file is reported instead of
/// overwritten, and only the changed keys are written, so project-local values of
/// the other keys are not promoted into the global configuration.
pub fn persist_settings_keys(
    settings: &Settings,
    keys: &[&str],
    path: Option<&Path>,
) -> anyhow::Result<()> {
    let file_path = match path {
        Some(p) => p.to_path_buf(),
        None => SettingsManager::global_path()?,
    };
    let mut target = SettingsManager::load_file(&file_path)?;
    for &key in keys {
        copy_toggle(&mut target, settings, key);
    }
    SettingsManager::save_file(&file_path, &target)
}

/// Persists a single interactive setting.
pub fn persist_settings(settings: &Settings, key: &str, path: Option<&Path>) -> anyhow::Result<()> {
    persist_settings_keys(settings, &[key], path)
}

pub(super) fn mehh_themes_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))
        .map(PathBuf::from)?;
    Some(home.join(".mehh").join("agent").join("themes"))
}

/// Returns true when a theme name can be resolved to a valid theme.
///
/// Built-in names (`dark`, `light`) always resolve; other names resolve when they
/// point at a readable, valid theme file.
pub fn theme_source_exists(name: &str) -> bool {
    Theme::try_load(name).is_ok()
}

pub(super) fn queue_mode_label(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::OneAtATime => "one-at-a-time",
        QueueMode::All => "all",
    }
}

/// Status line shown after an approval decision saved a permission rule.
/// `save_result` is `None` for a session-only rule, which is never persisted.
pub(in crate::interactive) fn permission_rule_status_message(
    persist_rule: bool,
    tool_name: &str,
    save_result: Option<&anyhow::Result<()>>,
) -> String {
    let scope = if persist_rule { "Rule" } else { "Session rule" };
    match save_result {
        Some(Err(e)) => format!("Could not save rule: {e}"),
        Some(Ok(())) | None => format!("{scope} added: always allow tool '{tool_name}'"),
    }
}

/// Human readable label for a thinking level.
pub fn thinking_level_label(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// Discovers built-in themes plus any `.json` files in `~/.mehh/agent/themes/`.
pub(in crate::interactive) fn discover_available_themes() -> Vec<String> {
    let mut themes = vec!["dark".to_string(), "light".to_string()];
    if let Some(dir) = mehh_themes_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut custom: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| ext == "json")
                })
                .filter_map(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                })
                .collect();
            custom.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then(a.cmp(b)));
            for name in custom {
                if !themes.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                    themes.push(name);
                }
            }
        }
    }
    themes
}

/// Monotonic permission-mode order from safest/read-only to full autonomy.
pub(in crate::interactive) const PERMISSION_MODES: [crate::tool_inspection::PermissionMode; 4] = [
    crate::tool_inspection::PermissionMode::Chat,
    crate::tool_inspection::PermissionMode::Approve,
    crate::tool_inspection::PermissionMode::SmartApprove,
    crate::tool_inspection::PermissionMode::Auto,
];

/// Returns the live runtime permission mode when unlocked, falling back to saved settings.
pub(in crate::interactive) fn effective_permission_mode(
    session: &AgentSession,
) -> crate::tool_inspection::PermissionMode {
    session
        .inspection_manager
        .try_mode()
        .or(session.settings.permission_mode)
        .unwrap_or_default()
}

/// Builds a settings snapshot reflecting active runtime mode and thinking level without mutating `session.settings`.
pub(in crate::interactive) async fn effective_settings_snapshot(session: &AgentSession) -> Settings {
    let mut snapshot = session.settings.clone();
    snapshot.permission_mode = Some(session.inspection_manager.mode().await);
    snapshot.default_thinking_level = session.settings.default_thinking_level.or_else(|| {
        Some(
            session
                .effective_thinking_level()
                .unwrap_or(ThinkingLevel::Off),
        )
    });
    snapshot
}

pub(super) fn format_thinking_setting_summary(saved: ThinkingLevel, session: &AgentSession) -> String {
    let saved_label = thinking_level_label(saved);
    let profile = mehh_ai::model_thinking_profile(&session.model);
    if !profile.supported {
        if saved != ThinkingLevel::Off {
            return format!(
                "Thinking effort → {saved_label} (active model '{}' uses n/a)",
                session.model.id
            );
        }
        return format!("Thinking effort → {saved_label}");
    }
    let effective = session
        .effective_thinking_level()
        .unwrap_or(ThinkingLevel::Off);
    if effective != saved {
        format!(
            "Thinking effort → {saved_label} (active model '{}' uses {})",
            session.model.id,
            thinking_level_label(effective)
        )
    } else {
        format!("Thinking effort → {saved_label}")
    }
}

pub(super) fn available_themes_with_current(current: &str) -> Vec<String> {
    let mut themes = discover_available_themes();
    if !current.trim().is_empty() && !themes.iter().any(|t| t.eq_ignore_ascii_case(current)) {
        themes.push(current.to_string());
    }
    themes
}

pub(in crate::interactive) fn set_session_thinking_level(session: &mut AgentSession, level: ThinkingLevel) {
    session.set_thinking_level_explicit(level);
}

pub(super) fn thinking_levels_for_model(model: &mehh_ai::Model, current: ThinkingLevel) -> Vec<ThinkingLevel> {
    let profile = mehh_ai::model_thinking_profile(model);
    let mut levels = profile.levels;
    if profile.supported
        && !levels.contains(&current)
        && (model.provider.eq_ignore_ascii_case("faux") || current != ThinkingLevel::Off)
    {
        levels.push(current);
    }
    levels
}

pub(in crate::interactive) fn parse_thinking_level_str(raw: &str) -> Option<ThinkingLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" | "med" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

pub(super) fn thinking_levels_for_current(current: ThinkingLevel) -> Vec<ThinkingLevel> {
    let mut levels = vec![
        ThinkingLevel::Off,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
    ];
    if !levels.contains(&current) {
        let insert_at = match current {
            ThinkingLevel::Minimal => 1,
            ThinkingLevel::Xhigh | ThinkingLevel::Max => levels.len(),
            _ => levels.len(),
        };
        levels.insert(insert_at, current);
    }
    levels
}

/// Canonical lowercase label for a permission mode.
pub(in crate::interactive) fn permission_mode_label(mode: crate::tool_inspection::PermissionMode) -> &'static str {
    use crate::tool_inspection::PermissionMode;
    match mode {
        PermissionMode::Auto => "auto",
        PermissionMode::SmartApprove => "smart_approve",
        PermissionMode::Approve => "approve",
        PermissionMode::Chat => "chat",
    }
}

/// Concise explanation of what each permission mode permits.
pub(in crate::interactive) fn permission_mode_description(
    mode: crate::tool_inspection::PermissionMode,
) -> &'static str {
    use crate::tool_inspection::PermissionMode;
    match mode {
        PermissionMode::SmartApprove => {
            "Auto-allows reads & safe cmds; asks before edits or risky shell"
        }
        PermissionMode::Approve => "Asks confirmation before every tool call",
        PermissionMode::Auto => {
            "Runs tools automatically without asking (security blocks still active)"
        }
        PermissionMode::Chat => {
            "Read-only exploration; blocks all file edits and shell commands"
        }
    }
}

/// Concise explanation of each thinking effort level.
pub(in crate::interactive) fn thinking_level_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "Disabled — fastest responses, no extended reasoning",
        ThinkingLevel::Minimal => "Minimal reasoning for very light tasks",
        ThinkingLevel::Low => "Light reasoning for straightforward tasks",
        ThinkingLevel::Medium => "Balanced reasoning depth and speed",
        ThinkingLevel::High => "Deep reasoning for complex debugging and design",
        ThinkingLevel::Xhigh => "Extended deep reasoning for hard problems",
        ThinkingLevel::Max => "Maximum reasoning budget",
    }
}

pub(super) fn theme_description(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("dark") {
        "Low-glare dark terminal palette"
    } else if name.eq_ignore_ascii_case("light") {
        "High-contrast palette for bright terminals"
    } else {
        "Custom theme from ~/.mehh/agent/themes"
    }
}

/// Applies and persists a validated theme selection, returning a status message.
pub(in crate::interactive) fn apply_theme_choice(
    name: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> String {
    let theme = match Theme::try_load(name) {
        Ok(t) => t,
        Err(e) if e.ends_with("not found") => {
            return format!(
                "Unknown theme: '{name}'. Use dark, light, or a theme file name."
            );
        }
        Err(e) => return format!("Failed to load theme '{name}': {e}"),
    };
    state.set_theme(theme.clone());
    session.settings.theme = Some(name.to_string());
    match persist_settings(&session.settings, "theme", state.settings_file.as_deref()) {
        Ok(()) => format!("Theme set to '{name}' (active: {})", theme.name),
        Err(e) => format!("Theme set to '{name}' but saving settings failed: {e}"),
    }
}

/// Applies a live theme preview while the `/theme` picker is open.
pub(in crate::interactive) fn sync_theme_preview(state: &mut InteractiveState) {
    let selected_id = state
        .selector
        .as_ref()
        .filter(|sel| sel.kind == SelectorKind::Theme)
        .and_then(|sel| sel.list.selected_item())
        .map(|item| item.id.clone());
    let Some(id) = selected_id else {
        return;
    };
    if let Some(cached) = state.theme_preview_cache.get(&id).cloned() {
        state.set_theme(cached);
        return;
    }
    if let Ok(t) = Theme::try_load(&id) {
        state.theme_preview_cache.insert(id, t.clone());
        state.set_theme(t);
    }
}

pub(super) fn select_list_preselected(
    items: Vec<SelectItem>,
    max_visible: usize,
    selected_idx: usize,
) -> SelectList {
    let mut list = SelectList::new(items, max_visible);
    if !list.items.is_empty() {
        list.selected = selected_idx.min(list.items.len() - 1);
    }
    list
}

pub(super) fn cycle_index(len: usize, current: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    }
}

/// Converts a built `SettingsList` into `SelectItem` entries without re-scanning disk themes.
pub(in crate::interactive) fn settings_selector_items_from_list(sl: &SettingsList) -> Vec<SelectItem> {
    sl.items
        .iter()
        .map(|it| {
            let mut item =
                SelectItem::new(it.id.clone(), format!("{}: {}", it.label, it.current_value));
            if let Some(ref desc) = it.description {
                item = item.with_description(desc.clone());
            }
            item
        })
        .collect()
}

/// Builds the picker entries for the settings screen from `build_settings_list`.
#[cfg(test)]
pub(in crate::interactive) fn settings_selector_items(settings: &Settings) -> Vec<SelectItem> {
    settings_selector_items_from_list(&build_settings_list(settings))
}

/// Builds the two-column settings list component for the settings screen.
pub(in crate::interactive) fn build_settings_list(settings: &Settings) -> SettingsList {
    let current_mode = settings.permission_mode.unwrap_or_default();
    let mode_values: Vec<String> = PERMISSION_MODES
        .iter()
        .map(|&m| permission_mode_label(m).to_string())
        .collect();
    let mode_descs: Vec<(String, String)> = PERMISSION_MODES
        .iter()
        .map(|&m| {
            (
                permission_mode_label(m).to_string(),
                permission_mode_description(m).to_string(),
            )
        })
        .collect();

    let current_thinking_enum = settings.default_thinking_level.unwrap_or(ThinkingLevel::Off);
    let current_thinking = thinking_level_label(current_thinking_enum);
    let thinking_levels = thinking_levels_for_current(current_thinking_enum);
    let thinking_values: Vec<String> = thinking_levels
        .iter()
        .map(|&l| thinking_level_label(l).to_string())
        .collect();
    let thinking_descs: Vec<(String, String)> = thinking_levels
        .iter()
        .map(|&l| {
            (
                thinking_level_label(l).to_string(),
                thinking_level_description(l).to_string(),
            )
        })
        .collect();

    let current_auto_compact = if settings.auto_compact.unwrap_or(true) {
        "on"
    } else {
        "off"
    };
    let current_theme = settings.theme.clone().unwrap_or_else(|| "dark".to_string());
    let themes = available_themes_with_current(&current_theme);
    let theme_descs: Vec<(String, String)> = themes
        .iter()
        .map(|t| (t.clone(), theme_description(t).to_string()))
        .collect();
    let current_thinking_blocks = if settings.hide_thinking_block.unwrap_or(false) {
        "hide"
    } else {
        "show"
    };

    let items = vec![
        SettingItem::new(
            "permission_mode",
            "Permission mode",
            permission_mode_label(current_mode),
        )
        .with_category("SAFETY & AGENT")
        .with_values(mode_values)
        .with_description("Controls when the agent asks before running tools or shell commands")
        .with_value_descriptions(mode_descs),
        SettingItem::new(
            "default_thinking_level",
            "Thinking effort",
            current_thinking,
        )
        .with_category("SAFETY & AGENT")
        .with_values(thinking_values)
        .with_description("Default reasoning depth for supported models")
        .with_value_descriptions(thinking_descs),
        SettingItem::new("auto_compact", "Auto-compact", current_auto_compact)
            .with_category("SAFETY & AGENT")
            .with_values(vec!["on".to_string(), "off".to_string()])
            .with_description("Automatically summarize older history before hitting the context limit")
            .with_value_descriptions(vec![
                (
                    "on".to_string(),
                    "Compacts older turns automatically before context overflows".to_string(),
                ),
                (
                    "off".to_string(),
                    "Manual compaction only via /compact".to_string(),
                ),
            ]),
        SettingItem::new("theme", "Color theme", current_theme)
            .with_category("APPEARANCE")
            .with_values(themes)
            .with_description("Terminal color palette (built-in or ~/.mehh/agent/themes)")
            .with_value_descriptions(theme_descs),
        SettingItem::new(
            "hide_thinking_block",
            "Thinking blocks",
            current_thinking_blocks,
        )
        .with_category("APPEARANCE")
        .with_values(vec!["show".to_string(), "hide".to_string()])
        .with_description(
            "Show or hide model reasoning blocks in transcript (Ctrl+T toggles expand)",
        )
        .with_value_descriptions(vec![
            (
                "show".to_string(),
                "Render reasoning preview blocks above assistant replies".to_string(),
            ),
            (
                "hide".to_string(),
                "Hide reasoning blocks completely from the transcript".to_string(),
            ),
        ]),
        SettingItem::new(
            "steering_mode",
            "Mid-turn messages",
            queue_mode_label(settings.steering_mode.unwrap_or_default()),
        )
        .with_category("MESSAGE QUEUES")
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("How messages sent while the agent is busy are delivered (steering)")
        .with_value_descriptions(vec![
            (
                "one-at-a-time".to_string(),
                "Deliver one queued message per step so the agent adjusts incrementally"
                    .to_string(),
            ),
            (
                "all".to_string(),
                "Deliver all queued messages together in one batch".to_string(),
            ),
        ]),
        SettingItem::new(
            "follow_up_mode",
            "Follow-up queue",
            queue_mode_label(settings.follow_up_mode.unwrap_or_default()),
        )
        .with_category("MESSAGE QUEUES")
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("How follow-ups queued with Alt+Enter or /follow-up run after a turn")
        .with_value_descriptions(vec![
            (
                "one-at-a-time".to_string(),
                "Run one queued follow-up per turn in order".to_string(),
            ),
            (
                "all".to_string(),
                "Combine all queued follow-ups into the next turn".to_string(),
            ),
        ]),
    ];
    SettingsList::new(items, 10)
}

/// Returns the setting ID currently focused in `SelectorKind::Settings`.
/// Reads `settings_list` first so searches matching value/category descriptions stay in sync.
pub(in crate::interactive) fn settings_selected_id(selector: &Selector) -> Option<String> {
    selector
        .settings_list
        .as_ref()
        .and_then(|sl| sl.selected_item().map(|i| i.id.clone()))
        .or_else(|| selector.list.selected_item().map(|i| i.id.clone()))
}

/// Rebuilds the settings overlay after a toggle while preserving focused row, filter, and optional status banner.
pub(in crate::interactive) fn refresh_settings_selector_with_status(
    state: &mut InteractiveState,
    settings: &Settings,
    focus_id: Option<&str>,
    status: Option<(String, bool)>,
) {
    let prev = state
        .selector
        .as_ref()
        .map(|sel| sel.list.selected)
        .unwrap_or(0);
    if let Some(sel) = state.selector.as_mut() {
        let filter = sel.list.filter.clone();
        let mut detail = build_settings_list(settings);
        sel.list.set_items(settings_selector_items_from_list(&detail));
        detail.set_filter(&filter);
        let filtered = detail.filtered_indices();
        let resolved_idx = focus_id
            .and_then(|target_id| {
                filtered
                    .iter()
                    .position(|&idx| detail.items[idx].id == target_id)
            })
            .unwrap_or_else(|| {
                let count = if focus_id.is_some() {
                    filtered.len()
                } else {
                    sel.list.items.len()
                };
                prev.min(count.saturating_sub(1))
            });
        sel.list.selected = resolved_idx;
        detail.selected = resolved_idx;
        detail.status_message = status;
        sel.settings_list = Some(detail);
    }
}

/// Rebuild the settings overlay after a toggle without moving the cursor.
/// `SelectList::set_items` resets selection to zero, so restore it explicitly.
#[cfg(test)]
pub(in crate::interactive) fn refresh_settings_selector(state: &mut InteractiveState, settings: &Settings) {
    refresh_settings_selector_with_status(state, settings, None, None);
}

/// Feed a typed character or Backspace into a picker's fuzzy filter.
/// Returns false for navigation/action keys so the caller keeps handling them.
pub(in crate::interactive) fn update_selector_filter(selector: &mut Selector, key: &str) -> bool {
    let next = if key == "backspace" {
        let mut filter = selector.list.filter.clone();
        if filter.pop().is_none() {
            return true;
        }
        filter
    } else if key == " "
        && (selector.kind != SelectorKind::Settings || !selector.list.filter.is_empty())
    {
        format!("{}{}", selector.list.filter, " ")
    } else if key.chars().count() == 1
        && key.chars().all(|c| !c.is_control() && !c.is_whitespace())
    {
        format!("{}{}", selector.list.filter, key)
    } else {
        return false;
    };
    selector.list.set_filter(&next);
    if let Some(ref mut detail) = selector.settings_list {
        detail.set_filter(&next);
        detail.clear_status();
    }
    true
}

/// Cycles a single setting value forward (`forward = true`) or backward (`forward = false`).
pub(in crate::interactive) fn apply_setting_cycle_dir(
    key: &str,
    forward: bool,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<String> {
    match key {
        "permission_mode" => {
            let current = session.settings.permission_mode.unwrap_or_default();
            let curr_idx = PERMISSION_MODES
                .iter()
                .position(|&m| m == current)
                .unwrap_or(0);
            let next = PERMISSION_MODES[cycle_index(PERMISSION_MODES.len(), curr_idx, forward)];
            session.settings.permission_mode = Some(next);
            let label = permission_mode_label(next);
            if next == crate::tool_inspection::PermissionMode::Auto {
                Ok(format!(
                    "Permission mode → {label} (no approval prompts; default for all sessions)"
                ))
            } else {
                Ok(format!("Permission mode → {label}"))
            }
        }
        "theme" => {
            let current = session
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "dark".to_string());
            let themes = available_themes_with_current(&current);
            let curr_idx = themes
                .iter()
                .position(|t| t.eq_ignore_ascii_case(&current))
                .unwrap_or(0);
            let next = themes[cycle_index(themes.len(), curr_idx, forward)].clone();
            state.set_theme(Theme::load(Some(&next)));
            session.settings.theme = Some(next.clone());
            Ok(format!("Color theme → {next}"))
        }
        "steering_mode" => {
            let current = session.settings.steering_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.steering_mode = Some(next);
            Ok(format!("Mid-turn messages → {}", queue_mode_label(next)))
        }
        "follow_up_mode" => {
            let current = session.settings.follow_up_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.follow_up_mode = Some(next);
            Ok(format!("Follow-up queue → {}", queue_mode_label(next)))
        }
        "default_thinking_level" => {
            let current = session.settings.default_thinking_level.unwrap_or(ThinkingLevel::Off);
            let levels = thinking_levels_for_current(current);
            let curr_idx = levels.iter().position(|&l| l == current).unwrap_or(0);
            let next = levels[cycle_index(levels.len(), curr_idx, forward)];
            session.settings.default_thinking_level = Some(next);
            set_session_thinking_level(session, next);
            Ok(format_thinking_setting_summary(next, session))
        }
        "hide_thinking_block" => {
            let next = !session.settings.hide_thinking_block.unwrap_or(false);
            session.settings.hide_thinking_block = Some(next);
            Ok(if next {
                "Thinking blocks → hide".to_string()
            } else {
                "Thinking blocks → show".to_string()
            })
        }
        "auto_compact" => {
            let next = !session.settings.auto_compact.unwrap_or(true);
            session.settings.auto_compact = Some(next);
            Ok(if next {
                "Auto-compact → on".to_string()
            } else {
                "Auto-compact → off".to_string()
            })
        }
        other => anyhow::bail!("Unknown setting: '{other}'"),
    }
}

/// Cycles a single setting value and applies it to the running session.
#[cfg(test)]
pub(in crate::interactive) fn apply_setting_cycle(
    key: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<String> {
    apply_setting_cycle_dir(key, true, session, state)
}

pub(super) fn canonical_setting_key(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "permission_mode" | "permission" | "mode" => Some("permission_mode"),
        "default_thinking_level" | "thinking" | "thinking_level" | "thinking_effort" => {
            Some("default_thinking_level")
        }
        "auto_compact" | "compact" => Some("auto_compact"),
        "theme" | "color_theme" => Some("theme"),
        "hide_thinking_block" | "hide_thinking" | "thinking_blocks" => Some("hide_thinking_block"),
        "steering_mode" | "steering" | "mid_turn" => Some("steering_mode"),
        "follow_up_mode" | "follow_up" | "followup" => Some("follow_up_mode"),
        _ => None,
    }
}

pub(super) fn parse_queue_mode(raw: &str) -> anyhow::Result<QueueMode> {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "one-at-a-time" | "one" | "single" => Ok(QueueMode::OneAtATime),
        "all" | "batch" => Ok(QueueMode::All),
        other => anyhow::bail!("Invalid queue mode '{other}'. Choose: one-at-a-time, all"),
    }
}

pub(super) fn parse_bool_toggle(raw: &str, truthy: &[&str], falsy: &[&str], valid_hint: &str) -> anyhow::Result<bool> {
    let lower = raw.trim().to_ascii_lowercase();
    if truthy.contains(&lower.as_str()) {
        Ok(true)
    } else if falsy.contains(&lower.as_str()) {
        Ok(false)
    } else {
        anyhow::bail!("Invalid value '{raw}'. Choose: {valid_hint}")
    }
}

/// Applies an explicit `/settings <key> <value>` update and syncs runtime state.
pub(in crate::interactive) async fn apply_setting_value(
    raw_key: &str,
    raw_value: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<(String, String)> {
    let Some(key) = canonical_setting_key(raw_key) else {
        anyhow::bail!(
            "Unknown setting '{raw_key}'. Available: permission_mode, thinking, auto_compact, theme, thinking_blocks, steering_mode, follow_up_mode"
        );
    };
    let val = raw_value.trim();
    let summary = match key {
        "permission_mode" => {
            let mode = val.parse::<crate::tool_inspection::PermissionMode>().map_err(|e| anyhow::anyhow!(e))?;
            session.inspection_manager.set_mode(mode).await;
            session.settings.permission_mode = Some(mode);
            format!("Permission mode → {}", permission_mode_label(mode))
        }
        "default_thinking_level" => {
            let Some(lvl) = parse_thinking_level_str(val) else {
                anyhow::bail!(
                    "Invalid thinking effort '{val}'. Choose: off, minimal, low, medium, high, xhigh, max"
                );
            };
            session.settings.default_thinking_level = Some(lvl);
            set_session_thinking_level(session, lvl);
            format_thinking_setting_summary(lvl, session)
        }
        "auto_compact" => {
            let enabled = parse_bool_toggle(val, &["on", "true", "yes", "1"], &["off", "false", "no", "0"], "on, off")?;
            session.settings.auto_compact = Some(enabled);
            if enabled {
                "Auto-compact → on".to_string()
            } else {
                "Auto-compact → off".to_string()
            }
        }
        "theme" => {
            let theme = Theme::try_load(val).map_err(|e| anyhow::anyhow!("{e}"))?;
            state.set_theme(theme);
            session.settings.theme = Some(val.to_string());
            format!("Color theme → {val}")
        }
        "hide_thinking_block" => {
            let normalized_raw = raw_key.trim().to_ascii_lowercase().replace('-', "_");
            let hidden = if normalized_raw == "thinking_blocks" {
                let show = parse_bool_toggle(
                    val,
                    &["show", "on", "true", "1"],
                    &["hide", "off", "false", "0"],
                    "show, hide",
                )?;
                !show
            } else {
                parse_bool_toggle(
                    val,
                    &["hide", "true", "on", "1"],
                    &["show", "false", "off", "0"],
                    "show, hide",
                )?
            };
            session.settings.hide_thinking_block = Some(hidden);
            if hidden {
                "Thinking blocks → hide".to_string()
            } else {
                "Thinking blocks → show".to_string()
            }
        }
        "steering_mode" => {
            let mode = parse_queue_mode(val)?;
            session.settings.steering_mode = Some(mode);
            format!("Mid-turn messages → {}", queue_mode_label(mode))
        }
        "follow_up_mode" => {
            let mode = parse_queue_mode(val)?;
            session.settings.follow_up_mode = Some(mode);
            format!("Follow-up queue → {}", queue_mode_label(mode))
        }
        _ => unreachable!(),
    };
    Ok((key.to_string(), summary))
}

/// Cycles the active model's supported thinking levels directly (e.g., clicking `thinking:` pill).
pub(in crate::interactive) fn cycle_session_thinking(session: &mut AgentSession, state: &mut InteractiveState) {
    let profile = mehh_ai::model_thinking_profile(&session.model);
    if !profile.supported || profile.levels.is_empty() {
        state.items.push(TranscriptItem::System(format!(
            "Model '{}' does not support extended thinking.",
            session.model.id
        )));
        return;
    }
    let current = session
        .effective_thinking_level()
        .unwrap_or(ThinkingLevel::Off);
    let idx = profile
        .levels
        .iter()
        .position(|&lvl| lvl == current)
        .unwrap_or(0);
    let next = profile.levels[(idx + 1) % profile.levels.len()];
    session.set_thinking_level_explicit(next);
    state.status = None;
    let label = thinking_level_label(next);
    state.items.push(TranscriptItem::System(if next == ThinkingLevel::Off {
        "Thinking disabled for this session.".to_string()
    } else {
        format!("Thinking level set to: {label}")
    }));
}
