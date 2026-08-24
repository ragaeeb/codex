use crate::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::fmt;

mod candidate;
mod canonical;
mod debug;
mod pointer;
mod schema;
mod search;
mod validation;

#[cfg(test)]
#[path = "argument_repair_tests.rs"]
mod tests;

use pointer::JsonPointer;
use schema::SchemaGraph;
use search::RepairSearchResult;
use search::search_repairs;
use validation::Validator;

const MAX_ARGUMENT_REPAIR_POLICY_ALIASES: usize = 128;
const MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATHS: usize = 128;
const MAX_ARGUMENT_REPAIR_POLICY_ALIAS_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATH_BYTES: usize = 64 * 1024;

/// Conservative resource bounds for one argument validation and repair call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentRepairLimits {
    pub max_input_bytes: usize,
    pub max_schema_depth: usize,
    pub max_schema_nodes: usize,
    pub max_schema_bytes: usize,
    pub max_schema_metadata_bytes: usize,
    pub max_schema_name_bytes: usize,
    pub max_schema_enum_values: usize,
    pub max_schema_enum_bytes: usize,
    pub max_value_depth: usize,
    pub max_references: usize,
    pub max_validation_errors: usize,
    pub max_validation_diagnostic_bytes: usize,
    pub max_repair_candidates: usize,
    pub max_repairs_per_call: usize,
    pub max_repaired_output_bytes: usize,
}

/// Bounded work measurements from one validation/repair attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArgumentRepairMetrics {
    /// Number of atomic candidate-generation attempts made before deduplication.
    pub candidate_work: usize,
}

impl Default for ArgumentRepairLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 64 * 1024,
            max_schema_depth: 64,
            max_schema_nodes: 4_096,
            max_schema_bytes: 256 * 1024,
            max_schema_metadata_bytes: 64 * 1024,
            max_schema_name_bytes: 1024,
            max_schema_enum_values: 256,
            max_schema_enum_bytes: 64 * 1024,
            max_value_depth: 64,
            max_references: 128,
            max_validation_errors: 32,
            max_validation_diagnostic_bytes: 4 * 1024,
            max_repair_candidates: 128,
            max_repairs_per_call: 8,
            max_repaired_output_bytes: 64 * 1024,
        }
    }
}

/// Explicit policy gates for transformations that cannot be inferred safely.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ArgumentRepairPolicy {
    aliases: BTreeSet<KnownFieldAlias>,
    alias_bytes: usize,
    markdown_paths: BTreeSet<JsonPointer>,
    markdown_path_bytes: usize,
    limits: ArgumentRepairLimits,
}

impl ArgumentRepairPolicy {
    pub fn limits(&self) -> &ArgumentRepairLimits {
        &self.limits
    }

    pub fn set_limits(&mut self, limits: ArgumentRepairLimits) {
        self.limits = limits.clamp_to_hard_caps();
    }

    pub(crate) fn has_markdown_paths(&self) -> bool {
        !self.markdown_paths.is_empty()
    }

    /// Allow one exact field rename within the object at `object_path`.
    pub fn insert_known_field_alias(
        &mut self,
        object_path: &str,
        source_field: &str,
        destination_field: &str,
    ) -> Result<(), ArgumentRepairPolicyError> {
        if source_field == destination_field {
            return Err(ArgumentRepairPolicyError::AliasSourceEqualsDestination);
        }
        if source_field.len() > self.limits.max_schema_name_bytes
            || destination_field.len() > self.limits.max_schema_name_bytes
        {
            return Err(ArgumentRepairPolicyError::AliasFieldTooLong);
        }
        if object_path.len() > self.limits.max_schema_metadata_bytes {
            return Err(ArgumentRepairPolicyError::PolicyPathTooLong);
        }
        let object_path =
            JsonPointer::parse(object_path).ok_or(ArgumentRepairPolicyError::InvalidJsonPointer)?;
        let alias = KnownFieldAlias {
            object_path,
            source_field: source_field.to_string(),
            destination_field: destination_field.to_string(),
        };
        if self.aliases.contains(&alias) {
            return Ok(());
        }
        if self.aliases.len() >= MAX_ARGUMENT_REPAIR_POLICY_ALIASES {
            return Err(ArgumentRepairPolicyError::PolicyEntryLimitExceeded);
        }
        let entry_bytes = alias
            .object_path
            .as_string()
            .len()
            .checked_add(alias.source_field.len())
            .and_then(|bytes| bytes.checked_add(alias.destination_field.len()))
            .ok_or(ArgumentRepairPolicyError::PolicyBytesLimitExceeded)?;
        let alias_bytes = self
            .alias_bytes
            .checked_add(entry_bytes)
            .filter(|bytes| *bytes <= MAX_ARGUMENT_REPAIR_POLICY_ALIAS_BYTES)
            .ok_or(ArgumentRepairPolicyError::PolicyBytesLimitExceeded)?;
        self.aliases.insert(alias);
        self.alias_bytes = alias_bytes;
        Ok(())
    }

