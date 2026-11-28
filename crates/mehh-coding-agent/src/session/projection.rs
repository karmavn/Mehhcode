use std::collections::{HashMap, HashSet};

use mehh_ai::{Message, SystemMessage, UserMessage};

use super::types::{
    parse_iso8601_or_now, ContextEditReplacement, RpcSessionTreeNode, SessionEntry,
    SessionEntryKind, SessionTreeNode,
};
use super::SessionManager;

impl SessionManager {
    /// Collects active path entry indices from root to `start_id` with cycle protection.
    pub(super) fn collect_path_indices(&self, start_id: Option<&str>) -> Vec<usize> {
        let mut path_indices = Vec::new();
        let mut visited = HashSet::new();
        let mut curr = start_id;

        while let Some(id) = curr {
            if let Some(&idx) = self.by_id.get(id) {
                if !visited.insert(idx) {
                    break;
                }
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();
        path_indices
    }

    /// Projects active messages from `leaf_id` back to root, taking compactions into account.
    pub fn build_context_messages(&self) -> Vec<Message> {
        let path_indices = self.collect_path_indices(self.leaf_id.as_deref());

        // Collect context edits along the active path
        let mut edits: HashMap<&str, &Option<ContextEditReplacement>> = HashMap::new();
        for &idx in &path_indices {
            if let SessionEntryKind::ContextEdit {
                target_id,
                replacement,
            } = &self.entries[idx].kind
            {
                edits.insert(target_id.as_str(), replacement);
            }
        }

        let mut edited_removed_result_ids: HashSet<String> = HashSet::new();
        let project_entry = |entry: &SessionEntry,
                             removed_results: &mut HashSet<String>|
         -> Option<Message> {
            if let Some(edit) = edits.get(entry.id.as_str()) {
                if edit.is_none() {
                    // Omitted from context; track removed ToolResult IDs so matching ToolCalls are cleaned up.
                    if let SessionEntryKind::Message {
                        message: Message::ToolResult(tr),
                    } = &entry.kind
                    {
                        removed_results.insert(tr.tool_call_id.clone());
                    }
                    return None;
                }
            }
            let replacement = edits.get(entry.id.as_str()).and_then(|r| r.as_ref());

            match &entry.kind {
                SessionEntryKind::Message { message } => {
                    if let Some(rep) = replacement {
                        let mut msg = message.clone();
                        match &mut msg {
                            Message::User(u) => {
                                u.content = rep.content.to_user_content();
                            }
                            Message::Assistant(a) => {
                                a.content = rep.content.to_assistant_content();
                            }
                            Message::ToolResult(tr) => {
                                tr.content = rep.content.to_tool_result_content();
                            }
                            Message::System(s) => {
                                s.content = rep.content.to_text();
                            }
                        }
                        Some(msg)
                    } else {
                        Some(message.clone())
                    }
                }
                SessionEntryKind::CustomMessage { content, .. } => {
                    let effective_content = if let Some(rep) = replacement {
                        rep.content.to_user_content()
                    } else {
                        content.clone()
                    };
                    let ts = parse_iso8601_or_now(&entry.timestamp);
                    Some(Message::User(UserMessage {
                        content: effective_content,
                        timestamp: ts,
                    }))
                }
                SessionEntryKind::BranchSummary { summary, .. } => {
                    const BRANCH_SUMMARY_PREFIX: &str =
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
                    const BRANCH_SUMMARY_SUFFIX: &str = "\n</summary>";
                    let ts = parse_iso8601_or_now(&entry.timestamp);
                    Some(Message::User(UserMessage {
                        content: mehh_ai::UserContent::Text(format!(
                            "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
                        )),
                        timestamp: ts,
                    }))
                }
                _ => None,
            }
        };

        // Check if there is a Compaction entry on the active path
        let last_compaction_pos = path_indices.iter().rposition(|&idx| {
            matches!(&self.entries[idx].kind, SessionEntryKind::Compaction { .. })
        });

        let mut messages = if let Some(comp_pos) = last_compaction_pos {
            let comp_idx = path_indices[comp_pos];
            if let SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } = &self.entries[comp_idx].kind
            {
                let comp_ts = parse_iso8601_or_now(&self.entries[comp_idx].timestamp);

                // Find first kept entry on active path before compaction
                let first_kept_pos = path_indices[..comp_pos]
                    .iter()
                    .position(|&idx| self.entries[idx].id == *first_kept_entry_id)
                    .unwrap_or(0);

                let cap = 1 + (comp_pos - first_kept_pos) + (path_indices.len() - comp_pos - 1);
                let mut msgs = Vec::with_capacity(cap);
                msgs.push(Message::System(SystemMessage {
                    content: summary.clone(),
                    timestamp: comp_ts,
                }));

                for &idx in &path_indices[first_kept_pos..comp_pos] {
                    if let Some(m) = project_entry(
                        &self.entries[idx],
                        &mut edited_removed_result_ids,
                    ) {
                        msgs.push(m);
                    }
                }

                for &idx in &path_indices[comp_pos + 1..] {
                    if let Some(m) = project_entry(
                        &self.entries[idx],
                        &mut edited_removed_result_ids,
                    ) {
                        msgs.push(m);
                    }
                }

                msgs
            } else {
                Vec::new()
            }
        } else {
            let mut msgs = Vec::with_capacity(path_indices.len());
            for &idx in &path_indices {
                if let Some(m) = project_entry(
                    &self.entries[idx],
                    &mut edited_removed_result_ids,
                ) {
                    msgs.push(m);
                }
            }
            msgs
        };

        if !edits.is_empty() {
            clean_orphaned_tool_pairs(&mut messages, &edited_removed_result_ids);
        }

        if let Some(comp_idx) = last_compaction_pos.map(|pos| path_indices[pos]) {
            if let Some(first_non_sys_idx) =
                messages.iter().position(|m| !matches!(m, Message::System(_)))
            {
                if matches!(messages[first_non_sys_idx], Message::Assistant(_)) {
                    let bridge_ts = parse_iso8601_or_now(&self.entries[comp_idx].timestamp);
                    messages.insert(
                        first_non_sys_idx,
                        Message::User(UserMessage {
                            content: mehh_ai::UserContent::Text(
                                "Continuing from compacted conversation summary above."
                                    .to_string(),
                            ),
                            timestamp: bridge_ts,
                        }),
                    );
                }
            }
        }

        messages
    }

    /// Returns the entries along the active branch from root to the current leaf.
    pub fn active_path_entries(&self) -> Vec<&SessionEntry> {
        self.collect_path_indices(self.leaf_id.as_deref())
            .into_iter()
            .map(|idx| &self.entries[idx])
            .collect()
    }

    /// Builds an iterative, cycle-safe DFS tree projection of session entries.
    pub fn build_tree(&self) -> Vec<SessionTreeNode> {
        let active_entries = self.active_path_entries();
        let active_set: HashSet<&str> = active_entries.iter().map(|e| e.id.as_str()).collect();

        let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            let parent_key = entry
                .parent_id
                .as_deref()
                .filter(|p| self.by_id.contains_key(*p));
            children.entry(parent_key).or_default().push(idx);
        }

        let mut tree = Vec::new();
        let mut visited = vec![false; self.entries.len()];
        let mut stack: Vec<(usize, usize)> = Vec::new();

        if let Some(root_indices) = children.get(&None) {
            for &idx in root_indices.iter().rev() {
                stack.push((idx, 0));
            }
        }

        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;

            let entry = &self.entries[idx];
            let is_active = active_set.contains(entry.id.as_str());
            let is_leaf = self.leaf_id.as_deref() == Some(entry.id.as_str());
            let is_user_or_branch = matches!(
                &entry.kind,
                SessionEntryKind::Message {
                    message: Message::User(_)
                } | SessionEntryKind::BranchSummary { .. }
                    | SessionEntryKind::Compaction { .. }
            );

            tree.push(SessionTreeNode {
                entry_id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
                depth,
                label: entry_to_label(entry),
                timestamp: entry.timestamp.clone(),
                is_active,
                is_leaf,
                is_user_or_branch,
            });

            if let Some(child_indices) = children.get(&Some(entry.id.as_str())) {
                for &child_idx in child_indices.iter().rev() {
                    if !visited[child_idx] {
                        stack.push((child_idx, depth + 1));
                    }
                }
            }
        }

