use super::*;
use codex_skills_extension::SkillProviderSource;
use pretty_assertions::assert_eq;

fn provider(catalog: SkillCatalog) -> Arc<StaticSkillProvider> {
    Arc::new(StaticSkillProvider {
        catalog,
        read_requests: Arc::new(Mutex::new(Vec::new())),
        list_calls: None,
        fail_first_list: false,
    })
}

fn catalog_entry(kind: SkillSourceKind, authority: &str, name: &str) -> SkillCatalogEntry {
    let package = format!("{authority}/{name}");
    SkillCatalogEntry::new(
        SkillPackageId(package.clone()),
        SkillAuthority::new(kind, authority),
        name,
        format!("long routing description for {name}"),
        SkillResourceId::new(format!("skill://{package}/SKILL.md")),
    )
    .with_display_path(format!("skill://{package}/SKILL.md"))
    .with_short_description(Some(format!("short {name}")))
}

#[tokio::test]
async fn compact_catalog_feature_reaches_every_surface_and_preserves_explicit_loading() -> TestResult
{
    let custom_kind = SkillSourceKind::Custom("catalog-test".to_string());
    let custom_catalog = SkillCatalog {
        entries: vec![
            catalog_entry(custom_kind.clone(), "custom", "zeta"),
            catalog_entry(custom_kind.clone(), "custom", "alpha"),
        ],
        warnings: Vec::new(),
    };
    let executor_catalog = SkillCatalog {
        entries: vec![catalog_entry(
            SkillSourceKind::Executor,
            "env-1",
            "executor-skill",
        )],
        warnings: Vec::new(),
    };
    let orchestrator_catalog = SkillCatalog {
        entries: vec![catalog_entry(
            SkillSourceKind::Orchestrator,
            "codex_apps",
            "orchestrator-skill",
        )],
        warnings: Vec::new(),
    };
    let host_catalog = SkillCatalog {
        entries: vec![catalog_entry(SkillSourceKind::Host, "host", "host-skill")],
        warnings: Vec::new(),
    };
    let providers = SkillProviders::new()
        .with_provider(SkillProviderSource::new(
            custom_kind,
            "custom",
            provider(custom_catalog),
        ))
        .with_executor_provider(provider(executor_catalog))
        .with_orchestrator_provider(provider(orchestrator_catalog))
        .with_host_provider(provider(host_catalog));
    let mut builder = ExtensionRegistryBuilder::new();
    install_with_providers(&mut builder, providers, skills_extension_config);
    let registry = builder.build();

    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let mut config = default_config();
    config.catalog_selection_enabled = true;
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    let thread_context = registry.context_contributors()[0]
        .contribute_thread_context(&session_store, &thread_store)
        .await;
    let thread_body = thread_context[0].text();
    assert!(thread_body.find("alpha").expect("alpha") < thread_body.find("zeta").expect("zeta"));
    assert!(thread_body.contains("short alpha"));
    assert!(!thread_body.contains("long routing description"));

    let turn_store = ExtensionData::new("turn-1");
    let turn_fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Text {
                    text: "$alpha".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &turn_store,
        )
        .await;
    assert_eq!(turn_fragments.len(), 2);
    assert!(turn_fragments[0].render().contains("short alpha"));
    assert!(turn_fragments[1].render().contains("<name>alpha</name>"));
    assert!(turn_fragments[1].render().contains("# Lint Fix"));

    let world_turn_store = ExtensionData::new("turn-2");
    world_turn_store.insert(HostSkillsSnapshot::new(Arc::new(
        SkillLoadOutcome::default(),
    )));
    let selected_roots = [SelectedCapabilityRoot {
        id: "executor-skill".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: "env-1".to_string(),
            path: PathUri::parse("file:///skills/executor-skill")?,
        },
    }];
    let sections = registry.context_contributors()[0]
        .contribute_world_state(WorldStateContributionInput {
            thread_id: codex_protocol::ThreadId::new(),
            turn_id: "turn-2",
            environments: &[],
            ready_selected_capability_roots: &selected_roots,
            executor_capability_discovery: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &world_turn_store,
        })
        .await;
    for (id, name) in [
        ("skills", "executor-skill"),
        ("orchestrator_skills", "orchestrator-skill"),
        ("host_skills", "host-skill"),
    ] {
        let fragment = world_state_section(&sections, id)
            .render_diff(PreviousWorldStateSection::Absent)
            .ok_or("catalog should render")?;
        let body = fragment.body();
        assert!(body.contains(name));
        assert!(body.contains(&format!("short {name}")));
        assert!(!body.contains("long routing description"));
    }

    Ok(())
}
