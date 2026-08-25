use std::num::NonZeroUsize;

use codex_protocol::protocol::SkillScope;
use codex_utils_string::approx_token_count;

use crate::catalog::SkillCatalogEntry;

const DEFAULT_SKILL_METADATA_CHAR_BUDGET: usize = 8_000;
const MAX_CONFIGURED_SKILL_METADATA_TOKEN_BUDGET: usize = 10_000;
const SKILL_METADATA_CONTEXT_WINDOW_PERCENT: usize = 2;
const SKILL_DESCRIPTION_TRUNCATION_WARNING_THRESHOLD_CHARS: usize = 100;
const APPROX_BYTES_PER_TOKEN: usize = 4;
pub(crate) const SKILL_DESCRIPTION_TRUNCATED_WARNING: &str = "Skill descriptions were shortened to fit the skills context budget. Codex can still see every skill, but some descriptions are shorter. Disable unused skills or plugins to leave more room for the rest.";
const SKILL_DESCRIPTIONS_REMOVED_WARNING_PREFIX: &str =
    "Exceeded skills context budget. All skill descriptions were removed and";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillCatalogRenderPolicy {
    CoreCompatible,
    ExtensionCompatible,
    StableCompact,
}

impl SkillCatalogRenderPolicy {
    pub(crate) fn description(self, entry: &SkillCatalogEntry) -> &str {
        match self {
            Self::CoreCompatible => entry.description.as_str(),
            Self::ExtensionCompatible | Self::StableCompact => entry
                .short_description
                .as_deref()
                .unwrap_or(entry.description.as_str()),
        }
    }

    pub(crate) fn order_entries(self, entries: &mut [&SkillCatalogEntry]) {
        match self {
            Self::CoreCompatible | Self::StableCompact => {
                let scope_rank = |entry: &SkillCatalogEntry| match entry.prompt_scope() {
                    Some(SkillScope::System) => 0,
                    Some(SkillScope::Admin) => 1,
                    Some(SkillScope::Repo) => 2,
                    Some(SkillScope::User) => 3,
                    None => 4,
                };
                entries.sort_by(|a, b| {
                    scope_rank(a)
                        .cmp(&scope_rank(b))
                        .then_with(|| a.name.cmp(&b.name))
                        .then_with(|| a.main_prompt.as_str().cmp(b.main_prompt.as_str()))
                });
            }
            Self::ExtensionCompatible => {}
        }
    }

    pub(crate) fn includes_omission_notice(self) -> bool {
        match self {
            Self::CoreCompatible => false,
            Self::ExtensionCompatible | Self::StableCompact => true,
        }
    }

    pub(crate) fn outcome(self) -> SkillCatalogRenderOutcome {
        match self {
            Self::CoreCompatible | Self::ExtensionCompatible => SkillCatalogRenderOutcome::Legacy,
            Self::StableCompact => SkillCatalogRenderOutcome::Compact,
        }
    }
}

pub(crate) fn catalog_render_policy(stable_compact: bool) -> SkillCatalogRenderPolicy {
    if stable_compact {
        SkillCatalogRenderPolicy::StableCompact
    } else {
        SkillCatalogRenderPolicy::ExtensionCompatible
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillCatalogRenderOutcome {
    Legacy,
    Compact,
    Fallback,
}

impl SkillCatalogRenderOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Compact => "compact",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillMetadataBudget {
    Tokens(usize),
    Characters(usize),
}

impl SkillMetadataBudget {
    pub(crate) fn limit(self) -> usize {
        match self {
            Self::Tokens(limit) | Self::Characters(limit) => limit,
        }
    }

    pub(crate) fn cost_from_counts(self, chars: usize, bytes: usize) -> usize {
        match self {
            Self::Tokens(_) => {
                bytes.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1))
                    / APPROX_BYTES_PER_TOKEN
            }
            Self::Characters(_) => chars,
        }
    }

    pub(crate) fn cost(self, text: &str) -> usize {
        match self {
            Self::Tokens(_) => approx_token_count(text),
            Self::Characters(_) => text.chars().count(),
        }
    }
}

pub(crate) fn metadata_line_cost(budget: SkillMetadataBudget, line: &str) -> usize {
    let line = format!("{line}\n");
    match budget {
        SkillMetadataBudget::Tokens(_) => approx_token_count(&line),
        SkillMetadataBudget::Characters(_) => line.chars().count(),
    }
}

pub(crate) fn skill_metadata_budget(
    context_window: Option<i64>,
    max_context_tokens: Option<NonZeroUsize>,
) -> SkillMetadataBudget {
    if let Some(max_context_tokens) = max_context_tokens {
        return SkillMetadataBudget::Tokens(
            max_context_tokens
                .get()
                .min(MAX_CONFIGURED_SKILL_METADATA_TOKEN_BUDGET),
        );
    }

    context_window
        .and_then(|window| usize::try_from(window).ok())
        .filter(|window| *window > 0)
        .map(|window| {
            SkillMetadataBudget::Tokens(
                window
                    .saturating_mul(SKILL_METADATA_CONTEXT_WINDOW_PERCENT)
                    .saturating_div(100)
                    .max(1),
            )
        })
        .unwrap_or(SkillMetadataBudget::Characters(
            DEFAULT_SKILL_METADATA_CHAR_BUDGET,
        ))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SkillRenderReport {
    pub(crate) total_count: usize,
    pub(crate) included_count: usize,
    pub(crate) omitted_count: usize,
    pub(crate) truncated_description_chars: usize,
    pub(crate) truncated_description_count: usize,
}

impl SkillRenderReport {
    pub(crate) fn warning_message(&self) -> Option<String> {
        if self.omitted_count > 0 {
            let skill_word = if self.omitted_count == 1 {
                "skill"
            } else {
                "skills"
            };
            let verb = if self.omitted_count == 1 {
                "was"
            } else {
                "were"
            };
            return Some(format!(
                "{} {} additional {} {} not included in the model-visible skills list.",
                SKILL_DESCRIPTIONS_REMOVED_WARNING_PREFIX, self.omitted_count, skill_word, verb
            ));
        }

        (self.average_truncated_description_chars()
            > SKILL_DESCRIPTION_TRUNCATION_WARNING_THRESHOLD_CHARS)
            .then(|| SKILL_DESCRIPTION_TRUNCATED_WARNING.to_string())
    }

    pub(crate) fn average_truncated_description_chars(&self) -> usize {
        if self.total_count == 0 || self.truncated_description_chars == 0 {
            return 0;
        }

        self.truncated_description_chars
            .saturating_add(self.total_count.saturating_sub(1))
            / self.total_count
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkillRenderSize {
    pub(crate) full_body_bytes: usize,
    pub(crate) rendered_body_bytes: usize,
    pub(crate) full_body_tokens: usize,
    pub(crate) rendered_body_tokens: usize,
    pub(crate) outcome: Option<SkillCatalogRenderOutcome>,
}

impl SkillRenderSize {
    pub(crate) fn with_outcome(mut self, outcome: SkillCatalogRenderOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }
}