        tree
    }

    /// Builds a nested RPC session tree iteratively with cycle protection and resolved labels.
    pub fn build_rpc_tree(&self) -> Vec<RpcSessionTreeNode> {
        let mut labels_by_target: HashMap<String, Option<String>> = HashMap::new();
        for entry in &self.entries {
            if let SessionEntryKind::Label { target_id, label } = &entry.kind {
                labels_by_target.insert(target_id.clone(), label.clone());
            }
        }

        let mut children_map: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            let p = entry
                .parent_id
                .as_deref()
                .filter(|pid| self.by_id.contains_key(*pid));
            children_map.entry(p).or_default().push(idx);
        }

        enum Frame {
            Enter(usize, Option<usize>),
            Exit(usize, Option<usize>),
        }

        let mut visited = vec![false; self.entries.len()];
        let mut children_slots: Vec<Vec<RpcSessionTreeNode>> = vec![Vec::new(); self.entries.len()];
        let mut roots = Vec::new();
        let mut stack = Vec::new();

        if let Some(root_indices) = children_map.get(&None) {
            for &idx in root_indices.iter().rev() {
                stack.push(Frame::Enter(idx, None));
            }
        }

        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(idx, parent_idx) => {
                    if visited[idx] {
                        continue;
                    }
                    visited[idx] = true;
                    stack.push(Frame::Exit(idx, parent_idx));
                    if let Some(child_indices) =
                        children_map.get(&Some(self.entries[idx].id.as_str()))
                    {
                        for &child_idx in child_indices.iter().rev() {
                            if !visited[child_idx] {
                                stack.push(Frame::Enter(child_idx, Some(idx)));
                            }
                        }
                    }
                }
                Frame::Exit(idx, parent_idx) => {
                    let entry = &self.entries[idx];
                    let labels = match labels_by_target.get(&entry.id) {
                        Some(Some(lbl)) if !lbl.is_empty() => vec![lbl.clone()],
                        _ => Vec::new(),
                    };
                    let node = RpcSessionTreeNode {
                        id: entry.id.clone(),
                        parent_id: entry.parent_id.clone(),
                        entry: entry.kind.clone(),
                        labels,
                        children: std::mem::take(&mut children_slots[idx]),
                    };
                    match parent_idx {
                        Some(p) => children_slots[p].push(node),
                        None => roots.push(node),
                    }
                }
            }
        }

        roots
    }
}

