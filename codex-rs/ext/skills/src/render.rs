use std::borrow::Cow;

use codex_utils_string::take_bytes_at_char_boundary;

use crate::aliases::AliasPlan;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::catalog_prompt::SkillPromptKind;
use crate::catalog_prompt::available_skills_body_bytes;
use crate::catalog_prompt::render_available_skills_body;
use crate::fragments::AvailableSkillsInstructions;
use crate::host_aliases::shared_host_alias_roots;
use crate::render_policy::SkillCatalogRenderOutcome;
use crate::render_policy::SkillCatalogRenderPolicy;
use crate::render_policy::SkillMetadataBudget;
use crate::render_policy::SkillRenderReport;
use crate::render_policy::SkillRenderSize;
use crate::render_policy::catalog_render_policy;
use crate::render_policy::metadata_line_cost;

const MAX_SKILL_PROMPT_BYTES: usize = 8_000;
const MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1_024;
const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";
pub(crate) const MAX_SKILL_NAME_BYTES: usize = 256;
pub(crate) const MAX_SKILL_PATH_BYTES: usize = 1_024;

struct SkillLine<'a> {
    name: &'a str,
    description: Cow<'a, str>,
    full_description: Cow<'a, str>,
    locator: String,
    locator_kind: &'static str,
}

impl<'a> SkillLine<'a> {
    fn new(entry: &'a SkillCatalogEntry, policy: SkillCatalogRenderPolicy) -> Self {
        Self::new_with_full_policy(entry, policy, policy)
    }

    fn new_with_full_policy(
        entry: &'a SkillCatalogEntry,
        policy: SkillCatalogRenderPolicy,
        full_policy: SkillCatalogRenderPolicy,
    ) -> Self {
        let locator = match &entry.authority.kind {
            SkillSourceKind::Executor | SkillSourceKind::Orchestrator => entry.id.0.as_str(),
            SkillSourceKind::Host | SkillSourceKind::Custom(_) => entry.rendered_path(),
        };
        Self::with_locator_and_full_policy(entry, policy, full_policy, locator.to_string())
    }

    #[cfg(test)]
    fn with_locator(
        entry: &'a SkillCatalogEntry,
        policy: SkillCatalogRenderPolicy,
        locator: String,
    ) -> Self {
        Self::with_locator_and_full_policy(entry, policy, policy, locator)
    }

    fn with_locator_and_full_policy(
        entry: &'a SkillCatalogEntry,
        policy: SkillCatalogRenderPolicy,
        full_policy: SkillCatalogRenderPolicy,
        locator: String,
    ) -> Self {
        let description = policy.description(entry);
        let full_description = full_policy.description(entry);
        Self {
            name: entry.name.as_str(),
            description: truncate_catalog_skill_description(description),
            full_description: truncate_catalog_skill_description(full_description),
            locator,
            locator_kind: match &entry.authority.kind {
                SkillSourceKind::Host => "file",
                SkillSourceKind::Executor => "executor package",
                SkillSourceKind::Orchestrator => "orchestrator package",
                SkillSourceKind::Custom(_) => "custom resource",
            },
        }
    }

    fn full_cost(&self, budget: SkillMetadataBudget) -> usize {
        metadata_line_cost(budget, &self.render_full())
    }

    fn minimum_cost(&self, budget: SkillMetadataBudget) -> usize {
        metadata_line_cost(budget, &self.render_minimum())
    }

    fn description_char_count(&self) -> usize {
        self.description.chars().count()
    }

    fn render_full(&self) -> String {
        self.render_with_description(self.description.as_ref())
    }

    fn render_baseline(&self) -> String {
        self.render_with_description(self.full_description.as_ref())
    }

    fn render_minimum(&self) -> String {
        self.render_with_description("")
    }

    fn render_with_description_chars(&self, description_chars: usize) -> String {
        let end = self
            .description
            .char_indices()
            .nth(description_chars)
            .map_or(self.description.len(), |(index, _)| index);
        self.render_with_description(&self.description[..end])
    }

    fn render_with_description(&self, description: &str) -> String {
        let name = self.name;
        let locator = self.locator.as_str();
        let locator_kind = self.locator_kind;
        if description.is_empty() {
            format!("- {name}: ({locator_kind}: {locator})")
        } else {
            format!("- {name}: {description} ({locator_kind}: {locator})")
        }
    }
}

struct RenderedSkillLine {
    line: String,
}

struct RenderedSkillLines {
    lines: Vec<RenderedSkillLine>,
    omitted_count: usize,
    truncated_description_chars: usize,
    truncated_description_count: usize,
}

struct DescriptionBudgetLine {
    description_char_count: usize,
    extra_costs: Vec<usize>,
}

