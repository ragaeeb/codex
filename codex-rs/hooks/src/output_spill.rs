use codex_protocol::ThreadId;
use codex_protocol::items::HookPromptFragment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::OutputArtifactStore;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text;
use tracing::warn;

const HOOK_OUTPUTS_DIR: &str = "hook_outputs";
pub(crate) const DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT: usize = 2_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdditionalContextLimit {
    token_limit: usize,
}

impl AdditionalContextLimit {
    pub(crate) fn from_config(value: Option<usize>) -> Self {
        Self {
            token_limit: value.unwrap_or(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT),
        }
    }
}

impl Default for AdditionalContextLimit {
    fn default() -> Self {
        Self::from_config(/*value*/ None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdditionalContext {
    pub text: String,
    pub limit: AdditionalContextLimit,
}

#[derive(Clone)]
pub(crate) struct HookOutputSpiller {
    output_dir: AbsolutePathBuf,
}

impl HookOutputSpiller {
    pub(crate) fn new(thread_id: ThreadId) -> Self {
        Self {
            output_dir: AbsolutePathBuf::resolve_path_against_base(std::env::temp_dir(), "/")
                .join(HOOK_OUTPUTS_DIR)
                .join(thread_id.to_string()),
        }
    }

    /// Keeps hook text within the model-visible hook-output budget.
    ///
    /// Oversized text is written in full under the OS temp directory at
    /// `<temp_dir>/hook_outputs/<thread_id>/`
    /// and replaced with the same head/tail preview style used for other truncated
    /// output, plus a path back to the preserved full text.
    pub(crate) async fn maybe_spill_text(&self, text: String) -> String {
        self.maybe_spill_text_with_limit(text, AdditionalContextLimit::default())
            .await
    }

    async fn maybe_spill_text_with_limit(
        &self,
        text: String,
        limit: AdditionalContextLimit,
    ) -> String {
        let token_limit = limit.token_limit;
        if token_limit == 0 || approx_token_count(&text) <= token_limit {
            return text;
        }

        match OutputArtifactStore::new(self.output_dir.clone())
            .store_text(&text)
            .await
        {
            Ok(artifact) => {
                spilled_hook_output_preview(&text, &artifact.diagnostic_path, token_limit)
            }
            Err(err) => {
                warn!(error_kind = ?err.kind(), "failed to spill hook output");
                formatted_truncate_text(&text, TruncationPolicy::Tokens(token_limit))
            }
        }
    }

    pub(crate) async fn maybe_spill_additional_contexts(
        &self,
        contexts: Vec<AdditionalContext>,
    ) -> Vec<String> {
        let mut spilled = Vec::with_capacity(contexts.len());
        for context in contexts {
            spilled.push(
                self.maybe_spill_text_with_limit(context.text, context.limit)
                    .await,
            );
        }
        spilled
    }

    pub(crate) async fn maybe_spill_prompt_fragments(
        &self,
        fragments: Vec<HookPromptFragment>,
    ) -> Vec<HookPromptFragment> {
        let mut spilled = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            spilled.push(HookPromptFragment {
                text: self.maybe_spill_text(fragment.text).await,
                hook_run_id: fragment.hook_run_id,
            });
        }
        spilled
    }
}

/// Builds the model-visible replacement for a spilled hook output.
///
/// The path footer is budgeted before truncation so adding the recovery path
/// does not consume the configured preview budget.
fn spilled_hook_output_preview(text: &str, path: &AbsolutePathBuf, token_limit: usize) -> String {
    let footer = format!("\n\nFull hook output saved to: {}", path.display());
    let preview_policy =
        TruncationPolicy::Tokens(token_limit.saturating_sub(approx_token_count(&footer)));
    format!("{}{footer}", formatted_truncate_text(text, preview_policy))
}

#[cfg(test)]
#[path = "output_spill_tests.rs"]
mod tests;
