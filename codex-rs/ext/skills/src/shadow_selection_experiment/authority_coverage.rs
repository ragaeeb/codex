use std::collections::HashSet;

use codex_otel::MetricsClient;

use crate::catalog::SkillCatalog;
use crate::catalog::SkillSourceKind;

const AUTHORITY_CATALOG_ENTRY_COUNT_METRIC: &str =
    "codex.skills.shadow_selection.authority_catalog_entries";
const AUTHORITY_ELIGIBLE_ENTRY_COUNT_METRIC: &str =
    "codex.skills.shadow_selection.authority_eligible_entries";
const AUTHORITY_TAGS: [&str; 4] = ["host", "executor", "orchestrator", "custom"];

pub(super) fn record_authority_coverage(
    metrics_client: Option<&MetricsClient>,
    catalog: &SkillCatalog,
    eligible_ids: &HashSet<usize>,
    query_script: &'static str,
) {
    let Some(metrics_client) = metrics_client else {
        return;
    };
    for (index, authority) in authority_coverage(catalog, eligible_ids).iter().enumerate() {
        let tags = [
            ("authority", AUTHORITY_TAGS[index]),
            ("query_script", query_script),
        ];
        let _ = metrics_client.histogram(
            AUTHORITY_CATALOG_ENTRY_COUNT_METRIC,
            super::metric_value(authority.catalog_entries),
            &tags,
        );
        let _ = metrics_client.histogram(
            AUTHORITY_ELIGIBLE_ENTRY_COUNT_METRIC,
            super::metric_value(authority.eligible_entries),
            &tags,
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct AuthorityCoverage {
    pub(super) catalog_entries: usize,
    pub(super) eligible_entries: usize,
}

pub(super) fn authority_coverage(
    catalog: &SkillCatalog,
    eligible_ids: &HashSet<usize>,
) -> [AuthorityCoverage; AUTHORITY_TAGS.len()] {
    let mut coverage = [AuthorityCoverage::default(); AUTHORITY_TAGS.len()];
    for (id, entry) in catalog.entries.iter().enumerate() {
        if !entry.is_model_visible() {
            continue;
        }
        let authority = &mut coverage[authority_index(&entry.authority.kind)];
        authority.catalog_entries = authority.catalog_entries.saturating_add(1);
        if eligible_ids.contains(&id) {
            authority.eligible_entries = authority.eligible_entries.saturating_add(1);
        }
    }
    coverage
}

pub(super) fn authority_index(kind: &SkillSourceKind) -> usize {
    match kind {
        SkillSourceKind::Host => 0,
        SkillSourceKind::Executor => 1,
        SkillSourceKind::Orchestrator => 2,
        SkillSourceKind::Custom(_) => 3,
    }
}