impl DescriptionBudgetLine {
    fn new(line: &SkillLine<'_>, budget: SkillMetadataBudget) -> Self {
        let minimum_line = line.render_minimum();
        let minimum_chars = minimum_line.chars().count().saturating_add(1);
        let minimum_bytes = minimum_line.len().saturating_add(1);
        let minimum_cost = budget.cost_from_counts(minimum_chars, minimum_bytes);

        let description_char_count = line.description.chars().count();
        let mut extra_costs = Vec::with_capacity(description_char_count.saturating_add(1));
        extra_costs.push(0);

        let mut prefix_chars = 0usize;
        let mut prefix_bytes = 0usize;
        for ch in line.description.chars() {
            prefix_chars = prefix_chars.saturating_add(1);
            prefix_bytes = prefix_bytes.saturating_add(ch.len_utf8());
            let rendered_chars = minimum_chars.saturating_add(prefix_chars).saturating_add(1);
            let rendered_bytes = minimum_bytes.saturating_add(prefix_bytes).saturating_add(1);
            let cost = budget
                .cost_from_counts(rendered_chars, rendered_bytes)
                .saturating_sub(minimum_cost);
            extra_costs.push(cost);
        }

        Self {
            description_char_count,
            extra_costs,
        }
    }
}

#[derive(Clone, Copy)]
enum SkillLineAllocation {
    Omitted,
    DescriptionChars(usize),
}

fn render_skill_lines(
    skill_lines: Vec<SkillLine<'_>>,
    budget: SkillMetadataBudget,
) -> RenderedSkillLines {
    let allocations = allocate_skill_lines(&skill_lines, budget);
    render_allocated_skill_lines(&skill_lines, &allocations)
}

