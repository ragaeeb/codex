//! Feature-gated, first-party argument repair at the dispatch boundary.

use std::collections::BTreeMap;
use std::mem;
use std::time::Duration;
use std::time::Instant;

use codex_features::Feature;
use codex_history::MAX_TOOL_ARGUMENT_REPAIR_RULES;
use codex_history::ToolArgumentRepairOutcome as ReceiptOutcome;
use codex_history::ToolArgumentRepairReason;
use codex_history::ToolArgumentRepairReceipt;
use codex_history::ToolArgumentRepairToolFamily;
use codex_protocol::models::ResponseItem;
use codex_tools::ArgumentRepairOutcome;
use codex_tools::ToolPayload;
use codex_tools::ToolSpec;

use super::context::ToolCallSource;
use super::context::ToolInvocation;
use super::registry::CoreToolRuntime;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;

const FEATURE_COHORT: &str = "tool_argument_repair_v1";
const POLICY_MISS_REASON: &str = "not_allowlisted";
const NO_REASON: &str = "none";
const MAX_DISCLOSED_REPAIRED_CALLS: usize = 32;

/// One bounded, deterministic model-context disclosure emitted after a response's tool outputs.
pub(crate) struct ArgumentRepairDisclosure {
    pub(crate) item: ResponseItem,
    pub(crate) nested_receipt: Option<ToolArgumentRepairReceipt>,
}

/// Aggregates successful repairs so parallel and nested calls cannot interleave or flood history.
#[derive(Default)]
pub(crate) struct ArgumentRepairDisclosureAccumulator {
    retained_calls: usize,
    dropped_calls: usize,
    family_counts: BTreeMap<&'static str, usize>,
    rule_counts: BTreeMap<String, usize>,
    nested_receipt: Option<ToolArgumentRepairReceipt>,
}

impl ArgumentRepairDisclosureAccumulator {
    pub(crate) fn record(
        &mut self,
        receipt: Option<&ToolArgumentRepairReceipt>,
        source: &ToolCallSource,
    ) {
        let Some(receipt) = receipt.filter(|receipt| receipt.outcome == ReceiptOutcome::Repaired)
        else {
            return;
        };
        if self.retained_calls >= MAX_DISCLOSED_REPAIRED_CALLS {
            self.dropped_calls = self.dropped_calls.saturating_add(1);
            return;
        }
        self.retained_calls += 1;
        *self
            .family_counts
            .entry(family_label(receipt.tool_family))
            .or_default() += 1;
        for rule in receipt
            .rules
            .iter()
            .filter(|rule| is_stable_rule_id(rule))
            .take(MAX_TOOL_ARGUMENT_REPAIR_RULES)
        {
            *self.rule_counts.entry(rule.clone()).or_default() += 1;
        }
        if matches!(source, ToolCallSource::CodeMode { .. }) && self.nested_receipt.is_none() {
            self.nested_receipt = Some(receipt.clone());
        }
    }

    pub(crate) fn take_disclosure(&mut self) -> Option<ArgumentRepairDisclosure> {
        if self.retained_calls == 0 && self.dropped_calls == 0 {
            return None;
        }
        let summary = mem::take(self);
        let families = format_counts(summary.family_counts);
        let rules = format_counts(summary.rule_counts);
        let body = format!(
            "tool_argument_repair outcome=\"repaired\" repaired_call_count=\"{}\" dropped_call_count=\"{}\" families=\"{families}\" rules=\"{rules}\"",
            summary.retained_calls, summary.dropped_calls,
        );
        Some(ArgumentRepairDisclosure {
            item: ContextualUserFragment::into(InternalModelContextFragment::new(
                InternalContextSource::from_static("tool_argument_repair"),
                body,
            )),
            nested_receipt: summary.nested_receipt,
        })
    }
}

