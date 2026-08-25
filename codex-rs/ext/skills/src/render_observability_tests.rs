use std::sync::Mutex;

use codex_extension_api::ExtensionMetrics;
use codex_otel::THREAD_SKILLS_CATALOG_FULL_BYTES_METRIC;
use codex_otel::THREAD_SKILLS_CATALOG_FULL_TOKENS_METRIC;
use codex_otel::THREAD_SKILLS_CATALOG_RENDER_OUTCOME_METRIC;
use codex_otel::THREAD_SKILLS_CATALOG_RENDERED_BYTES_METRIC;
use codex_otel::THREAD_SKILLS_CATALOG_RENDERED_TOKENS_METRIC;
use pretty_assertions::assert_eq;

use super::*;
use crate::render_policy::SkillCatalogRenderOutcome;

#[derive(Debug, Eq, PartialEq)]
struct RecordedHistogram {
    name: String,
    value: i64,
    tags: Vec<(String, String)>,
}

#[derive(Default)]
struct RecordingMetrics {
    samples: Mutex<Vec<RecordedHistogram>>,
}

impl ExtensionMetrics for RecordingMetrics {
    fn counter(&self, name: &str, _inc: i64, _tags: &[(&str, &str)]) {
        panic!("unexpected counter: {name}");
    }

    fn histogram(&self, name: &str, value: i64, tags: &[(&str, &str)]) {
        self.samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedHistogram {
                name: name.to_string(),
                value,
                tags: tags
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                    .collect(),
            });
    }
}

#[test]
fn records_core_equivalent_catalog_render_metrics_with_surface() {
    let metrics = RecordingMetrics::default();
    let report = SkillRenderReport {
        total_count: 5,
        included_count: 3,
        omitted_count: 2,
        truncated_description_chars: 700,
        truncated_description_count: 4,
    };

    record_catalog_render(
        Some(&metrics),
        CatalogSurface::TurnInput,
        SkillMetadataBudget::Tokens(400),
        &report,
        SkillRenderSize {
            full_body_bytes: 2_000,
            rendered_body_bytes: 1_200,
            full_body_tokens: 500,
            rendered_body_tokens: 300,
            outcome: Some(SkillCatalogRenderOutcome::Compact),
        },
    );

    assert_eq!(
        *metrics
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            RecordedHistogram {
                name: THREAD_SKILLS_ENABLED_TOTAL_METRIC.to_string(),
                value: 5,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_KEPT_TOTAL_METRIC.to_string(),
                value: 3,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_TRUNCATED_METRIC.to_string(),
                value: 1,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_DESCRIPTION_TRUNCATED_CHARS_METRIC.to_string(),
                value: 700,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_CATALOG_FULL_BYTES_METRIC.to_string(),
                value: 2_000,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_CATALOG_RENDERED_BYTES_METRIC.to_string(),
                value: 1_200,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_CATALOG_FULL_TOKENS_METRIC.to_string(),
                value: 500,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_CATALOG_RENDERED_TOKENS_METRIC.to_string(),
                value: 300,
                tags: vec![("catalog_surface".to_string(), "turn_input".to_string())],
            },
            RecordedHistogram {
                name: THREAD_SKILLS_CATALOG_RENDER_OUTCOME_METRIC.to_string(),
                value: 1,
                tags: vec![
                    ("catalog_surface".to_string(), "turn_input".to_string()),
                    ("render_outcome".to_string(), "compact".to_string()),
                ],
            },
        ]
    );
}