    /// Allow markdown path-wrapper removal at one exact JSON Pointer path.
    pub fn allow_markdown_path(&mut self, path: &str) -> Result<(), ArgumentRepairPolicyError> {
        if path.len() > self.limits.max_schema_metadata_bytes {
            return Err(ArgumentRepairPolicyError::PolicyPathTooLong);
        }
        let path = JsonPointer::parse(path).ok_or(ArgumentRepairPolicyError::InvalidJsonPointer)?;
        if self.markdown_paths.contains(&path) {
            return Ok(());
        }
        if self.markdown_paths.len() >= MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATHS {
            return Err(ArgumentRepairPolicyError::PolicyEntryLimitExceeded);
        }
        let path_bytes = self
            .markdown_path_bytes
            .checked_add(path.as_string().len())
            .filter(|bytes| *bytes <= MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATH_BYTES)
            .ok_or(ArgumentRepairPolicyError::PolicyBytesLimitExceeded)?;
        self.markdown_paths.insert(path);
        self.markdown_path_bytes = path_bytes;
        Ok(())
    }
}

impl ArgumentRepairLimits {
    fn clamp_to_hard_caps(mut self) -> Self {
        let caps = Self::default();
        self.max_input_bytes = self.max_input_bytes.min(caps.max_input_bytes);
        self.max_schema_depth = self.max_schema_depth.min(caps.max_schema_depth);
        self.max_schema_nodes = self.max_schema_nodes.min(caps.max_schema_nodes);
        self.max_schema_bytes = self.max_schema_bytes.min(caps.max_schema_bytes);
        self.max_schema_metadata_bytes = self
            .max_schema_metadata_bytes
            .min(caps.max_schema_metadata_bytes);
        self.max_schema_name_bytes = self.max_schema_name_bytes.min(caps.max_schema_name_bytes);
        self.max_schema_enum_values = self.max_schema_enum_values.min(caps.max_schema_enum_values);
        self.max_schema_enum_bytes = self.max_schema_enum_bytes.min(caps.max_schema_enum_bytes);
        self.max_value_depth = self.max_value_depth.min(caps.max_value_depth);
        self.max_references = self.max_references.min(caps.max_references);
        self.max_validation_errors = self.max_validation_errors.min(caps.max_validation_errors);
        self.max_validation_diagnostic_bytes = self
            .max_validation_diagnostic_bytes
            .min(caps.max_validation_diagnostic_bytes);
        self.max_repair_candidates = self.max_repair_candidates.min(caps.max_repair_candidates);
        self.max_repairs_per_call = self.max_repairs_per_call.min(caps.max_repairs_per_call);
        self.max_repaired_output_bytes = self
            .max_repaired_output_bytes
            .min(caps.max_repaired_output_bytes);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KnownFieldAlias {
    object_path: JsonPointer,
    source_field: String,
    destination_field: String,
}

/// Invalid explicit repair policy metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentRepairPolicyError {
    InvalidJsonPointer,
    AliasSourceEqualsDestination,
    AliasFieldTooLong,
    PolicyPathTooLong,
    PolicyEntryLimitExceeded,
    PolicyBytesLimitExceeded,
}

/// Stable identifiers for the only transformations the engine may apply.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentRepairRule {
    OptionalNullRemoved,
    StringifiedArrayDecoded,
    StringifiedObjectDecoded,
    ScalarWrappedInArray,
    NumericStringTyped,
    BooleanStringTyped,
    KnownFieldAlias,
    MarkdownPathUnwrapped,
}

impl ArgumentRepairRule {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OptionalNullRemoved => "optional_null_removed",
            Self::StringifiedArrayDecoded => "stringified_array_decoded",
            Self::StringifiedObjectDecoded => "stringified_object_decoded",
            Self::ScalarWrappedInArray => "scalar_wrapped_in_array",
            Self::NumericStringTyped => "numeric_string_typed",
            Self::BooleanStringTyped => "boolean_string_typed",
            Self::KnownFieldAlias => "known_field_alias",
            Self::MarkdownPathUnwrapped => "markdown_path_unwrapped",
        }
    }
}

impl fmt::Display for ArgumentRepairRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// JSON value types used in structured validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArgumentValidationType {
    String,
    Number,
    Boolean,
    Integer,
    Object,
    Array,
    Null,
}

/// Schema keyword or required type that failed at one local argument path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArgumentValidationKeyword {
    MalformedJson,
    Type {
        expected: Vec<ArgumentValidationType>,
    },
    Enum,
    Required,
    AdditionalProperties,
    AnyOf,
    OneOf,
    AllOf,
}

