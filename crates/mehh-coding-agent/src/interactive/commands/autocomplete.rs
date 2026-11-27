use super::*;
use super::settings_ui::{canonical_setting_key, theme_description, thinking_levels_for_model};

pub(super) fn mode_autocomplete_items(session: Option<&AgentSession>) -> Vec<SelectItem> {
    let current = session.map(effective_permission_mode);
    PERMISSION_MODES
        .iter()
        .map(|&mode| {
            let key = permission_mode_label(mode);
            let desc = if current == Some(mode) {
                format!("{} (current)", permission_mode_description(mode))
            } else {
                permission_mode_description(mode).to_string()
            };
            SelectItem::new(key, key).with_description(desc)
        })
        .collect()
}

pub(super) fn thinking_autocomplete_items(session: Option<&AgentSession>) -> Vec<SelectItem> {
    let current = session.map(|s| {
        s.effective_thinking_level()
            .unwrap_or(ThinkingLevel::Off)
    });
    let levels = match (session, current) {
        (Some(s), Some(cur)) => thinking_levels_for_model(&s.model, cur),
        _ => vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
    };
    levels
        .iter()
        .map(|&lvl| {
            let key = thinking_level_label(lvl);
            let desc = if current == Some(lvl) {
                format!("{} (current)", thinking_level_description(lvl))
            } else {
                thinking_level_description(lvl).to_string()
            };
            SelectItem::new(key, key).with_description(desc)
        })
        .collect()
}

pub(super) fn theme_autocomplete_items(current_theme: Option<&str>) -> Vec<SelectItem> {
    let themes = match current_theme {
        Some(ct) => available_themes_with_current(ct),
        None => discover_available_themes(),
    };
    themes
        .into_iter()
        .map(|name| {
            let is_current = current_theme.is_some_and(|ct| ct.eq_ignore_ascii_case(&name));
            let desc = if is_current {
                format!("{} (current)", theme_description(&name))
            } else {
                theme_description(&name).to_string()
            };
            SelectItem::new(name.clone(), name).with_description(desc)
        })
        .collect()
}

pub(super) fn settings_key_autocomplete_items() -> Vec<SelectItem> {
    [
        ("permission_mode", "Approval policy (chat / approve / smart_approve / auto)"),
        ("default_thinking_level", "Thinking effort (off / low / medium / high)"),
        ("auto_compact", "Auto-compact context (on / off)"),
        ("theme", "Color theme (dark / light / custom)"),
        ("hide_thinking_block", "Thinking blocks (show / hide)"),
        ("steering_mode", "Mid-turn messages (one-at-a-time / all)"),
        ("follow_up_mode", "Follow-up queue (one-at-a-time / all)"),
    ]
    .iter()
    .map(|&(key, desc)| SelectItem::new(format!("{key} "), key).with_description(desc))
    .collect()
}

