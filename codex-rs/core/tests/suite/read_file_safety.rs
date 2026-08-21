use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_rejects_binary_special_and_missing_unicode_paths() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("invalid.bin"))?,
            vec![0xff, 0xfe, 0xfd],
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        fs.write_file(
            &executor_path_uri(cwd.join("nul.txt"))?,
            b"valid\0but-binary".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        fs.write_file(
            &executor_path_uri(cwd.join("成功🙂.txt"))?,
            "unicode success\n".as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        fs.create_directory(
            &executor_path_uri(cwd.join("directory"))?,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let read_args = |path| json!({"path": path, "max_bytes": 2_048});
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "invalid-response",
                vec![read_call("invalid", read_args("invalid.bin"))],
            ),
            tool_response("nul-response", vec![read_call("nul", read_args("nul.txt"))]),
            tool_response(
                "directory-response",
                vec![read_call("directory", read_args("directory"))],
            ),
            tool_response(
                "unicode-response",
                vec![read_call("unicode", read_args("成功🙂.txt"))],
            ),
            tool_response(
                "missing-response",
                vec![read_call("missing", read_args("不存在.txt"))],
            ),
            assistant_response("diagnostic-response", "diagnostic-message"),
        ],
    )
    .await;

    test.submit_turn("check file diagnostics").await?;

    assert!(output(&mock, "invalid")?.contains("not valid UTF-8"));
    assert!(output(&mock, "nul")?.contains("NUL"));
    assert!(output(&mock, "directory")?.contains("regular files"));
    assert_eq!(
        response_json(&mock, "unicode")?["window"]["text"],
        "unicode success\n"
    );
    let missing = output(&mock, "missing")?;
    assert!(missing.contains("could not find"));
    let quoted_missing_path = serde_json::to_string("不存在.txt")?;
    assert!(missing.contains(quoted_missing_path.trim_matches('"')));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_rejects_a_fifo_without_opening_or_blocking() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "FIFO setup is host-local");

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, _fs| async move {
        let fifo = cwd.as_path().join("pipe");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status()?;
        anyhow::ensure!(status.success(), "mkfifo failed for {}", fifo.display());
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "fifo-response",
                vec![read_call(
                    "fifo-read",
                    json!({"path": "pipe", "max_bytes": 2_048}),
                )],
            ),
            assistant_response("fifo-complete", "fifo-message"),
        ],
    )
    .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        test.submit_turn("inspect the fifo"),
    )
    .await??;
    assert!(output(&mock, "fifo-read")?.contains("regular files"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_honors_filesystem_sandbox_denials() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("denied.txt"))?,
            b"should not be readable".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let denied_path = test.workspace_path_uri("denied.txt")?;
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry::new(
            FileSystemPath::from(denied_path),
            FileSystemAccessMode::Deny,
        )]),
        NetworkSandboxPolicy::Restricted,
    );
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "sandbox-response",
                vec![read_call(
                    "sandbox-read",
                    json!({"path": "denied.txt", "max_bytes": 2_048}),
                )],
            ),
            assistant_response("sandbox-complete", "sandbox-message"),
        ],
    )
    .await;
    test.submit_turn_with_permission_profile("read the denied file", profile)
        .await?;
    let diagnostic = output(&mock, "sandbox-read")?;
    assert!(diagnostic.contains("sandbox") || diagnostic.contains("denied"));
    assert!(!diagnostic.contains("should not be readable"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a configured remote executor; run in the Docker or Wine remote-test matrix"]
async fn read_file_selects_the_requested_environment() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let environment_manager = test.thread_manager.environment_manager();
    let local_environment = environment_manager
        .get_environment(LOCAL_ENVIRONMENT_ID)
        .ok_or_else(|| {
            anyhow::anyhow!("remote/local builder did not create the local environment")
        })?;
    let local_root = tempfile::tempdir()?;
    let local_cwd = PathUri::from_host_native_path(local_root.path())?;
    local_environment
        .get_filesystem()
        .write_file(
            &local_cwd.join("local-selection.txt")?,
            b"selected local environment\n".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    let remote_selection = test.executor_environment().selection().clone();
    anyhow::ensure!(
        remote_selection.environment_id != LOCAL_ENVIRONMENT_ID,
        "remote/local builder selected the local environment for the remote executor test"
    );
    let local_selection = TurnEnvironmentSelection {
        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
        cwd: local_cwd.clone(),
        workspace_roots: vec![local_cwd],
        config: EnvironmentConfigState::FromThread,
    };
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "environment-response",
                vec![read_call(
                    "local-selection-read",
                    json!({
                        "path": "local-selection.txt",
                        "environment_id": LOCAL_ENVIRONMENT_ID,
                        "max_bytes": 2_048,
                    }),
                )],
            ),
            assistant_response("environment-complete", "environment-message"),
        ],
    )
    .await;
    test.submit_turn_with_environments(
        "read from the local selection",
        Some(vec![remote_selection, local_selection]),
    )
    .await?;
    assert_eq!(
        response_json(&mock, "local-selection-read")?["window"]["text"],
        "selected local environment\n"
    );
    Ok(())
}
