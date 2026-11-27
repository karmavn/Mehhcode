use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use mehh_ai::{now_ms, Message, ThinkingLevel};

/// Generates an ISO 8601 UTC timestamp string (e.g. `2026-11-12T13:17:17.000Z`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let days = (total_secs / 86400) as i64;
    let day_secs = (total_secs % 86400) as u32;
    let hours = day_secs / 3600;
    let mins = (day_secs % 3600) / 60;
    let secs = day_secs % 60;

    // Euclidean affine transform algorithm (Howard Hinnant)
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{mins:02}:{secs:02}.{millis:03}Z")
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Parses an ISO 8601 UTC string to unix timestamp in milliseconds, falling back to current time.
pub fn parse_iso8601_or_now(s: &str) -> i64 {
    let clean = s.trim().trim_end_matches('Z');
    let Some((date_part, time_part)) = clean.split_once('T') else {
        return now_ms();
    };
    if time_part.contains('T') {
        return now_ms();
    }

    let mut date_iter = date_part.split('-');
    let (Some(ys), Some(ms_s), Some(ds), None) = (
        date_iter.next(),
        date_iter.next(),
        date_iter.next(),
        date_iter.next(),
    ) else {
        return now_ms();
    };

    let mut time_iter = time_part.split(':');
    let (Some(hs), Some(mins_s), Some(sec_part)) =
        (time_iter.next(), time_iter.next(), time_iter.next())
    else {
        return now_ms();
    };

    let Ok(y) = ys.parse::<i64>() else {
        return now_ms();
    };
    let Ok(m) = ms_s.parse::<i64>() else {
        return now_ms();
    };
    let Ok(d) = ds.parse::<i64>() else {
        return now_ms();
    };
    let Ok(h) = hs.parse::<i64>() else {
        return now_ms();
    };
    let Ok(min) = mins_s.parse::<i64>() else {
        return now_ms();
    };

    let (sec_str, frac_opt) = match sec_part.split_once('.') {
        Some((sec_s, frac_and_more)) => {
            let frac = frac_and_more.split('.').next().unwrap_or("");
            (sec_s, Some(frac))
        }
        None => (sec_part, None),
    };
    let Ok(sec) = sec_str.parse::<i64>() else {
        return now_ms();
    };
    let ms: i64 = if let Some(frac) = frac_opt {
        let end = frac
            .as_bytes()
            .iter()
            .take(3)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if end == 0 {
            0
        } else {
            let val = frac[..end].parse::<i64>().unwrap_or(0);
            match end {
                1 => val * 100,
                2 => val * 10,
                _ => val,
            }
        }
    } else {
        0
    };

    // Reject values that the civil-date algorithm would otherwise silently
    // normalize into a different valid date (e.g. Feb 30 -> Mar 2).
    if !(1..=12).contains(&m)
        || d < 1
        || d > days_in_month(y, m)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&min)
        || !(0..=59).contains(&sec)
    {
        return now_ms();
    }

    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u32;
    let days = era * 146097 + doe as i64 - 719468;

    let total_secs = days * 86400 + h * 3600 + min * 60 + sec;
    total_secs * 1000 + ms
}

/// JSONL v3 header record written on line 1 of every session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(
        rename = "parentSession",
        alias = "parent_session",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
}

/// Content that an append-only context edit may replace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextEditableContent {
    Text(String),
    ToolResult(Vec<mehh_ai::ToolResultContentBlock>),
    User(Vec<mehh_ai::UserContentBlock>),
    Assistant(Vec<mehh_ai::AssistantContentBlock>),
    Value(serde_json::Value),
}