/// One content-free validation failure at a redacted JSON Pointer-style local location.
///
/// The repair engine retains exact pointers internally for candidate generation. Public
/// outcomes replace every path token with a structural placeholder so dynamic object keys,
/// filesystem names, and other argument-derived data cannot escape through diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArgumentValidationError {
    pub path: String,
    pub keyword: ArgumentValidationKeyword,
}

/// Deterministically ordered content-free validation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentValidationResult {
    pub errors: Vec<ArgumentValidationError>,
}

impl ArgumentValidationResult {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    fn malformed_json() -> Self {
        Self {
            errors: vec![ArgumentValidationError {
                path: String::new(),
                keyword: ArgumentValidationKeyword::MalformedJson,
            }],
        }
    }
}

/// The resource bound that stopped validation or repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentRepairLimit {
    InputBytes,
    SchemaDepth,
    SchemaNodes,
    SchemaBytes,
    SchemaMetadataBytes,
    SchemaNameBytes,
    SchemaEnumValues,
    SchemaEnumBytes,
    ValueDepth,
    References,
    ValidationErrors,
    ValidationDiagnosticBytes,
    RepairCandidates,
    RepairsPerCall,
    RepairedOutputBytes,
}

/// Unsupported schema behavior that is rejected rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedSchemaReason {
    ExternalReference,
    InvalidReference,
    MissingReference,
    ReferenceCycle,
}

/// Result of validating untouched arguments and conservatively attempting repair.
#[derive(Clone, PartialEq, Eq)]
pub enum ArgumentRepairOutcome {
    ValidUnchanged {
        raw_arguments: String,
    },
    Repaired {
        arguments: String,
        rules: Vec<ArgumentRepairRule>,
    },
    NotRepairable {
        validation: ArgumentValidationResult,
    },
    LimitExceeded {
        limit: ArgumentRepairLimit,
    },
    UnsupportedSchema {
        reason: UnsupportedSchemaReason,
    },
}

pub(crate) enum EngineError {
    Limit(ArgumentRepairLimit),
    Unsupported(UnsupportedSchemaReason),
}

impl From<EngineError> for ArgumentRepairOutcome {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Limit(limit) => Self::LimitExceeded { limit },
            EngineError::Unsupported(reason) => Self::UnsupportedSchema { reason },
        }
    }
}

/// Validate raw tool arguments and return them unchanged when already valid.
pub fn validate_and_repair(
    schema: &JsonSchema,
    raw_arguments: &str,
    policy: &ArgumentRepairPolicy,
) -> ArgumentRepairOutcome {
    validate_and_repair_with_metrics(schema, raw_arguments, policy).0
}

/// Validate and repair arguments while returning bounded work measurements for telemetry.
pub fn validate_and_repair_with_metrics(
    schema: &JsonSchema,
    raw_arguments: &str,
    policy: &ArgumentRepairPolicy,
) -> (ArgumentRepairOutcome, ArgumentRepairMetrics) {
    let metrics = ArgumentRepairMetrics::default();
    if raw_arguments.len() > policy.limits.max_input_bytes {
        return (
            ArgumentRepairOutcome::LimitExceeded {
                limit: ArgumentRepairLimit::InputBytes,
            },
            metrics,
        );
    }

    let value: JsonValue = match serde_json::from_str(raw_arguments) {
        Ok(value) => value,
        Err(_) => {
            return (
                ArgumentRepairOutcome::NotRepairable {
                    validation: ArgumentValidationResult::malformed_json(),
                },
                metrics,
            );
        }
    };

    let graph = SchemaGraph::new(schema, &policy.limits);
    if let Err(error) = graph.preflight() {
        return (error.into(), metrics);
    }

    let validator = Validator::new(&graph);
    let validation = match validator.validate(&value) {
        Ok(validation) => validation,
        Err(error) => return (error.into(), metrics),
    };

    if validation.is_valid() && !policy.has_markdown_paths() {
        return (
            ArgumentRepairOutcome::ValidUnchanged {
                raw_arguments: raw_arguments.to_string(),
            },
            metrics,
        );
    }

    match search_repairs(&graph, &validator, &value, policy) {
        Ok(RepairSearchResult::Repaired {
            value,
            rules,
            candidate_work,
        }) => (
            ArgumentRepairOutcome::Repaired {
                arguments: value.to_string(),
                rules,
            },
            ArgumentRepairMetrics { candidate_work },
        ),
        Ok(RepairSearchResult::OriginalFailure { candidate_work }) => {
            if validation.is_valid() {
                (
                    ArgumentRepairOutcome::ValidUnchanged {
                        raw_arguments: raw_arguments.to_string(),
                    },
                    ArgumentRepairMetrics { candidate_work },
                )
            } else {
                (
                    ArgumentRepairOutcome::NotRepairable { validation },
                    ArgumentRepairMetrics { candidate_work },
                )
            }
        }
        Err((error, candidate_work)) => (error.into(), ArgumentRepairMetrics { candidate_work }),
    }
}
