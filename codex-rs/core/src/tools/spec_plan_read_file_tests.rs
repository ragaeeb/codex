use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn dynamic_tools_cannot_replace_native_read_file() {
    let plan = probe_with(
        |turn| {
            set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                /*namespace*/ None,
                "read_file",
                /*defer_loading*/ false,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&["read_file"]);
    plan.assert_registered_contains(&["read_file"]);
    assert_eq!(
        plan.visible_names
            .iter()
            .filter(|name| *name == "read_file")
            .count(),
        1
    );
}

#[tokio::test]
async fn native_read_file_feature_controls_environment_backed_exposure() {
    let disabled = probe(|turn| {
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ false);
    })
    .await;
    disabled.assert_visible_lacks(&["read_file"]);
    disabled.assert_registered_lacks(&["read_file"]);

    let enabled = probe(|turn| {
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
    })
    .await;
    enabled.assert_visible_contains(&["read_file"]);
    enabled.assert_registered_contains(&["read_file"]);
    assert_eq!(enabled.exposure("read_file"), ToolExposure::Direct);
    assert!(!has_parameter(
        enabled.visible_spec("read_file"),
        "environment_id"
    ));

    let multiple = probe(|turn| {
        duplicate_primary_environment(turn);
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
    })
    .await;
    multiple.assert_visible_contains(&["read_file"]);
    assert!(has_parameter(
        multiple.visible_spec("read_file"),
        "environment_id"
    ));
}

#[tokio::test]
async fn native_read_file_is_available_to_guardian_and_code_mode() {
    let guardian = probe(|turn| {
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
        update_config(turn, |config| {
            config
                .permissions
                .set_permission_profile(codex_protocol::models::PermissionProfile::Managed {
                    file_system: codex_protocol::models::ManagedFileSystemPermissions::Unrestricted,
                    network: codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
                })
                .expect("managed guardian profile should be accepted");
        });
        let permission_profile = turn
            .config
            .permissions
            .permission_profile_state()
            .snapshot();
        let TurnEnvironmentState::Ready(environment) = turn
            .environments
            .environments
            .first_mut()
            .expect("primary environment")
        else {
            panic!("primary environment should be ready");
        };
        environment.config_mut().permission_profile = permission_profile;
        turn.session_source = codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Other(
                crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
            ),
        );
    })
    .await;
    guardian.assert_visible_contains(&["read_file"]);
    guardian.assert_registered_contains(&["read_file"]);

    let unmanaged_guardian = probe(|turn| {
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
        update_config(turn, |config| {
            config
                .permissions
                .set_permission_profile(codex_protocol::models::PermissionProfile::Disabled)
                .expect("disabled guardian profile should be accepted");
        });
        let permission_profile = turn
            .config
            .permissions
            .permission_profile_state()
            .snapshot();
        let TurnEnvironmentState::Ready(environment) = turn
            .environments
            .environments
            .first_mut()
            .expect("primary environment")
        else {
            panic!("primary environment should be ready");
        };
        environment.config_mut().permission_profile = permission_profile;
        turn.session_source = codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Other(
                crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
            ),
        );
    })
    .await;
    unmanaged_guardian.assert_visible_lacks(&["read_file"]);
    unmanaged_guardian.assert_registered_lacks(&["read_file"]);

    let code_mode = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        set_feature(turn, Feature::NativeReadFile, /*enabled*/ true);
    })
    .await;
    code_mode.assert_visible_contains(&[codex_code_mode::PUBLIC_TOOL_NAME]);
    code_mode.assert_registered_contains(&["read_file"]);
    assert_eq!(code_mode.exposure("read_file"), ToolExposure::Direct);
    let ToolSpec::Freeform(exec) = code_mode.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(
        serde_json::to_string(exec)
            .expect("code mode spec should serialize")
            .contains("read_file")
    );
}