/// Removes orphaned `ToolResult` messages (whose matching `ToolCall` was hidden or edited out)
/// and orphaned `ToolCall` blocks (whose `ToolResult` was explicitly hidden by `ContextEdit`).
fn clean_orphaned_tool_pairs(
    messages: &mut Vec<Message>,
    edited_removed_result_ids: &HashSet<String>,
) {
    let mut present_call_ids: HashSet<String> = HashSet::with_capacity(messages.len());
    let mut present_result_ids: HashSet<String> = HashSet::with_capacity(messages.len());

    for msg in messages.iter() {
        match msg {
            Message::Assistant(a) => {
                for b in &a.content {
                    if let mehh_ai::AssistantContentBlock::ToolCall(tc) = b {
                        present_call_ids.insert(tc.id.clone());
                    }
                }
            }
            Message::ToolResult(tr) => {
                present_result_ids.insert(tr.tool_call_id.clone());
            }
            _ => {}
        }
    }

    messages.retain_mut(|msg| match msg {
        Message::ToolResult(tr) => present_call_ids.contains(&tr.tool_call_id),
        Message::Assistant(a) if !edited_removed_result_ids.is_empty() => {
            a.content.retain(|block| match block {
                mehh_ai::AssistantContentBlock::ToolCall(tc) => {
                    !edited_removed_result_ids.contains(&tc.id)
                        || present_result_ids.contains(&tc.id)
                }
                _ => true,
            });
            !a.content.is_empty()
        }
        _ => true,
    });
}

