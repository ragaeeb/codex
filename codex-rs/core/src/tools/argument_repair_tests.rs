use super::*;
use crate::session::step_context::StepContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use codex_features::Feature;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::OtelProvider;
use codex_otel::SessionTelemetry;
use codex_otel::ToolResultLogPolicy;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_tools::ArgumentRepairPolicy;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;

struct RepairTestHandler {
    name: ToolName,
    policy: Option<ArgumentRepairPolicy>,
    observed_arguments: Arc<Mutex<Vec<String>>>,
    failure: Option<&'static str>,
}

impl ToolExecutor<ToolInvocation> for RepairTestHandler {
    fn tool_name(&self) -> ToolName {
        self.name.clone()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: self.name.name.clone(),
            description: "test tool".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "max_bytes".to_string(),
                    JsonSchema::integer(/*description*/ None),
                )]),
                /*required*/ None,
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => String::new(),
        };
        self.observed_arguments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(arguments);
        let failure = self.failure;
        Box::pin(async move {
            if let Some(message) = failure {
                return Err(crate::function_tool::FunctionCallError::RespondToModel(
                    message.to_string(),
                ));
            }
            Ok(
                Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                    as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for RepairTestHandler {
    fn argument_repair_policy(&self) -> Option<ArgumentRepairPolicy> {
        self.policy.clone()
    }

    fn tool_result_log_policy(&self) -> ToolResultLogPolicy {
        ToolResultLogPolicy::ContentFree {
            tool_family: "read_file",
        }
    }
}

async fn test_invocation(
    tool_name: ToolName,
    arguments: &str,
    feature_enabled: bool,
) -> ToolInvocation {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .set_enabled(Feature::ToolArgumentRepair, feature_enabled)
        .expect("test feature should be mutable");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "repair-call".to_string(),
        tool_name,
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

async fn test_invocation_with_telemetry(
    tool_name: ToolName,
    arguments: &str,
    feature_enabled: bool,
    telemetry: SessionTelemetry,
) -> ToolInvocation {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.session_telemetry = telemetry;
    Arc::make_mut(&mut turn.config)
        .features
        .set_enabled(Feature::ToolArgumentRepair, feature_enabled)
        .expect("test feature should be mutable");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "repair-call".to_string(),
        tool_name,
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn handler(name: ToolName, policy: Option<ArgumentRepairPolicy>) -> Arc<RepairTestHandler> {
    Arc::new(RepairTestHandler {
        name,
        policy,
        observed_arguments: Arc::new(Mutex::new(Vec::new())),
        failure: None,
    })
}

fn failing_handler(name: ToolName, policy: Option<ArgumentRepairPolicy>) -> Arc<RepairTestHandler> {
    Arc::new(RepairTestHandler {
        name,
        policy,
        observed_arguments: Arc::new(Mutex::new(Vec::new())),
        failure: Some("expected handler failure"),
    })
}

#[tokio::test]
async fn feature_off_preserves_raw_handler_arguments_and_receipt_absence() -> anyhow::Result<()> {
    let name = ToolName::plain("read_file");
    let handler = handler(name.clone(), Some(ArgumentRepairPolicy::default()));
    let observed = Arc::clone(&handler.observed_arguments);
    let registry = ToolRegistry::with_handler_for_test(Arc::clone(&handler));
    let raw = r#"{"max_bytes":"4096"}"#;

    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(name, raw, /*feature_enabled*/ false).await,
            /*terminal_outcome_reached*/ None,
        )
        .await?;

    assert_eq!(
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [raw.to_string()]
    );
    assert_eq!(result.argument_repair_receipt, None);
    Ok(())
}

#[tokio::test]
async fn enabled_repair_reaches_handler_once_with_effective_arguments() -> anyhow::Result<()> {
    let name = ToolName::plain("read_file");
    let handler = handler(name.clone(), Some(ArgumentRepairPolicy::default()));
    let observed = Arc::clone(&handler.observed_arguments);
    let registry = ToolRegistry::with_handler_for_test(Arc::clone(&handler));
    let mut invocation = test_invocation(
        name,
        r#"{"max_bytes":"4096"}"#,
        /*feature_enabled*/ true,
    )
    .await;
    let repaired = repair_invocation(handler.as_ref(), &mut invocation)
        .expect("enabled first-party policy should produce a receipt");
    assert_eq!(repaired.outcome, ReceiptOutcome::Repaired);
    let ToolPayload::Function { arguments } = invocation.payload else {
        panic!("repair must retain a function payload");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&arguments)?,
        serde_json::json!({"max_bytes": 4096})
    );

    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                ToolName::plain("read_file"),
                r#"{"max_bytes":"4096"}"#,
                /*feature_enabled*/ true,
            )
            .await,
            /*terminal_outcome_reached*/ None,
        )
        .await?;
    assert_eq!(
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [r#"{"max_bytes":4096}"#.to_string()]
    );
    let receipt = result
        .argument_repair_receipt
        .expect("effective call should carry a receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::Repaired);
    assert_eq!(receipt.rules, ["numeric_string_typed"]);
    assert_eq!(receipt.input_bytes, r#"{"max_bytes":"4096"}"#.len());
    assert_eq!(receipt.effective_bytes, r#"{"max_bytes":4096}"#.len());
    Ok(())
}

#[tokio::test]
async fn repaired_handler_failure_retains_a_bounded_model_receipt() -> anyhow::Result<()> {
    let name = ToolName::plain("read_file");
    let handler = failing_handler(name.clone(), Some(ArgumentRepairPolicy::default()));
    let registry = ToolRegistry::with_handler_for_test(handler);

    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                name,
                r#"{"max_bytes":"4096"}"#,
                /*feature_enabled*/ true,
            )
            .await,
            /*terminal_outcome_reached*/ None,
        )
        .await?;

    assert!(!result.result.success_for_logging());
    assert_eq!(result.result.log_output(), "expected handler failure");
    let receipt = result
        .argument_repair_receipt
        .expect("a repaired handler failure must retain its receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::Repaired);
    assert_eq!(receipt.rules, ["numeric_string_typed"]);
    Ok(())
}

#[tokio::test]
async fn policy_miss_keeps_namespaced_and_unlisted_tools_unmodified() -> anyhow::Result<()> {
    for (name, policy) in [
        (
            ToolName::namespaced("mcp__server__", "read_file"),
            Some(ArgumentRepairPolicy::default()),
        ),
        (ToolName::plain("dynamic_tool"), None),
    ] {
        let handler = handler(name.clone(), policy);
        let observed = Arc::clone(&handler.observed_arguments);
        let registry = ToolRegistry::with_handler_for_test(Arc::clone(&handler));
        let raw = r#"{"max_bytes":"4096"}"#;
        let result = registry
            .dispatch_any_with_terminal_outcome(
                test_invocation(name, raw, /*feature_enabled*/ true).await,
                /*terminal_outcome_reached*/ None,
            )
            .await?;
        assert_eq!(
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [raw.to_string()]
        );
        assert_eq!(result.argument_repair_receipt, None);
    }
    Ok(())
}

#[test]
fn model_disclosure_is_bounded_and_content_free() {
    let receipt = ToolArgumentRepairReceipt {
        tool_family: ToolArgumentRepairToolFamily::ReadFile,
        outcome: ReceiptOutcome::Repaired,
        rules: vec![
            "numeric_string_typed".to_string(),
            "secret_rule".to_string(),
        ],
        input_bytes: 19,
        effective_bytes: 17,
        candidate_work: 1,
        repair_duration_micros: 1,
        reason: None,
    };
    let mut disclosure = ArgumentRepairDisclosureAccumulator::default();
    for index in 0..40 {
        disclosure.record(
            Some(&receipt),
            &ToolCallSource::CodeMode {
                cell_id: format!("cell-{index}"),
                runtime_tool_call_id: format!("nested-{index}"),
            },
        );
    }
    let disclosure = disclosure
        .take_disclosure()
        .expect("repaired calls disclose a bounded aggregate");
    let text = serde_json::to_string(&disclosure.item).expect("context item should serialize");
    assert!(text.contains("tool_argument_repair"));
    assert!(text.contains("numeric_string_typed"));
    assert!(text.contains("repaired_call_count=\\\"32\\\""));
    assert!(text.contains("dropped_call_count=\\\"8\\\""));
    assert!(!text.contains("secret_rule"));
    assert!(!text.contains("/Users/secret"));
    assert!(text.len() < 1_024);
    assert!(disclosure.nested_receipt.is_some());
}

#[test]
fn policy_debug_does_not_expose_alias_values_or_paths() {
    let mut policy = ArgumentRepairPolicy::default();
    policy
        .insert_known_field_alias(
            "/secret/project/path",
            "secret_source_alias",
            "secret_destination_alias",
        )
        .expect("test alias should be accepted");
    let debug = format!("{policy:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("project"));
}

#[tokio::test]
async fn telemetry_capture_is_content_free_across_success_error_limit_and_retry() {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics should initialize");
    let telemetry = SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.6-luna",
        "gpt-5.6-luna",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics);

    let cases = [
        (r#"{"max_bytes":"4096"}"#, ArgumentRepairPolicy::default()),
        (
            r#"{"max_bytes":"not-a-number"}"#,
            ArgumentRepairPolicy::default(),
        ),
        (r#"{"max_bytes":4096}"#, ArgumentRepairPolicy::default()),
    ];
    for (arguments, policy) in cases {
        let handler = handler(ToolName::plain("read_file"), Some(policy));
        let mut invocation = test_invocation_with_telemetry(
            ToolName::plain("read_file"),
            arguments,
            /*feature_enabled*/ true,
            telemetry.clone(),
        )
        .await;
        let _ = repair_invocation(handler.as_ref(), &mut invocation);
    }
    let mut limited = ArgumentRepairPolicy::default();
    let mut limits = limited.limits().clone();
    limits.max_input_bytes = 1;
    limited.set_limits(limits);
    let handler = handler(ToolName::plain("read_file"), Some(limited));
    let mut invocation = test_invocation_with_telemetry(
        ToolName::plain("read_file"),
        r#"{"max_bytes":"4096"}"#,
        /*feature_enabled*/ true,
        telemetry.clone(),
    )
    .await;
    let _ = repair_invocation(handler.as_ref(), &mut invocation);

    let snapshot = telemetry
        .snapshot_metrics()
        .expect("metrics snapshot should be available");
    let serialized = format!("{snapshot:?}");
    assert!(serialized.contains("codex.tool_argument_repair"));
    assert!(serialized.contains("numeric_string_typed"));
    assert!(serialized.contains("repaired"));
    assert!(serialized.contains("not_repairable"));
    assert!(serialized.contains("limit_exceeded"));
    for secret in ["not-a-number", "4096", "max_bytes"] {
        assert!(!serialized.contains(secret), "telemetry leaked {secret}");
    }
}

#[tokio::test]
async fn router_otel_capture_hides_alias_and_unknown_argument_content() -> anyhow::Result<()> {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("tool-argument-repair-privacy-test");
    let subscriber = tracing_subscriber::registry()
        .with(
            OpenTelemetryTracingBridge::new(&logger_provider)
                .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .set_enabled(Feature::ToolArgumentRepair, /*enabled*/ true)
        .expect("test feature should be mutable");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let mut policy = ArgumentRepairPolicy::default();
    policy
        .insert_known_field_alias("", "PRIVATE_ALIAS_SOURCE", "max_bytes")
        .expect("test alias should be accepted");
    let handler = handler(ToolName::plain("read_file"), Some(policy));
    let router = ToolRouter::from_parts(
        ToolRegistry::with_handler_for_test(Arc::clone(&handler)),
        vec![handler.spec()],
    );
    let calls = [
        ("alias", json!({"PRIVATE_ALIAS_SOURCE": "4096"}).to_string()),
        (
            "unknown",
            json!({
                "max_bytes": "not-a-number",
                "PRIVATE_UNKNOWN_KEY": "PRIVATE_UNKNOWN_VALUE"
            })
            .to_string(),
        ),
    ];
    let dispatch = tracing::Dispatch::new(subscriber);
    {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        for (call_id, arguments) in calls {
            router
                .dispatch_tool_call_with_code_mode_result(
                    Arc::clone(&session),
                    StepContext::for_test(Arc::clone(&turn)),
                    CancellationToken::new(),
                    Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    ToolCall {
                        tool_name: ToolName::plain("read_file"),
                        call_id: format!("privacy-{call_id}"),
                        payload: ToolPayload::Function { arguments },
                        encrypted_function_args: None,
                    },
                    ToolCallSource::Direct,
                )
                .await?;
        }
    }
    tracer_provider.force_flush()?;
    logger_provider.force_flush()?;

    let rendered_spans = span_exporter
        .get_finished_spans()?
        .iter()
        .map(|span| format!("{:?}{:?}", span.attributes, span.events.events))
        .collect::<Vec<_>>()
        .join("\n");
    let rendered_logs = log_exporter
        .get_emitted_logs()?
        .iter()
        .map(|log| format!("{:?}", log.record))
        .collect::<Vec<_>>()
        .join("\n");
    for secret in [
        "PRIVATE_ALIAS_SOURCE",
        "PRIVATE_UNKNOWN_KEY",
        "PRIVATE_UNKNOWN_VALUE",
        "not-a-number",
        "4096",
    ] {
        assert!(
            !rendered_spans.contains(secret),
            "span telemetry leaked {secret}"
        );
        assert!(
            !rendered_logs.contains(secret),
            "log telemetry leaked {secret}"
        );
    }
    assert!(
        rendered_spans.contains("[content-free tool call]"),
        "spans did not contain the content-free call marker: {rendered_spans}"
    );
    assert!(
        rendered_logs.contains("content-free tool arguments"),
        "logs did not contain the content-free marker: {rendered_logs}"
    );
    Ok(())
}