pub(super) fn settings_value_autocomplete_items(
    key: &str,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) -> Vec<SelectItem> {
    match canonical_setting_key(key) {
        Some("permission_mode") => mode_autocomplete_items(session),
        Some("default_thinking_level") => thinking_autocomplete_items(session),
        Some("theme") => theme_autocomplete_items(current_theme),
        Some("auto_compact") => {
            let cur = session.map(|s| s.settings.auto_compact.unwrap_or(true));
            [
                ("on", true, "Automatically summarize older history"),
                ("off", false, "Manual compaction only via /compact"),
            ]
            .iter()
            .map(|&(val, b, desc)| {
                let d = if cur == Some(b) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("hide_thinking_block") => {
            let cur = session.map(|s| s.settings.hide_thinking_block.unwrap_or(false));
            [
                ("show", false, "Render reasoning blocks in transcript"),
                ("hide", true, "Hide reasoning blocks from transcript"),
            ]
            .iter()
            .map(|&(val, b, desc)| {
                let d = if cur == Some(b) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("steering_mode") => {
            let cur = session.map(|s| s.settings.steering_mode.unwrap_or_default());
            [
                ("one-at-a-time", QueueMode::OneAtATime, "Deliver one steering message per step"),
                ("all", QueueMode::All, "Deliver all steering messages together"),
            ]
            .iter()
            .map(|&(val, m, desc)| {
                let d = if cur == Some(m) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("follow_up_mode") => {
            let cur = session.map(|s| s.settings.follow_up_mode.unwrap_or_default());
            [
                ("one-at-a-time", QueueMode::OneAtATime, "Run one follow-up per turn in order"),
                ("all", QueueMode::All, "Combine all follow-ups into next turn"),
            ]
            .iter()
            .map(|&(val, m, desc)| {
                let d = if cur == Some(m) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        _ => Vec::new(),
    }
}

pub(super) struct ArgAutocompletePlan {
    candidates: Vec<SelectItem>,
    prefix: String,
    replace_start: usize,
    is_settings_key: bool,
}

/// Resolves argument suggestions for slash commands whose options are known statically.
/// Returns `None` when the command should fall through to `@file` completion.
pub(super) fn slash_argument_candidates(
    cmd: &str,
    after_cmd: &str,
    total_chars: usize,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) -> Option<ArgAutocompletePlan> {
    let trimmed_after = after_cmd.trim_start();
    match cmd {
        "/settings" => {
            if let Some(split_idx) = trimmed_after.find(char::is_whitespace) {
                let key_tok = &trimmed_after[..split_idx];
                let after_key = &trimmed_after[split_idx..];
                let val_trimmed = after_key.trim_start();
                if val_trimmed.chars().any(char::is_whitespace) {
                    return Some(ArgAutocompletePlan {
                        candidates: Vec::new(),
                        prefix: String::new(),
                        replace_start: 0,
                        is_settings_key: false,
                    });
                }
                let replace_start = total_chars.saturating_sub(val_trimmed.chars().count());
                let items = settings_value_autocomplete_items(key_tok, session, current_theme);
                Some(ArgAutocompletePlan {
                    candidates: items,
                    prefix: val_trimmed.to_string(),
                    replace_start,
                    is_settings_key: false,
                })
            } else {
                let replace_start = total_chars.saturating_sub(trimmed_after.chars().count());
                Some(ArgAutocompletePlan {
                    candidates: settings_key_autocomplete_items(),
                    prefix: trimmed_after.to_string(),
                    replace_start,
                    is_settings_key: true,
                })
            }
        }
        "/mode" | "/thinking" | "/theme" | "/mcp" | "/connect" | "/login" | "/logout" => {
            if trimmed_after.chars().any(char::is_whitespace) {
                return Some(ArgAutocompletePlan {
                    candidates: Vec::new(),
                    prefix: String::new(),
                    replace_start: 0,
                    is_settings_key: false,
                });
            }
            let replace_start = total_chars.saturating_sub(trimmed_after.chars().count());
            let items = match cmd {
                "/mode" => mode_autocomplete_items(session),
                "/thinking" => thinking_autocomplete_items(session),
                "/theme" => theme_autocomplete_items(current_theme),
                "/mcp" => vec![
                    SelectItem::new("status", "status")
                        .with_description("Show MCP connection and tool counts"),
                    SelectItem::new("reconnect", "reconnect")
                        .with_description("Reconnect all or one MCP server"),
                    SelectItem::new("auth", "auth")
                        .with_description("Authenticate an MCP server via OAuth"),
                    SelectItem::new("resources", "resources")
                        .with_description("List resources exposed by MCP servers"),
                ],
                "/connect" | "/login" => mehh_ai::KNOWN_PROVIDERS
                    .iter()
                    .map(|spec| {
                        SelectItem::new(spec.id, spec.id).with_description(spec.display_name)
                    })
                    .collect(),
                "/logout" => mehh_ai::KNOWN_PROVIDERS
                    .iter()
                    .map(|spec| {
                        SelectItem::new(spec.id, spec.id)
                            .with_description("Remove stored credential")
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(ArgAutocompletePlan {
                candidates: items,
                prefix: trimmed_after.to_string(),
                replace_start,
                is_settings_key: false,
            })
        }
        _ => None,
    }
}

pub(super) fn filter_command_or_arg_candidates(
    candidates: Vec<SelectItem>,
    prefix: &str,
    match_description: bool,
) -> Vec<SelectItem> {
    if prefix.is_empty() {
        return candidates;
    }
    let lower_prefix = prefix.to_ascii_lowercase();
    let mut matched: Vec<SelectItem> = candidates
        .iter()
        .filter(|it| {
            it.id.trim().to_ascii_lowercase().starts_with(&lower_prefix)
                || it.label.to_ascii_lowercase().starts_with(&lower_prefix)
        })
        .cloned()
        .collect();
    for (_, item) in mehh_tui::fuzzy_filter_indexed(&candidates, prefix, |it| {
        if match_description {
            format!(
                "{} {} {}",
                it.id,
                it.label,
                it.description.as_deref().unwrap_or("")
            )
        } else {
            it.label.clone()
        }
    }) {
        if !matched.iter().any(|existing| existing.id == item.id) {
            matched.push(item.clone());
        }
    }
    matched
}

/// Refreshes the slash command and argument autocomplete popup with optional session context.
pub(in crate::interactive) fn update_autocomplete_with_context(
    editor: &mut Editor,
    current_text: &str,
    cwd: &Path,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) {
    let total_chars = current_text.chars().count();
    if current_text.starts_with('/')
        && !current_text.chars().any(char::is_whitespace)
        && editor.cursor_row == 0
        && editor.cursor_col == total_chars
    {
        if BUILTIN_SLASH_COMMANDS
            .iter()
            .any(|&(id, _)| id == current_text)
        {
            editor.clear_autocomplete();
            return;
        }

        let mut matches: Vec<SelectItem> = BUILTIN_SLASH_COMMANDS
            .iter()
            .filter(|&&(id, _)| id.starts_with(current_text))
            .map(|&(id, desc)| SelectItem::new(id, id).with_description(desc))
            .collect();

        if total_chars >= 2 {
            let query = current_text.trim_start_matches('/');
            for (_, &(id, desc)) in mehh_tui::fuzzy_filter_indexed(
                BUILTIN_SLASH_COMMANDS,
                query,
                |&(id, desc)| format!("{id} {desc}"),
            ) {
                if !matches.iter().any(|existing| existing.id == id) {
                    matches.push(SelectItem::new(id, id).with_description(desc));
                }
            }
        }

        if matches.is_empty() {
            editor.clear_autocomplete();
        } else {
            editor.set_autocomplete_replacing(matches, 0);
            if let Some(ref mut ac) = editor.autocomplete {
                ac.max_visible = 7;
            }
        }
        return;
    }

    if current_text.starts_with('/')
        && current_text.contains(' ')
        && editor.cursor_row == 0
        && editor.cursor_col == total_chars
    {
        if let Some(ws_pos) = current_text.find(char::is_whitespace) {
            let cmd = &current_text[..ws_pos];
            let after_cmd = &current_text[ws_pos..];
            if let Some(plan) = slash_argument_candidates(
                cmd,
                after_cmd,
                total_chars,
                session,
                current_theme,
            ) {
                let exact_match = !plan.prefix.is_empty()
                    && (plan
                        .candidates
                        .iter()
                        .any(|c| c.id.trim().eq_ignore_ascii_case(&plan.prefix))
                        || (plan.is_settings_key
                            && canonical_setting_key(&plan.prefix).is_some()));
                if exact_match {
                    editor.clear_autocomplete();
                    return;
                }
                let matches = filter_command_or_arg_candidates(
                    plan.candidates,
                    &plan.prefix,
                    !plan.is_settings_key,
                );
                if matches.is_empty() {
                    editor.clear_autocomplete();
                } else {
                    editor.set_autocomplete_replacing(matches, plan.replace_start);
                    if let Some(ref mut ac) = editor.autocomplete {
                        ac.max_visible = 7;
                    }
                }
                return;
            }
        }
    }

    let active_line = editor
        .lines
        .get(editor.cursor_row)
        .map(String::as_str)
        .unwrap_or("");
    if let Some(at_q) = crate::file_complete::extract_at_query(active_line, editor.cursor_col) {
        let normalized_query = at_q
            .query
            .trim_matches(|c| c == '"' || c == '\'')
            .replace('\\', "/");
        let matches = crate::file_complete::complete_file_paths(cwd, &normalized_query, 15);
        if matches
            .iter()
            .any(|rel| rel.eq_ignore_ascii_case(&normalized_query))
        {
            editor.clear_autocomplete();
        } else {
            let items: Vec<SelectItem> = matches
                .into_iter()
                .map(|rel| {
                    let value = if rel.chars().any(char::is_whitespace) {
                        format!("@\"{rel}\"")
                    } else {
                        format!("@{rel}")
                    };
                    SelectItem::new(value, rel)
                })
                .collect();
            if items.is_empty() {
                editor.clear_autocomplete();
            } else {
                editor.set_autocomplete_replacing(items, at_q.replace_start);
            }
        }
    } else {
        editor.clear_autocomplete();
    }
}

/// Refreshes the slash command autocomplete popup for the current editor text.
///
/// The popup displays the full command in the label while the id carries the text
/// inserted at the cursor, matching the file-completion path. A prefix that already
/// spells a command exactly leaves the popup closed, so `Enter` submits the command
/// instead of completing it a second time.
#[cfg(test)]
pub(in crate::interactive) fn update_autocomplete_with_cwd(editor: &mut Editor, current_text: &str, cwd: &Path) {
    update_autocomplete_with_context(editor, current_text, cwd, None, None);
}

