use super::*;

#[test]
fn rejects_bounds_that_could_not_produce_a_bounded_response() {
    for args in [
        ReadFileArgs {
            path: "file.txt".to_string(),
            offset: None,
            max_bytes: Some(MIN_REQUEST_BYTES - 1),
            max_lines: None,
            environment_id: None,
        },
        ReadFileArgs {
            path: "file.txt".to_string(),
            offset: None,
            max_bytes: Some(MAX_RESPONSE_BYTES + 1),
            max_lines: None,
            environment_id: None,
        },
        ReadFileArgs {
            path: "file.txt".to_string(),
            offset: None,
            max_bytes: None,
            max_lines: Some(MAX_RESPONSE_LINES + 1),
            environment_id: None,
        },
    ] {
        assert!(parse_bounds(&args).is_err());
    }
}

#[test]
fn path_diagnostics_obey_the_independent_model_cap() {
    let error = path_error(
        &"x".repeat(MAX_PATH_ARGUMENT_BYTES),
        "detail".repeat(MAX_MODEL_VISIBLE_BYTES),
    );
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("path errors should be returned to the model");
    };
    assert!(message.len() <= MAX_MODEL_VISIBLE_BYTES);
}

#[test]
fn zero_model_policy_is_rejected_before_window_rendering() {
    let bounds = ReadBounds {
        offset: 0,
        max_bytes: DEFAULT_MAX_BYTES,
        max_lines: DEFAULT_MAX_LINES,
    };
    assert_eq!(
        effective_model_response_max_bytes(
            bounds,
            codex_protocol::openai_models::TruncationPolicyConfig::bytes(/*limit*/ 0),
            /*configured_token_limit*/ None,
        ),
        0
    );
}