fn allocate_skill_lines(
    skill_lines: &[SkillLine<'_>],
    budget: SkillMetadataBudget,
) -> Vec<SkillLineAllocation> {
    let full_cost = skill_lines.iter().fold(0usize, |used, line| {
        used.saturating_add(line.full_cost(budget))
    });
    if full_cost <= budget.limit() {
        return skill_lines
            .iter()
            .map(|line| SkillLineAllocation::DescriptionChars(line.description_char_count()))
            .collect();
    }

    let minimum_cost = skill_lines.iter().fold(0usize, |used, line| {
        used.saturating_add(line.minimum_cost(budget))
    });
    if minimum_cost <= budget.limit() {
        return allocate_description_chars(
            budget,
            skill_lines,
            budget.limit().saturating_sub(minimum_cost),
        )
        .into_iter()
        .map(SkillLineAllocation::DescriptionChars)
        .collect();
    }

    let mut used = 0usize;
    skill_lines
        .iter()
        .map(|line| {
            let next_used = used.saturating_add(line.minimum_cost(budget));
            if next_used <= budget.limit() {
                used = next_used;
                SkillLineAllocation::DescriptionChars(0)
            } else {
                SkillLineAllocation::Omitted
            }
        })
        .collect()
}

fn render_allocated_skill_lines(
    skill_lines: &[SkillLine<'_>],
    allocations: &[SkillLineAllocation],
) -> RenderedSkillLines {
    let mut lines = Vec::new();
    let mut omitted_count = 0usize;
    let mut truncated_description_chars = 0usize;
    let mut truncated_description_count = 0usize;
    for (line, allocation) in skill_lines.iter().zip(allocations) {
        let description_char_count = line.description_char_count();
        match allocation {
            SkillLineAllocation::Omitted => {
                omitted_count = omitted_count.saturating_add(1);
                truncated_description_chars =
                    truncated_description_chars.saturating_add(description_char_count);
                if description_char_count > 0 {
                    truncated_description_count = truncated_description_count.saturating_add(1);
                }
            }
            SkillLineAllocation::DescriptionChars(description_chars) => {
                let truncated_chars = description_char_count.saturating_sub(*description_chars);
                if truncated_chars > 0 {
                    truncated_description_chars =
                        truncated_description_chars.saturating_add(truncated_chars);
                    truncated_description_count = truncated_description_count.saturating_add(1);
                }
                lines.push(RenderedSkillLine {
                    line: line.render_with_description_chars(*description_chars),
                });
            }
        }
    }
    RenderedSkillLines {
        lines,
        omitted_count,
        truncated_description_chars,
        truncated_description_count,
    }
}

fn allocate_description_chars(
    budget: SkillMetadataBudget,
    skill_lines: &[SkillLine<'_>],
    limit: usize,
) -> Vec<usize> {
    let budget_lines = skill_lines
        .iter()
        .map(|line| DescriptionBudgetLine::new(line, budget))
        .collect::<Vec<_>>();
    let mut char_allocations = vec![0usize; budget_lines.len()];
    let mut current_extra_costs = vec![0usize; budget_lines.len()];
    let mut remaining = limit;

    // Distribute description space round-robin so no skill monopolizes the
    // remaining budget.
    loop {
        let mut changed = false;
        for (index, line) in budget_lines.iter().enumerate() {
            if char_allocations[index] >= line.description_char_count {
                continue;
            }

            let next_chars = char_allocations[index].saturating_add(1);
            let next_cost = line.extra_costs[next_chars];
            let delta = next_cost.saturating_sub(current_extra_costs[index]);
            if delta <= remaining {
                char_allocations[index] = next_chars;
                current_extra_costs[index] = next_cost;
                remaining = remaining.saturating_sub(delta);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    char_allocations
}

struct RenderedCatalog {
    prompt_kind: SkillPromptKind,
    skill_root_lines: Vec<String>,
    skill_lines: Vec<String>,
    report: SkillRenderReport,
}

impl RenderedCatalog {
    fn body_bytes(&self, include_skills_usage_instructions: bool) -> usize {
        available_skills_body_bytes(
            self.prompt_kind,
            &self.skill_root_lines,
            &self.skill_lines,
            include_skills_usage_instructions,
        )
    }
}

pub(crate) struct AvailableSkillsRender {
    prompt_kind: SkillPromptKind,
    skill_root_lines: Vec<String>,
    skill_lines: Vec<String>,
    preserve_empty_fragment: bool,
    pub(crate) report: SkillRenderReport,
    pub(crate) size: SkillRenderSize,
}

#[derive(Default)]
pub(crate) struct RenderedSkillCatalogs {
    pub(crate) executor: Option<AvailableSkillsRender>,
    pub(crate) orchestrator: Option<AvailableSkillsRender>,
    pub(crate) host: Option<AvailableSkillsRender>,
}

impl RenderedSkillCatalogs {
    fn with_outcome(mut self, outcome: SkillCatalogRenderOutcome) -> Self {
        for rendered in [
            self.executor.as_mut(),
            self.orchestrator.as_mut(),
            self.host.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            rendered.size = rendered.size.with_outcome(outcome);
        }
        self
    }
}

impl AvailableSkillsRender {
    pub(crate) fn into_fragment(
        self,
        include_skills_usage_instructions: bool,
    ) -> Option<AvailableSkillsInstructions> {
        (self.preserve_empty_fragment || !self.skill_lines.is_empty()).then(|| {
            AvailableSkillsInstructions::from_skill_lines(
                self.prompt_kind,
                self.skill_root_lines,
                self.skill_lines,
                include_skills_usage_instructions,
            )
        })
    }
}

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(catalog_entry_count = catalog.entries.len())
)]
pub(crate) fn render_available_skills(
    catalog: &SkillCatalog,
    policy: SkillCatalogRenderPolicy,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> Option<AvailableSkillsRender> {
    let full_policy = match policy {
        SkillCatalogRenderPolicy::StableCompact => SkillCatalogRenderPolicy::ExtensionCompatible,
        policy => policy,
    };
    render_available_skills_with_full_policy(
        catalog,
        policy,
        full_policy,
        budget,
        include_skills_usage_instructions,
    )
}

fn render_available_skills_with_full_policy(
    catalog: &SkillCatalog,
    policy: SkillCatalogRenderPolicy,
    full_policy: SkillCatalogRenderPolicy,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> Option<AvailableSkillsRender> {
    let mut entries = catalog
        .entries
        .iter()
        .filter(|entry| entry.is_model_visible())
        .collect::<Vec<_>>();
    policy.order_entries(&mut entries);
    if entries.is_empty() {
        return None;
    }

    let absolute = render_catalog(
        entries
            .iter()
            .map(|entry| SkillLine::new(entry, policy))
            .collect(),
        budget,
        Vec::new(),
        SkillPromptKind::Unaliased,
        policy,
    );
    let full_skill_lines = entries
        .iter()
        .map(|entry| SkillLine::new_with_full_policy(entry, policy, full_policy).render_baseline())
        .collect::<Vec<_>>();
    let full_body_bytes = available_skills_body_bytes(
        SkillPromptKind::Unaliased,
        &[],
        &full_skill_lines,
        include_skills_usage_instructions,
    );
    let selected =
        if absolute.report.omitted_count == 0 && absolute.report.truncated_description_chars == 0 {
            absolute
        } else if let Some(aliased) = build_aliased_catalog(
            &entries,
            policy,
            full_policy,
            budget,
            include_skills_usage_instructions,
        ) && aliased_render_is_better(
            &aliased,
            &absolute,
            budget,
            include_skills_usage_instructions,
        ) {
            aliased
        } else {
            absolute
        };
    if policy == SkillCatalogRenderPolicy::StableCompact && selected.report.omitted_count > 0 {
        return render_available_skills_with_full_policy(
            catalog,
            full_policy,
            full_policy,
            budget,
            include_skills_usage_instructions,
        )
        .map(|mut rendered| {
            rendered.size = rendered
                .size
                .with_outcome(SkillCatalogRenderOutcome::Fallback);
            rendered
        });
    }
    let rendered_body_bytes = selected.body_bytes(include_skills_usage_instructions);

    Some(AvailableSkillsRender {
        prompt_kind: selected.prompt_kind,
        skill_root_lines: selected.skill_root_lines,
        skill_lines: selected.skill_lines,
        preserve_empty_fragment: policy == SkillCatalogRenderPolicy::CoreCompatible,
        report: selected.report,
        size: SkillRenderSize {
            full_body_bytes,
            rendered_body_bytes,
            full_body_tokens: full_body_bytes.saturating_add(3) / 4,
            rendered_body_tokens: rendered_body_bytes.saturating_add(3) / 4,
            outcome: Some(policy.outcome()),
        },
    })
}

pub(crate) fn render_combined_available_skills(
    executor_catalog: &SkillCatalog,
    orchestrator_catalog: &SkillCatalog,
    host_catalog: &SkillCatalog,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
    stable_compact: bool,
) -> RenderedSkillCatalogs {
    let mut executor_entries = executor_catalog
        .entries
        .iter()
        .filter(|entry| entry.is_model_visible())
        .collect::<Vec<_>>();
    let mut orchestrator_entries = orchestrator_catalog
        .entries
        .iter()
        .filter(|entry| entry.is_model_visible())
        .collect::<Vec<_>>();
    let mut host_entries = host_catalog
        .entries
        .iter()
        .filter(|entry| entry.is_model_visible())
        .collect::<Vec<_>>();
    let extension_policy = catalog_render_policy(stable_compact);
    let host_policy = if stable_compact {
        SkillCatalogRenderPolicy::StableCompact
    } else {
        SkillCatalogRenderPolicy::CoreCompatible
    };
    extension_policy.order_entries(&mut executor_entries);
    extension_policy.order_entries(&mut orchestrator_entries);
    host_policy.order_entries(&mut host_entries);
    let nonempty_catalog_count = [
        !executor_entries.is_empty(),
        !orchestrator_entries.is_empty(),
        !host_entries.is_empty(),
    ]
    .into_iter()
    .filter(|nonempty| *nonempty)
    .count();
    let extension_full_policy = SkillCatalogRenderPolicy::ExtensionCompatible;
    let host_full_policy = SkillCatalogRenderPolicy::CoreCompatible;
    if nonempty_catalog_count <= 1 {
        return RenderedSkillCatalogs {
            executor: render_available_skills_with_full_policy(
                executor_catalog,
                extension_policy,
                extension_full_policy,
                budget,
                include_skills_usage_instructions,
            ),
            orchestrator: render_available_skills_with_full_policy(
                orchestrator_catalog,
                extension_policy,
                extension_full_policy,
                budget,
                include_skills_usage_instructions,
            ),
            host: render_available_skills_with_full_policy(
                host_catalog,
                host_policy,
                host_full_policy,
                budget,
                include_skills_usage_instructions,
            ),
        }
        .with_outcome(extension_policy.outcome());
    }
    let absolute = render_combined_lines(
        CatalogLines::unaliased(&executor_entries, extension_policy, extension_full_policy),
        CatalogLines::unaliased(
            &orchestrator_entries,
            extension_policy,
            extension_full_policy,
        ),
        CatalogLines::unaliased(&host_entries, host_policy, host_full_policy),
        budget,
        include_skills_usage_instructions,
    );

    let mut selected = absolute;
    if !combined_catalog_fully_rendered(&selected) {
        let host_only_aliases = build_aliased_combined_catalog(
            CatalogLines::unaliased(&executor_entries, extension_policy, extension_full_policy),
            CatalogLines::unaliased(
                &orchestrator_entries,
                extension_policy,
                extension_full_policy,
            ),
            CatalogLines::aliased(&host_entries, host_policy, host_full_policy),
            budget,
            include_skills_usage_instructions,
        );
        let executor_only_aliases = build_aliased_combined_catalog(
            CatalogLines::aliased(&executor_entries, extension_policy, extension_full_policy),
            CatalogLines::unaliased(
                &orchestrator_entries,
                extension_policy,
                extension_full_policy,
            ),
            CatalogLines::unaliased(&host_entries, host_policy, host_full_policy),
            budget,
            include_skills_usage_instructions,
        );
        let orchestrator_only_aliases = build_aliased_combined_catalog(
            CatalogLines::unaliased(&executor_entries, extension_policy, extension_full_policy),
            CatalogLines::aliased(
                &orchestrator_entries,
                extension_policy,
                extension_full_policy,
            ),
            CatalogLines::unaliased(&host_entries, host_policy, host_full_policy),
            budget,
            include_skills_usage_instructions,
        );
        let all_source_aliases = build_aliased_combined_catalog(
            CatalogLines::aliased(&executor_entries, extension_policy, extension_full_policy),
            CatalogLines::aliased(
                &orchestrator_entries,
                extension_policy,
                extension_full_policy,
            ),
            CatalogLines::aliased(&host_entries, host_policy, host_full_policy),
            budget,
            include_skills_usage_instructions,
        );

        for candidate in [
            host_only_aliases,
            executor_only_aliases,
            orchestrator_only_aliases,
            all_source_aliases,
        ]
        .into_iter()
        .flatten()
        {
            if combined_render_is_better(
                &candidate,
                &selected,
                budget,
                include_skills_usage_instructions,
            ) {
                selected = candidate;
            }
        }
    }

    if stable_compact && !combined_catalog_fully_rendered(&selected) {
        return render_combined_available_skills(
            executor_catalog,
            orchestrator_catalog,
            host_catalog,
            budget,
            include_skills_usage_instructions,
            /*stable_compact*/ false,
        )
        .with_outcome(SkillCatalogRenderOutcome::Fallback);
    }

    RenderedSkillCatalogs {
        executor: Some(selected.executor),
        orchestrator: Some(selected.orchestrator),
        host: Some(selected.host),
    }
    .with_outcome(extension_policy.outcome())
}

struct CombinedAvailableSkillsRender {
    executor: AvailableSkillsRender,
    orchestrator: AvailableSkillsRender,
    host: AvailableSkillsRender,
}

struct CatalogLines<'a> {
    prompt_kind: SkillPromptKind,
    skills: Vec<SkillLine<'a>>,
    root_lines: Vec<String>,
}

impl<'a> CatalogLines<'a> {
    fn unaliased(
        entries: &[&'a SkillCatalogEntry],
        policy: SkillCatalogRenderPolicy,
        full_policy: SkillCatalogRenderPolicy,
    ) -> Self {
        Self {
            prompt_kind: SkillPromptKind::Unaliased,
            skills: entries
                .iter()
                .map(|entry| SkillLine::new_with_full_policy(entry, policy, full_policy))
                .collect(),
            root_lines: Vec::new(),
        }
    }

    fn aliased(
        entries: &[&'a SkillCatalogEntry],
        policy: SkillCatalogRenderPolicy,
        full_policy: SkillCatalogRenderPolicy,
    ) -> Self {
        let Some(plan) = build_alias_plan(entries) else {
            return Self::unaliased(entries, policy, full_policy);
        };

        Self {
            prompt_kind: entries
                .first()
                .map(|entry| SkillPromptKind::for_aliased_source(&entry.authority.kind))
                .unwrap_or(SkillPromptKind::Unaliased),
            skills: entries
                .iter()
                .map(|entry| {
                    SkillLine::with_locator_and_full_policy(
                        entry,
                        policy,
                        full_policy,
                        render_skill_locator_with_aliases(entry, &plan),
                    )
                })
                .collect(),
            root_lines: plan.root_lines(),
        }
    }
}

fn render_combined_lines(
    executor: CatalogLines<'_>,
    orchestrator: CatalogLines<'_>,
    host: CatalogLines<'_>,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> CombinedAvailableSkillsRender {
    let executor_end = executor.skills.len();
    let orchestrator_end = executor_end.saturating_add(orchestrator.skills.len());
    let mut lines = executor.skills;
    lines.extend(orchestrator.skills);
    lines.extend(host.skills);
    let mut allocations = allocate_skill_lines(&lines, budget);
    let omission_marker = reserve_non_host_omission_marker(
        &lines,
        executor_end,
        orchestrator_end,
        budget,
        &mut allocations,
    );
    let (executor_omission_marker, orchestrator_omission_marker) =
        if executor_end == orchestrator_end {
            (omission_marker, None)
        } else {
            (None, omission_marker)
        };

    CombinedAvailableSkillsRender {
        executor: render_combined_group(
            &lines[..executor_end],
            &allocations[..executor_end],
            executor.prompt_kind,
            executor.root_lines,
            executor_omission_marker,
            include_skills_usage_instructions,
        ),
        orchestrator: render_combined_group(
            &lines[executor_end..orchestrator_end],
            &allocations[executor_end..orchestrator_end],
            orchestrator.prompt_kind,
            orchestrator.root_lines,
            orchestrator_omission_marker,
            include_skills_usage_instructions,
        ),
        host: render_combined_group(
            &lines[orchestrator_end..],
            &allocations[orchestrator_end..],
            host.prompt_kind,
            host.root_lines,
            /*omission_marker*/ None,
            include_skills_usage_instructions,
        ),
    }
}

fn render_combined_group(
    skill_lines: &[SkillLine<'_>],
    allocations: &[SkillLineAllocation],
    prompt_kind: SkillPromptKind,
    skill_root_lines: Vec<String>,
    omission_marker: Option<String>,
    include_skills_usage_instructions: bool,
) -> AvailableSkillsRender {
    let RenderedSkillLines {
        mut lines,
        omitted_count,
        truncated_description_chars,
        truncated_description_count,
    } = render_allocated_skill_lines(skill_lines, allocations);
    if let Some(marker) = omission_marker {
        lines.push(RenderedSkillLine { line: marker });
    }
    let rendered_skill_lines = lines
        .iter()
        .map(|rendered| rendered.line.clone())
        .collect::<Vec<_>>();
    let full_skill_lines = skill_lines
        .iter()
        .map(SkillLine::render_baseline)
        .collect::<Vec<_>>();
    let full_body_bytes = available_skills_body_bytes(
        prompt_kind,
        &skill_root_lines,
        &full_skill_lines,
        include_skills_usage_instructions,
    );
    let rendered_body_bytes = available_skills_body_bytes(
        prompt_kind,
        &skill_root_lines,
        &rendered_skill_lines,
        include_skills_usage_instructions,
    );
    AvailableSkillsRender {
        prompt_kind,
        skill_root_lines,
        skill_lines: rendered_skill_lines,
        preserve_empty_fragment: false,
        report: SkillRenderReport {
            total_count: skill_lines.len(),
            included_count: skill_lines.len().saturating_sub(omitted_count),
            omitted_count,
            truncated_description_chars,
            truncated_description_count,
        },
        size: SkillRenderSize {
            full_body_bytes,
            rendered_body_bytes,
            full_body_tokens: full_body_bytes.saturating_add(3) / 4,
            rendered_body_tokens: rendered_body_bytes.saturating_add(3) / 4,
            outcome: None,
        },
    }
}

fn build_aliased_combined_catalog(
    executor: CatalogLines<'_>,
    orchestrator: CatalogLines<'_>,
    host: CatalogLines<'_>,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> Option<CombinedAvailableSkillsRender> {
    if [
        &executor.root_lines,
        &orchestrator.root_lines,
        &host.root_lines,
    ]
    .into_iter()
    .all(Vec::is_empty)
    {
        return None;
    }

    let table_cost = [&executor, &orchestrator, &host]
        .into_iter()
        .filter(|catalog| !catalog.root_lines.is_empty())
        .map(|catalog| {
            aliased_metadata_overhead_cost(
                budget,
                catalog.prompt_kind,
                &catalog.root_lines,
                include_skills_usage_instructions,
            )
        })
        .fold(0usize, usize::saturating_add);
    if table_cost >= budget.limit() {
        return None;
    }

    let adjusted_limit = budget.limit().saturating_sub(table_cost);
    let adjusted_budget = match budget {
        SkillMetadataBudget::Tokens(_) => SkillMetadataBudget::Tokens(adjusted_limit),
        SkillMetadataBudget::Characters(_) => SkillMetadataBudget::Characters(adjusted_limit),
    };
    Some(render_combined_lines(
        executor,
        orchestrator,
        host,
        adjusted_budget,
        include_skills_usage_instructions,
    ))
}

fn combined_catalog_fully_rendered(rendered: &CombinedAvailableSkillsRender) -> bool {
    [&rendered.executor, &rendered.orchestrator, &rendered.host]
        .into_iter()
        .all(|catalog| {
            catalog.report.omitted_count == 0 && catalog.report.truncated_description_chars == 0
        })
}

fn combined_render_is_better(
    candidate: &CombinedAvailableSkillsRender,
    current: &CombinedAvailableSkillsRender,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> bool {
    let priority = |rendered: &CombinedAvailableSkillsRender| {
        (
            rendered.executor.report.included_count,
            rendered.orchestrator.report.included_count,
            rendered.host.report.included_count,
        )
    };
    if priority(candidate) != priority(current) {
        return priority(candidate) > priority(current);
    }

    let truncated_chars = |rendered: &CombinedAvailableSkillsRender| {
        [&rendered.executor, &rendered.orchestrator, &rendered.host]
            .into_iter()
            .fold(0usize, |total, catalog| {
                total.saturating_add(catalog.report.truncated_description_chars)
            })
    };
    if truncated_chars(candidate) != truncated_chars(current) {
        return truncated_chars(candidate) < truncated_chars(current);
    }

    combined_available_skills_cost(budget, candidate, include_skills_usage_instructions)
        < combined_available_skills_cost(budget, current, include_skills_usage_instructions)
}

fn combined_available_skills_cost(
    budget: SkillMetadataBudget,
    rendered: &CombinedAvailableSkillsRender,
    include_skills_usage_instructions: bool,
) -> usize {
    [&rendered.executor, &rendered.orchestrator, &rendered.host]
        .into_iter()
        .fold(0usize, |used, catalog| {
            let root_cost = if !catalog.skill_root_lines.is_empty() {
                aliased_metadata_overhead_cost(
                    budget,
                    catalog.prompt_kind,
                    &catalog.skill_root_lines,
                    include_skills_usage_instructions,
                )
            } else {
                Default::default()
            };
            catalog
                .skill_lines
                .iter()
                .fold(used.saturating_add(root_cost), |used, line| {
                    used.saturating_add(metadata_line_cost(budget, line))
                })
        })
}

fn reserve_non_host_omission_marker(
    skill_lines: &[SkillLine<'_>],
    executor_end: usize,
    orchestrator_end: usize,
    budget: SkillMetadataBudget,
    allocations: &mut [SkillLineAllocation],
) -> Option<String> {
    loop {
        let omitted_count = allocations[..orchestrator_end]
            .iter()
            .filter(|allocation| matches!(allocation, SkillLineAllocation::Omitted))
            .count();
        if omitted_count == 0 {
            return None;
        }

        let marker = omission_marker(omitted_count);
        let used = allocated_skill_lines_cost(skill_lines, allocations, budget);
        if used.saturating_add(metadata_line_cost(budget, &marker)) <= budget.limit() {
            return Some(marker);
        }

        let index = (orchestrator_end..allocations.len())
            .rev()
            .chain((executor_end..orchestrator_end).rev())
            .chain((0..executor_end).rev())
            .find(|index| {
                matches!(
                    allocations[*index],
                    SkillLineAllocation::DescriptionChars(_)
                )
            })?;
        allocations[index] = SkillLineAllocation::Omitted;
    }
}

fn allocated_skill_lines_cost(
    skill_lines: &[SkillLine<'_>],
    allocations: &[SkillLineAllocation],
    budget: SkillMetadataBudget,
) -> usize {
    skill_lines
        .iter()
        .zip(allocations)
        .fold(0usize, |used, (line, allocation)| match allocation {
            SkillLineAllocation::Omitted => used,
            SkillLineAllocation::DescriptionChars(description_chars) => {
                used.saturating_add(metadata_line_cost(
                    budget,
                    &line.render_with_description_chars(*description_chars),
                ))
            }
        })
}

fn render_catalog(
    skill_lines: Vec<SkillLine<'_>>,
    budget: SkillMetadataBudget,
    skill_root_lines: Vec<String>,
    prompt_kind: SkillPromptKind,
    policy: SkillCatalogRenderPolicy,
) -> RenderedCatalog {
    let total_count = skill_lines.len();
    let RenderedSkillLines {
        lines: mut rendered_lines,
        omitted_count: mut omitted,
        truncated_description_chars,
        truncated_description_count,
    } = render_skill_lines(skill_lines, budget);
    let mut total_cost = rendered_lines.iter().fold(0usize, |used, rendered| {
        used.saturating_add(metadata_line_cost(budget, &rendered.line))
    });

    if omitted > 0 && policy.includes_omission_notice() {
        loop {
            let marker = omission_marker(omitted);
            if total_cost.saturating_add(metadata_line_cost(budget, &marker)) <= budget.limit() {
                rendered_lines.push(RenderedSkillLine { line: marker });
                break;
            }
            let Some(rendered) = rendered_lines.pop() else {
                break;
            };
            total_cost = total_cost.saturating_sub(metadata_line_cost(budget, &rendered.line));
            omitted = omitted.saturating_add(1);
        }
    }

    RenderedCatalog {
        prompt_kind,
        skill_root_lines,
        skill_lines: rendered_lines
            .into_iter()
            .map(|rendered| rendered.line)
            .collect(),
        report: SkillRenderReport {
            total_count,
            included_count: total_count.saturating_sub(omitted),
            omitted_count: omitted,
            truncated_description_chars,
            truncated_description_count,
        },
    }
}

#[cfg(test)]
fn available_skills_fragment(
    catalog: &SkillCatalog,
    include_skills_usage_instructions: bool,
    policy: SkillCatalogRenderPolicy,
    budget: SkillMetadataBudget,
) -> Option<AvailableSkillsInstructions> {
    render_available_skills(catalog, policy, budget, include_skills_usage_instructions)?
        .into_fragment(include_skills_usage_instructions)
}

fn build_aliased_catalog(
    entries: &[&SkillCatalogEntry],
    policy: SkillCatalogRenderPolicy,
    full_policy: SkillCatalogRenderPolicy,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> Option<RenderedCatalog> {
    let catalog = CatalogLines::aliased(entries, policy, full_policy);
    if catalog.root_lines.is_empty() {
        return None;
    }
    let table_cost = aliased_metadata_overhead_cost(
        budget,
        catalog.prompt_kind,
        &catalog.root_lines,
        include_skills_usage_instructions,
    );
    if table_cost >= budget.limit() {
        return None;
    }

    let adjusted_limit = budget.limit().saturating_sub(table_cost);
    let adjusted_budget = match budget {
        SkillMetadataBudget::Tokens(_) => SkillMetadataBudget::Tokens(adjusted_limit),
        SkillMetadataBudget::Characters(_) => SkillMetadataBudget::Characters(adjusted_limit),
    };
    Some(render_catalog(
        catalog.skills,
        adjusted_budget,
        catalog.root_lines,
        catalog.prompt_kind,
        policy,
    ))
}

pub(crate) fn build_alias_plan(entries: &[&SkillCatalogEntry]) -> Option<AliasPlan> {
    let source = &entries.first()?.authority.kind;
    if entries.iter().any(|entry| &entry.authority.kind != source) {
        return None;
    }
    let prefix = match source {
        SkillSourceKind::Host => "r",
        SkillSourceKind::Executor => "e",
        SkillSourceKind::Orchestrator => "o",
        SkillSourceKind::Custom(_) => return None,
    };

    let mut alias_ordered_entries = entries.to_vec();
    alias_ordered_entries.sort_by_key(|entry| entry.alias_root_order().unwrap_or(usize::MAX));
    let roots = match source {
        SkillSourceKind::Host => shared_host_alias_roots(&alias_ordered_entries),
        SkillSourceKind::Executor | SkillSourceKind::Orchestrator => alias_ordered_entries
            .iter()
            .filter_map(|entry| entry.alias_root())
            .map(str::to_string)
            .collect(),
        SkillSourceKind::Custom(_) => return None,
    };
    let roots = roots.iter().map(String::as_str).collect::<Vec<_>>();

    AliasPlan::build(prefix, &roots)
}

fn render_skill_locator_with_aliases(entry: &SkillCatalogEntry, plan: &AliasPlan) -> String {
    let locator = match &entry.authority.kind {
        SkillSourceKind::Executor | SkillSourceKind::Orchestrator => entry.id.0.as_str(),
        SkillSourceKind::Host | SkillSourceKind::Custom(_) => entry.rendered_path(),
    };
    if entry.alias_root().is_none() {
        return locator.to_string();
    }
    plan.shorten(locator).unwrap_or_else(|| locator.to_string())
}

fn aliased_metadata_overhead_cost(
    budget: SkillMetadataBudget,
    prompt_kind: SkillPromptKind,
    skill_root_lines: &[String],
    include_skills_usage_instructions: bool,
) -> usize {
    let empty_skill_lines: &[String] = &[];
    let absolute_body =
        render_available_skills_body(SkillPromptKind::Unaliased, &[], empty_skill_lines);
    let aliased_body =
        render_available_skills_body(prompt_kind, skill_root_lines, empty_skill_lines);
    let alias_instruction_cost = if include_skills_usage_instructions {
        prompt_kind
            .alias_instructions()
            .map_or(0, |instructions| metadata_line_cost(budget, instructions))
    } else {
        0
    };
    budget
        .cost(&aliased_body)
        .saturating_add(alias_instruction_cost)
        .saturating_sub(budget.cost(&absolute_body))
}

fn aliased_render_is_better(
    aliased: &RenderedCatalog,
    absolute: &RenderedCatalog,
    budget: SkillMetadataBudget,
    include_skills_usage_instructions: bool,
) -> bool {
    if aliased.report.included_count != absolute.report.included_count {
        return aliased.report.included_count > absolute.report.included_count;
    }
    if aliased.report.truncated_description_chars != absolute.report.truncated_description_chars {
        return aliased.report.truncated_description_chars
            < absolute.report.truncated_description_chars;
    }
    rendered_catalog_cost(budget, aliased, include_skills_usage_instructions)
        < rendered_catalog_cost(budget, absolute, include_skills_usage_instructions)
}

fn rendered_catalog_cost(
    budget: SkillMetadataBudget,
    rendered: &RenderedCatalog,
    include_skills_usage_instructions: bool,
) -> usize {
    let metadata_cost = if rendered.skill_root_lines.is_empty() {
        0
    } else {
        aliased_metadata_overhead_cost(
            budget,
            rendered.prompt_kind,
            &rendered.skill_root_lines,
            include_skills_usage_instructions,
        )
    };
    rendered
        .skill_lines
        .iter()
        .fold(metadata_cost, |used, line| {
            used.saturating_add(metadata_line_cost(budget, line))
        })
}

fn omission_marker(omitted: usize) -> String {
    let skill_word = if omitted == 1 { "skill" } else { "skills" };
    format!("- {omitted} additional {skill_word} omitted from this bounded skills list.")
}

pub(crate) fn truncate_catalog_skill_description(description: &str) -> Cow<'_, str> {
    if description
        .char_indices()
        .nth(MAX_CATALOG_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return Cow::Borrowed(description);
    }

    let prefix_chars = MAX_CATALOG_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let mut truncated = description[..prefix_end].to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    Cow::Owned(truncated)
}

pub(crate) fn truncate_main_prompt_contents(contents: &str) -> (String, bool) {
    truncate_utf8_to_bytes(contents, MAX_SKILL_PROMPT_BYTES)
}

pub(crate) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let truncated = take_bytes_at_char_boundary(contents, max_bytes);
    (truncated.to_string(), truncated.len() < contents.len())
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
