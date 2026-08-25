use std::collections::HashSet;

use pretty_assertions::assert_eq;

use super::authority_coverage::AuthorityCoverage;
use super::authority_coverage::authority_coverage;
use super::authority_coverage::authority_index;
use super::*;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;

#[test]
fn recent_invocations_refresh_recency_and_evict_old_skills() {
    let history = RecentSkillInvocations::default();
    for index in 0..=MAX_SHADOW_RESULTS {
        history.record(format!("skill-{index}"));
    }
    history.record("skill-1".to_string());

    let recent = history.snapshot();

    assert_eq!(MAX_SHADOW_RESULTS, recent.len());
    assert_eq!(Some("skill-1"), recent.first().map(String::as_str));
    assert_eq!(Some("skill-2"), recent.last().map(String::as_str));
    assert!(!recent.iter().any(|skill| skill == "skill-0"));
}

#[test]
fn rank_buckets_distinguish_results_above_twenty() {
    assert_eq!("11_20", rank_bucket(Some(20)));
    assert_eq!("21_50", rank_bucket(Some(21)));
    assert_eq!("21_50", rank_bucket(Some(50)));
    assert_eq!("miss", rank_bucket(Some(51)));
}

fn entry(kind: SkillSourceKind, name: &str, enabled: bool) -> SkillCatalogEntry {
    let resource = format!("{name}/SKILL.md");
    let entry = SkillCatalogEntry::new(
        SkillPackageId(resource.clone()),
        SkillAuthority::new(kind, "authority"),
        name,
        "description",
        SkillResourceId::new(resource),
    );
    if enabled { entry } else { entry.disabled() }
}

#[test]
fn authority_coverage_reports_visible_and_eligible_entries_by_broad_authority() {
    let catalog = SkillCatalog {
        entries: vec![
            entry(SkillSourceKind::Host, "host", /*enabled*/ true),
            entry(SkillSourceKind::Executor, "executor", /*enabled*/ true),
            entry(
                SkillSourceKind::Orchestrator,
                "orchestrator",
                /*enabled*/ true,
            ),
            entry(
                SkillSourceKind::custom("provider-secret"),
                "custom",
                /*enabled*/ true,
            ),
            entry(SkillSourceKind::Host, "disabled", /*enabled*/ false),
        ],
        warnings: Vec::new(),
    };
    let eligible_ids = HashSet::from([0, 2]);

    assert_eq!(
        authority_coverage(&catalog, &eligible_ids),
        [
            AuthorityCoverage {
                catalog_entries: 1,
                eligible_entries: 1,
            },
            AuthorityCoverage {
                catalog_entries: 1,
                eligible_entries: 0,
            },
            AuthorityCoverage {
                catalog_entries: 1,
                eligible_entries: 1,
            },
            AuthorityCoverage {
                catalog_entries: 1,
                eligible_entries: 0,
            },
        ]
    );
    assert_eq!(
        [
            authority_index(&SkillSourceKind::Host),
            authority_index(&SkillSourceKind::Executor),
            authority_index(&SkillSourceKind::Orchestrator),
            authority_index(&SkillSourceKind::custom("another-provider")),
        ],
        [0, 1, 2, 3]
    );
}
