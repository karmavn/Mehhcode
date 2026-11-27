use anyhow::{anyhow, Result};

use mehh_ai::{is_context_overflow, Message};

use super::{ActiveCancelGuard, AgentSession};

impl AgentSession {
    /// Prepares proactive auto-compaction metadata synchronously when threshold is reached.
    pub fn prepare_proactive_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            auto_compact_enabled,
        );
        let current_tokens =
            crate::compaction::estimate_context_tokens(&self.session.build_context_messages())
                + self.cached_prompt_and_tools_tokens;
        if !crate::compaction::should_auto_compact(
            current_tokens,
            usize::try_from(self.model.context_window).unwrap_or(usize::MAX),
            &comp_settings,
        ) {
            return None;
        }
        let active_entries = self.session.active_path_entries();
        crate::compaction::prepare_compaction_refs(
            &active_entries,
            comp_settings.keep_recent_tokens,
        )
    }

    /// Prepares emergency overflow compaction metadata synchronously (falling back to 1024 recent tokens).
    pub fn prepare_overflow_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        if !self.settings.auto_compact.unwrap_or(true) {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        );
        let active_entries = self.session.active_path_entries();
        crate::compaction::prepare_compaction_refs(
            &active_entries,
            comp_settings.keep_recent_tokens,
        )
        .or_else(|| crate::compaction::prepare_compaction_refs(&active_entries, 1024))
    }

    /// Persists a completed compaction entry into the active session branch.
    pub fn apply_prepared_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
    ) -> Result<()> {
        self.session
            .append_entry(crate::session::SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            })?;
        Ok(())
    }

    /// Runs proactive auto-compaction before a turn when enabled (default `true`).
    pub async fn run_proactive_compaction(&mut self) {
        if let Some(prep) = self.prepare_proactive_compaction() {
            if let Err(e) = self.run_prepared_compaction(prep, None).await {
                eprintln!("Warning: proactive auto-compaction failed: {e}");
            }
        }
    }

    /// Returns true if `messages` ended due to context overflow and auto-compaction is enabled.
    pub fn is_overflow_turn(&self, messages: &[Message]) -> bool {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return false;
        }
        messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(ast) => Some(ast),
                _ => None,
            })
            .is_some_and(|ast| {
                let context_window = {
                    let lw = mehh_ai::lock_recover(&self.lead_worker);
                    if let Some(worker) = lw
                        .worker_model()
                        .filter(|w| w.id == ast.model && w.provider == ast.provider)
                    {
                        worker.context_window
                    } else if self.model.id == ast.model && self.model.provider == ast.provider {
                        self.model.context_window
                    } else if let Some(worker) = lw.worker_model().filter(|w| w.id == ast.model) {
                        worker.context_window
                    } else {
                        self.model.context_window
                    }
                };
                is_context_overflow(ast, context_window)
            })
    }

    /// Attempts emergency compaction after a context overflow turn. Propagates provider/cancel errors.
    pub async fn try_overflow_compaction(&mut self) -> Result<bool> {
        let Some(prep) = self.prepare_overflow_compaction() else {
            return Ok(false);
        };
        self.run_prepared_compaction(prep, None).await?;
        Ok(true)
    }

    /// Manually triggers compaction on the active branch history, saving the summary entry.
    pub async fn compact(&mut self, custom_instructions: Option<&str>) -> Result<Option<String>> {
        let (cancel, mut guard) = ActiveCancelGuard::acquire(self.active_cancel.clone());

        let auth_res = tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Operation aborted"),
            res = self.ensure_api_key(None) => res,
        };
        auth_res?;
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }

        let keep = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        )
        .keep_recent_tokens;
        let res = self.compact_with_budget(custom_instructions, keep).await?;
        guard.disarm_and_clear();
        Ok(res)
    }

    async fn compact_with_budget(
        &mut self,
        custom_instructions: Option<&str>,
        keep_recent_tokens: usize,
    ) -> Result<Option<String>> {
        let active_entries = self.session.active_path_entries();

        let prep =
            match crate::compaction::prepare_compaction_refs(&active_entries, keep_recent_tokens) {
            Some(p) => p,
            None => return Ok(None),
        };
        let summary = self
            .run_prepared_compaction(prep, custom_instructions)
            .await?;
        Ok(Some(summary))
    }

    async fn run_prepared_compaction(
        &mut self,
        prep: crate::compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
    ) -> Result<String> {
        let (cancel, mut guard) = ActiveCancelGuard::acquire(self.active_cancel.clone());
        let summary = crate::compaction::compact_with_provider(
            &prep,
            &self.model,
            self.provider.as_ref(),
            self.api_key.as_deref(),
            custom_instructions,
            cancel,
        )
        .await?;
        guard.disarm_and_clear();
        self.apply_prepared_compaction(
            summary.clone(),
            prep.first_kept_entry_id,
            prep.tokens_before,
        )?;
        Ok(summary)
    }

    /// Navigates to a target entry in the session tree, optionally summarizing the abandoned branch.
    pub async fn navigate_tree(
        &mut self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<Option<String>> {
        let old_leaf_id = self.session.leaf_id().map(str::to_string);

        if self.session.get_entry(target_id).is_none() {
            return Err(anyhow!("Target entry ID not found: {target_id}"));
        }

        let summary = if summarize {
            let collected = crate::branch_summarization::collect_entries_for_branch_summary(
                &self.session,
                old_leaf_id.as_deref(),
                target_id,
            );
            if !collected.entries.is_empty() {
                let (cancel, mut guard) = ActiveCancelGuard::acquire(self.active_cancel.clone());

                let auth_res = tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("Operation aborted"),
                    res = self.ensure_api_key(None) => res,
                };
                auth_res?;
                if cancel.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }

                let sum = crate::branch_summarization::generate_branch_summary(
                    &collected.entries,
                    &self.model,
                    self.provider.as_ref(),
                    self.api_key.as_deref(),
                    custom_instructions,
                    None,
                    cancel,
                )
                .await?;
                guard.disarm_and_clear();

                // Switch branch to target_id first
                self.session.branch_from(target_id)?;
                // Append BranchSummary on top of target branch
                let from_id = old_leaf_id.unwrap_or_else(|| "root".to_string());
                self.session
                    .append_entry(crate::session::SessionEntryKind::BranchSummary {
                        from_id,
                        summary: sum.clone(),
                    })?;
                Some(sum)
            } else {
                self.session.branch_from(target_id)?;
                None
            }
        } else {
            self.session.branch_from(target_id)?;
            None
        };

        Ok(summary)
    }
}