fn truncate_chars(s: &str, max_chars: usize, keep_chars: usize) -> Option<&str> {
    if s.len() <= max_chars {
        return None;
    }
    let mut keep_byte_end: Option<usize> = (keep_chars == 0).then_some(0);
    for (count, (idx, _)) in s.char_indices().enumerate() {
        if count == keep_chars {
            keep_byte_end = Some(idx);
        }
        if count == max_chars {
            let end = keep_byte_end.unwrap_or(s.len());
            return s.get(..end);
        }
    }
    None
}

fn entry_to_label(entry: &SessionEntry) -> String {
    match &entry.kind {
        SessionEntryKind::Message { message } => match message {
            Message::User(u) => {
                let txt = u.content.text();
                let line = txt.lines().next().unwrap_or("").trim();
                if let Some(prefix) = truncate_chars(line, 40, 37) {
                    format!("User: {prefix}...")
                } else if line.is_empty() {
                    "User".to_string()
                } else {
                    format!("User: {line}")
                }
            }
            Message::Assistant(a) => {
                let txt = a.text_content();
                let line = txt.lines().next().unwrap_or("").trim();
                if let Some(prefix) = truncate_chars(line, 40, 37) {
                    format!("Assistant: {prefix}...")
                } else if line.is_empty() {
                    let first_tc = a.content.iter().find_map(|b| match b {
                        mehh_ai::AssistantContentBlock::ToolCall(tc) => Some(tc),
                        _ => None,
                    });
                    if let Some(tc) = first_tc {
                        format!("Assistant: tool call {}", tc.name)
                    } else {
                        "Assistant".to_string()
                    }
                } else {
                    format!("Assistant: {line}")
                }
            }
            Message::ToolResult(tr) => format!("ToolResult: {}", tr.tool_name),
            Message::System(s) => {
                let line = s.content.lines().next().unwrap_or("").trim();
                if let Some(prefix) = truncate_chars(line, 40, 37) {
                    format!("System: {prefix}...")
                } else {
                    format!("System: {line}")
                }
            }
        },
        SessionEntryKind::ThinkingLevelChange { thinking_level } => {
            format!("Thinking: {:?}", thinking_level)
        }
        SessionEntryKind::ModelChange { model_id, .. } => format!("Model: {model_id}"),
        SessionEntryKind::Compaction { summary, .. } => {
            let line = summary.lines().next().unwrap_or("").trim();
            if let Some(prefix) = truncate_chars(line, 40, 37) {
                format!("Compaction: {prefix}...")
            } else {
                format!("Compaction: {line}")
            }
        }
        SessionEntryKind::BranchSummary { summary, .. } => {
            let line = summary.lines().next().unwrap_or("").trim();
            if let Some(prefix) = truncate_chars(line, 40, 37) {
                format!("Branch: {prefix}...")
            } else {
                format!("Branch: {line}")
            }
        }
        SessionEntryKind::SessionInfo { name } => {
            format!("Session: {}", name.as_deref().unwrap_or("unnamed"))
        }
        SessionEntryKind::Custom { custom_type, .. } => format!("Custom: {custom_type}"),
        SessionEntryKind::Usage { kind, model, .. } => {
            let k = kind.as_deref().unwrap_or("usage");
            let m = model.as_deref().unwrap_or("");
            if m.is_empty() {
                format!("Usage: {k}")
            } else {
                format!("Usage: {k} ({m})")
            }
        }
        SessionEntryKind::CustomMessage {
            custom_type,
            content,
            ..
        } => {
            let txt = content.text();
            let line = txt.lines().next().unwrap_or("").trim();
            if let Some(prefix) = truncate_chars(line, 40, 30) {
                format!("CustomMessage [{custom_type}]: {prefix}...")
            } else if line.is_empty() {
                format!("CustomMessage [{custom_type}]")
            } else {
                format!("CustomMessage [{custom_type}]: {line}")
            }
        }
        SessionEntryKind::ContextEdit {
            target_id,
            replacement,
        } => {
            if replacement.is_some() {
                format!("ContextEdit: replace {target_id}")
            } else {
                format!("ContextEdit: hide {target_id}")
            }
        }
        SessionEntryKind::Label { target_id, label } => {
            if let Some(lbl) = label {
                format!("Label: {lbl} -> {target_id}")
            } else {
                format!("Label: clear -> {target_id}")
            }
        }
    }
}
