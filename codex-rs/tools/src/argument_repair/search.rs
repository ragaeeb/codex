use super::ArgumentRepairLimit;
use super::ArgumentRepairPolicy;
use super::ArgumentRepairRule;
use super::EngineError;
use super::candidate::CandidateBudget;
use super::candidate::RepairStep;
use super::candidate::generate_candidates;
use super::canonical::value_key;
use super::schema::SchemaGraph;
use super::validation::Validator;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::collections::VecDeque;

pub(crate) enum RepairSearchResult {
    Repaired {
        value: JsonValue,
        rules: Vec<ArgumentRepairRule>,
        candidate_work: usize,
    },
    OriginalFailure {
        candidate_work: usize,
    },
}

pub(crate) fn search_repairs<'graph, 'schema>(
    graph: &'graph SchemaGraph<'schema>,
    validator: &Validator<'graph, 'schema>,
    original: &JsonValue,
    policy: &ArgumentRepairPolicy,
) -> Result<RepairSearchResult, (EngineError, usize)> {
    let limits = policy.limits();
    let mut queue = VecDeque::new();
    queue.push_back(SearchState {
        value: original.clone(),
        steps: Vec::new(),
    });
    let mut seen = BTreeMap::<String, Vec<RepairStep>>::new();
    seen.insert(value_key(original), Vec::new());
    let mut valid = BTreeMap::<String, SearchState>::new();
    let mut candidate_budget = CandidateBudget::new(limits.max_repair_candidates);
    let mut repair_limit_hit = false;

    while let Some(state) = queue.pop_front() {
        let candidates = generate_candidates(
            graph,
            validator,
            &state.value,
            policy,
            &mut candidate_budget,
        )
        .map_err(|error| (error, candidate_budget.attempted()))?;
        if state.steps.len() >= limits.max_repairs_per_call {
            repair_limit_hit |= !candidates.is_empty();
            continue;
        }

        for candidate in candidates {
            let mut steps = state.steps.clone();
            steps.push(candidate.step);
            steps.sort();

            if serialized_json_bytes(&candidate.value) > limits.max_repaired_output_bytes {
                return Err((
                    EngineError::Limit(ArgumentRepairLimit::RepairedOutputBytes),
                    candidate_budget.attempted(),
                ));
            }
            let value_key = value_key(&candidate.value);
            if seen
                .get(&value_key)
                .is_some_and(|existing_steps| existing_steps <= &steps)
            {
                continue;
            }
            seen.insert(value_key.clone(), steps.clone());

            let validation = validator
                .validate(&candidate.value)
                .map_err(|error| (error, candidate_budget.attempted()))?;
            let next_state = SearchState {
                value: candidate.value,
                steps,
            };
            if validation.is_valid() {
                match valid.get(&value_key) {
                    Some(existing) if existing.steps.as_slice() <= next_state.steps.as_slice() => {}
                    Some(_) | None => {
                        valid.insert(value_key, next_state);
                    }
                }
                if valid.len() > 1 {
                    return Ok(RepairSearchResult::OriginalFailure {
                        candidate_work: candidate_budget.attempted(),
                    });
                }
            } else {
                queue.push_back(next_state);
            }
        }
    }

    if repair_limit_hit {
        return Err((
            EngineError::Limit(ArgumentRepairLimit::RepairsPerCall),
            candidate_budget.attempted(),
        ));
    }

    let Some((_, repaired)) = valid.into_iter().next() else {
        return Ok(RepairSearchResult::OriginalFailure {
            candidate_work: candidate_budget.attempted(),
        });
    };
    Ok(RepairSearchResult::Repaired {
        value: repaired.value,
        rules: repaired.steps.into_iter().map(|step| step.rule).collect(),
        candidate_work: candidate_budget.attempted(),
    })
}

struct SearchState {
    value: JsonValue,
    steps: Vec<RepairStep>,
}

struct JsonByteCounter(usize);

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_json_bytes(value: &JsonValue) -> usize {
    let mut counter = JsonByteCounter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        return usize::MAX;
    }
    counter.0
}