fn format_counts<K>(counts: BTreeMap<K, usize>) -> String
where
    K: AsRef<str> + Ord,
{
    counts
        .into_iter()
        .map(|(name, count)| format!("{}:{count}", name.as_ref()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Repairs one eligible function payload before PreToolUse hooks and approvals observe it.
pub(crate) fn repair_invocation(
    tool: &dyn CoreToolRuntime,
    invocation: &mut ToolInvocation,
) -> Option<ToolArgumentRepairReceipt> {
    if !invocation
        .turn
        .config
        .features
        .enabled(Feature::ToolArgumentRepair)
    {
        return None;
    }

    let ToolPayload::Function { arguments } = &invocation.payload else {
        emit_telemetry(
            invocation,
            TelemetrySnapshot {
                family: ToolArgumentRepairToolFamily::Unlisted,
                outcome: ReceiptOutcome::PolicyMiss,
                reason: Some(POLICY_MISS_REASON),
                input_bytes: arguments_bytes(&invocation.payload),
                effective_bytes: arguments_bytes(&invocation.payload),
                candidate_work: 0,
                rules: &[],
                duration: Duration::ZERO,
            },
        );
        return None;
    };
    let input_arguments = arguments.clone();
    let input_bytes = input_arguments.len();
    let family = tool_family(invocation);
    if !invocation.tool_name.is_default_namespace() || !matches!(tool.spec(), ToolSpec::Function(_))
    {
        emit_telemetry(
            invocation,
            TelemetrySnapshot {
                family,
                outcome: ReceiptOutcome::PolicyMiss,
                reason: Some(POLICY_MISS_REASON),
                input_bytes,
                effective_bytes: input_bytes,
                candidate_work: 0,
                rules: &[],
                duration: Duration::ZERO,
            },
        );
        return None;
    }
    let Some(policy) = tool.argument_repair_policy() else {
        emit_telemetry(
            invocation,
            TelemetrySnapshot {
                family,
                outcome: ReceiptOutcome::PolicyMiss,
                reason: Some(POLICY_MISS_REASON),
                input_bytes,
                effective_bytes: input_bytes,
                candidate_work: 0,
                rules: &[],
                duration: Duration::ZERO,
            },
        );
        return None;
    };

    let spec = tool.spec();
    let ToolSpec::Function(spec) = spec else {
        return None;
    };
    let started_at = Instant::now();
    let (outcome, metrics) =
        codex_tools::validate_and_repair_with_metrics(&spec.parameters, &input_arguments, &policy);
    let duration = started_at.elapsed();
    let (receipt_outcome, reason, rules) = match outcome {
        ArgumentRepairOutcome::ValidUnchanged { .. } => {
            (ReceiptOutcome::ValidUnchanged, None, Vec::new())
        }
        ArgumentRepairOutcome::Repaired { arguments, rules } => {
            invocation.payload = ToolPayload::Function { arguments };
            (
                ReceiptOutcome::Repaired,
                None,
                rules
                    .into_iter()
                    .map(|rule| rule.as_str().to_string())
                    .collect(),
            )
        }
        ArgumentRepairOutcome::NotRepairable { .. } => (
            ReceiptOutcome::NotRepairable,
            Some(ToolArgumentRepairReason::ValidationFailure),
            Vec::new(),
        ),
        ArgumentRepairOutcome::LimitExceeded { .. } => (
            ReceiptOutcome::LimitExceeded,
            Some(ToolArgumentRepairReason::LimitExceeded),
            Vec::new(),
        ),
        ArgumentRepairOutcome::UnsupportedSchema { .. } => (
            ReceiptOutcome::UnsupportedSchema,
            Some(ToolArgumentRepairReason::UnsupportedSchema),
            Vec::new(),
        ),
    };
    let effective_bytes = arguments_bytes(&invocation.payload);
    emit_telemetry(
        invocation,
        TelemetrySnapshot {
            family,
            outcome: receipt_outcome,
            reason: reason.map(reason_label),
            input_bytes,
            effective_bytes,
            candidate_work: metrics.candidate_work,
            rules: &rules,
            duration,
        },
    );
    Some(ToolArgumentRepairReceipt {
        tool_family: family,
        outcome: receipt_outcome,
        rules,
        input_bytes,
        effective_bytes,
        candidate_work: metrics.candidate_work,
        repair_duration_micros: u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
        reason,
    })
}

fn is_stable_rule_id(rule: &str) -> bool {
    matches!(
        rule,
        "optional_null_removed"
            | "stringified_array_decoded"
            | "stringified_object_decoded"
            | "scalar_wrapped_in_array"
            | "numeric_string_typed"
            | "boolean_string_typed"
            | "known_field_alias"
            | "markdown_path_unwrapped"
    )
}

fn tool_family(invocation: &ToolInvocation) -> ToolArgumentRepairToolFamily {
    if invocation.tool_name.is_default_namespace() && invocation.tool_name.name == "read_file" {
        ToolArgumentRepairToolFamily::ReadFile
    } else {
        ToolArgumentRepairToolFamily::Unlisted
    }
}

fn arguments_bytes(payload: &ToolPayload) -> usize {
    match payload {
        ToolPayload::Function { arguments } => arguments.len(),
        ToolPayload::Custom { input } => input.len(),
        ToolPayload::ToolSearch { arguments } => {
            serde_json::to_vec(arguments).map_or(0, |v| v.len())
        }
    }
}

struct TelemetrySnapshot<'a> {
    family: ToolArgumentRepairToolFamily,
    outcome: ReceiptOutcome,
    reason: Option<&'a str>,
    input_bytes: usize,
    effective_bytes: usize,
    candidate_work: usize,
    rules: &'a [String],
    duration: Duration,
}

fn emit_telemetry(invocation: &ToolInvocation, snapshot: TelemetrySnapshot<'_>) {
    let TelemetrySnapshot {
        family,
        outcome,
        reason,
        input_bytes,
        effective_bytes,
        candidate_work,
        rules,
        duration,
    } = snapshot;
    let family = family_label(family);
    let outcome = outcome_label(outcome);
    let reason = reason.unwrap_or(NO_REASON);
    let rule_count = rules.len().to_string();
    let rule_ids = if rules.is_empty() {
        "none".to_string()
    } else {
        rules.join(",")
    };
    let tags = [
        ("feature_cohort", FEATURE_COHORT),
        ("tool_family", family),
        ("outcome", outcome),
        ("reason", reason),
        ("rule_count", rule_count.as_str()),
        ("rule_ids", rule_ids.as_str()),
    ];
    let telemetry = &invocation.turn.session_telemetry;
    telemetry.counter("codex.tool_argument_repair", /*inc*/ 1, &tags);
    telemetry.histogram(
        "codex.tool_argument_repair.input_bytes",
        saturating_i64(input_bytes),
        &tags,
    );
    telemetry.histogram(
        "codex.tool_argument_repair.effective_bytes",
        saturating_i64(effective_bytes),
        &tags,
    );
    telemetry.histogram(
        "codex.tool_argument_repair.candidate_work",
        saturating_i64(candidate_work),
        &tags,
    );
    telemetry.record_duration("codex.tool_argument_repair.duration", duration, &tags);
}

fn saturating_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn family_label(family: ToolArgumentRepairToolFamily) -> &'static str {
    match family {
        ToolArgumentRepairToolFamily::ReadFile => "read_file",
        ToolArgumentRepairToolFamily::Unlisted => "unlisted",
    }
}

fn outcome_label(outcome: ReceiptOutcome) -> &'static str {
    match outcome {
        ReceiptOutcome::PolicyMiss => "policy_miss",
        ReceiptOutcome::ValidUnchanged => "valid_unchanged",
        ReceiptOutcome::Repaired => "repaired",
        ReceiptOutcome::NotRepairable => "not_repairable",
        ReceiptOutcome::LimitExceeded => "limit_exceeded",
        ReceiptOutcome::UnsupportedSchema => "unsupported_schema",
    }
}

fn reason_label(reason: ToolArgumentRepairReason) -> &'static str {
    match reason {
        ToolArgumentRepairReason::NotAllowlisted => "not_allowlisted",
        ToolArgumentRepairReason::UnsupportedSchema => "unsupported_schema",
        ToolArgumentRepairReason::LimitExceeded => "limit_exceeded",
        ToolArgumentRepairReason::ValidationFailure => "validation_failure",
    }
}

#[cfg(test)]
#[path = "argument_repair_tests.rs"]
mod tests;
