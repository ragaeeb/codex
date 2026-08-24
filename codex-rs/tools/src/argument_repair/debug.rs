use std::fmt;

use super::ArgumentRepairOutcome;
use super::ArgumentRepairPolicy;

impl fmt::Debug for ArgumentRepairPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArgumentRepairPolicy")
            .field("alias_count", &self.aliases.len())
            .field("alias_bytes", &self.alias_bytes)
            .field("markdown_path_count", &self.markdown_paths.len())
            .field("markdown_path_bytes", &self.markdown_path_bytes)
            .field("limits", &self.limits)
            .finish()
    }
}

impl fmt::Debug for ArgumentRepairOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValidUnchanged { raw_arguments } => formatter
                .debug_struct("ValidUnchanged")
                .field("raw_argument_bytes", &raw_arguments.len())
                .finish(),
            Self::Repaired { arguments, rules } => formatter
                .debug_struct("Repaired")
                .field("argument_bytes", &arguments.len())
                .field("rules", rules)
                .finish(),
            Self::NotRepairable { validation } => formatter
                .debug_struct("NotRepairable")
                .field("validation", validation)
                .finish(),
            Self::LimitExceeded { limit } => formatter
                .debug_struct("LimitExceeded")
                .field("limit", limit)
                .finish(),
            Self::UnsupportedSchema { reason } => formatter
                .debug_struct("UnsupportedSchema")
                .field("reason", reason)
                .finish(),
        }
    }
}