impl ContextEditableContent {
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::ToolResult(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::User(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::UserContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Assistant(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::AssistantContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Value(v) => {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            }
        }
    }

    pub fn to_user_content(&self) -> mehh_ai::UserContent {
        match self {
            Self::Text(s) => mehh_ai::UserContent::Text(s.clone()),
            Self::User(blocks) => mehh_ai::UserContent::Blocks(blocks.clone()),
            Self::ToolResult(blocks) => {
                let user_blocks = blocks
                    .iter()
                    .map(|b| match b {
                        mehh_ai::ToolResultContentBlock::Text(t) => {
                            mehh_ai::UserContentBlock::Text(t.clone())
                        }
                        mehh_ai::ToolResultContentBlock::Image(i) => {
                            mehh_ai::UserContentBlock::Image(i.clone())
                        }
                    })
                    .collect();
                mehh_ai::UserContent::Blocks(user_blocks)
            }
            Self::Assistant(_) => {
                let text = self.to_text();
                mehh_ai::UserContent::Text(text)
            }
            Self::Value(v) => {
                if let Ok(blocks) =
                    serde_json::from_value::<Vec<mehh_ai::UserContentBlock>>(v.clone())
                {
                    mehh_ai::UserContent::Blocks(blocks)
                } else if let Some(s) = v.as_str() {
                    mehh_ai::UserContent::Text(s.to_string())
                } else {
                    mehh_ai::UserContent::Text(v.to_string())
                }
            }
        }
    }

    pub fn to_assistant_content(&self) -> Vec<mehh_ai::AssistantContentBlock> {
        match self {
            Self::Assistant(blocks) => blocks.clone(),
            Self::Text(s) => vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                text: s.clone(),
                text_signature: None,
            })],
            _ => vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                text: self.to_text(),
                text_signature: None,
            })],
        }
    }

    pub fn to_tool_result_content(&self) -> Vec<mehh_ai::ToolResultContentBlock> {
        match self {
            Self::ToolResult(blocks) => blocks.clone(),
            Self::Text(s) => vec![mehh_ai::ToolResultContentBlock::Text(
                mehh_ai::TextContent {
                    text: s.clone(),
                    text_signature: None,
                },
            )],
            _ => vec![mehh_ai::ToolResultContentBlock::Text(
                mehh_ai::TextContent {
                    text: self.to_text(),
                    text_signature: None,
                },
            )],
        }
    }
}

/// Replacement payload for a context edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEditReplacement {
    pub content: ContextEditableContent,
}

/// Variant payload of a session entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryKind {
    Message {
        message: Message,
    },
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkingLevel,
    },
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Usage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        usage: mehh_ai::Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
    },
    SessionInfo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    CustomMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<bool>,
        #[serde(rename = "customType")]
        custom_type: String,
        content: mehh_ai::UserContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    ContextEdit {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement: Option<ContextEditReplacement>,
    },
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// An entry in a session tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: SessionEntryKind,
}

/// Node representing a session entry in a tree projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeNode {
    pub entry_id: String,
    pub parent_id: Option<String>,
    pub depth: usize,
    pub label: String,
    pub timestamp: String,
    pub is_active: bool,
    pub is_leaf: bool,
    pub is_user_or_branch: bool,
}

/// Recursive node representing a session entry in an RPC tree projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionTreeNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub entry: SessionEntryKind,
    pub labels: Vec<String>,
    pub children: Vec<RpcSessionTreeNode>,
}

/// Summary of a stored session on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub entry_count: usize,
    pub name: Option<String>,
    pub first_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PersistedBranchSelection {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "leafId")]
    leaf_id: String,
}

impl PersistedBranchSelection {
    pub(super) fn new(leaf_id: &str) -> Self {
        Self {
            kind: "branch_selection".to_string(),
            leaf_id: leaf_id.to_string(),
        }
    }

    pub(super) fn parse(line: &str) -> Option<String> {
        if !line.contains("branch_selection") {
            return None;
        }
        let parsed: Self = serde_json::from_str(line).ok()?;
        (parsed.kind == "branch_selection" && !parsed.leaf_id.is_empty()).then_some(parsed.leaf_id)
    }
}
