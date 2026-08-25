use super::*;
use codex_exec_server::FileMetadata;
use codex_file_system::read_file_window::ReadFileWindow;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::read_file::ReadFileHandler;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;

#[test]
fn telemetry_rules_are_bounded_and_content_free() {
    let cases = [
        (
            FunctionCallError::RespondToModel("file is not valid UTF-8".to_string()),
            "binary_rejected_v1",
        ),
        (
            FunctionCallError::RespondToModel("the filesystem sandbox denied the read".to_string()),
            "sandbox_denied_v1",
        ),
        (
            FunctionCallError::RespondToModel("max_bytes must be bounded".to_string()),
            "invalid_bounds_v1",
        ),
        (
            FunctionCallError::RespondToModel(
                "/private/秘密.txt sha256:secret artifact-id".to_string(),
            ),
            "read_error_v1",
        ),
    ];
    for (error, expected_rule) in cases {
        let rule = error_metric_rule(&error);
        assert_eq!(rule, expected_rule);
        assert!(!rule.contains('/'));
        assert!(!rule.contains("秘密"));
        assert!(!rule.contains("sha256"));
        assert!(!rule.contains("artifact"));
    }
}

#[tokio::test]
async fn telemetry_metrics_are_emitted_by_real_finalization_outcomes() {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client should initialize");
    let telemetry = SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.5",
        "gpt-5.5",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics);

    let (mut session, mut turn) = make_session_and_context().await;
    session.services.session_telemetry = telemetry.clone();
    turn.session_telemetry = telemetry.clone();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let invocation = test_invocation(Arc::clone(&session), Arc::clone(&turn), "local");
    let _ = finalize_fixture(&invocation, /*modified_at_ms*/ 1).await;
    let _ = finalize_fixture(&invocation, /*modified_at_ms*/ 1).await;
    let _ = finalize_fixture(&invocation, /*modified_at_ms*/ 2).await;

    let mut invalid_bounds = test_invocation(Arc::clone(&session), Arc::clone(&turn), "bounds");
    invalid_bounds.payload = ToolPayload::Function {
        arguments: r#"{"path":"SECRET_ARGUMENT","max_bytes":0}"#.to_string(),
    };
    let _ = ReadFileHandler::new(/*include_environment_id*/ false)
        .handle(invalid_bounds)
        .await;

    let unsupported_invocation =
        test_invocation(Arc::clone(&session), Arc::clone(&turn), "unsupported");
    let _ = finalize_fixture_with_spilling_support(
        &unsupported_invocation,
        /*modified_at_ms*/ 1,
        /*spilling_supported*/ false,
    )
    .await;
    let unsupported_output = finalize_fixture_with_spilling_support(
        &unsupported_invocation,
        /*modified_at_ms*/ 1,
        /*spilling_supported*/ false,
    )
    .await;
    assert!(
        unsupported_output
            .log_output()
            .contains("artifact_backend_unavailable_v1")
    );

    let (mut failed_session, mut failed_turn) = make_session_and_context().await;
    failed_session.services.session_telemetry = telemetry.clone();
    failed_turn.session_telemetry = telemetry.clone();
    let failed_session = Arc::new(failed_session);
    let failed_turn = Arc::new(failed_turn);
    let failed_invocation = test_invocation(
        Arc::clone(&failed_session),
        Arc::clone(&failed_turn),
        "store-failure",
    );
    let _ = finalize_fixture(&failed_invocation, /*modified_at_ms*/ 1).await;
    let artifact_root = failed_session.codex_home().await.join("tool_outputs");
    if artifact_root.is_dir() {
        std::fs::remove_dir_all(&artifact_root).expect("remove test artifact directory");
    }
    std::fs::create_dir_all(artifact_root.parent().expect("codex home parent"))
        .expect("create test codex home");
    std::fs::write(&artifact_root, b"blocked").expect("block test artifact storage");
    let store_failure_output = finalize_fixture(&failed_invocation, /*modified_at_ms*/ 1).await;
    assert!(
        store_failure_output
            .log_output()
            .contains("artifact_store_fallback_v1")
    );

    let rendered = format!(
        "{:?}",
        telemetry
            .snapshot_metrics()
            .expect("metrics snapshot should be available")
    );
    for safe in [
        "codex.tool.read_file.result",
        "codex.tool.read_file.source_bytes",
        "codex.tool.read_file.inline_bytes",
        "exact_window_artifact_v1",
        "artifact_backend_unavailable_v1",
        "artifact_store_fallback_v1",
        "invalid_bounds_v1",
    ] {
        assert!(rendered.contains(safe), "metrics should contain {safe}");
    }
    for secret in [
        "/private/秘密.txt",
        "SECRET_ARGUMENT",
        "SECRET_CONTENT",
        "SECRET_ARTIFACT_ID",
        "SECRET_ENVIRONMENT_ID",
    ] {
        assert!(!rendered.contains(secret), "metrics leaked {secret}");
    }
}

fn test_invocation(session: Arc<Session>, turn: Arc<TurnContext>, call_id: &str) -> ToolInvocation {
    ToolInvocation {
        step_context: StepContext::for_test(Arc::clone(&turn)),
        session,
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain("read_file"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}

async fn finalize_fixture(invocation: &ToolInvocation, modified_at_ms: i64) -> Box<dyn ToolOutput> {
    finalize_fixture_with_spilling_support(
        invocation,
        modified_at_ms,
        /*spilling_supported*/ true,
    )
    .await
}

async fn finalize_fixture_with_spilling_support(
    invocation: &ToolInvocation,
    modified_at_ms: i64,
    spilling_supported: bool,
) -> Box<dyn ToolOutput> {
    let text = serde_json::json!({
        "type": "file_read",
        "version": 1,
        "path": "metrics-fixture.txt",
        "fingerprint": {
            "size_bytes": 9_000,
            "modified_at_ms": modified_at_ms,
            "window_digest": "sha256:fixture"
        },
        "window": {
            "start_byte": 0,
            "end_byte": 9_000,
            "text": "x".repeat(9_000),
            "line_fragments": 1,
            "line_continues": false
        },
        "next_offset": null,
        "eof": true,
        "continuation": null
    })
    .to_string();
    let metadata = FileMetadata {
        is_directory: false,
        is_file: true,
        is_symlink: false,
        size: 9_000,
        created_at_ms: 1,
        modified_at_ms,
    };
    let window = ReadFileWindow {
        text: "x".repeat(9_000),
        start_byte: 0,
        end_byte: 9_000,
        line_fragments: 1,
        line_continues: false,
        next_offset: None,
        eof: true,
    };
    let canonical_path = codex_utils_path_uri::PathUri::from_host_native_path(
        std::env::temp_dir().join("read-file-metrics-fixture"),
    )
    .expect("fixture path should be absolute");
    finalize_read_with_spilling_support(
        invocation,
        ReadFinalization {
            environment_id: "primary",
            canonical_path,
            metadata: &metadata,
            window,
            inline_result: text,
            response_max_bytes: 8 * 1024,
        },
        spilling_supported,
    )
    .await
    .expect("finalization should return a bounded tool output")
}
