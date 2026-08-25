use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::OUTPUT_ARTIFACT_RETENTION;
use codex_utils_output_truncation::OutputArtifactStore;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::SystemTime;
use tracing::info;
use tracing::warn;

static SWEPT_ROOTS: LazyLock<Mutex<HashSet<AbsolutePathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub(crate) async fn sweep_expired_artifacts_once(codex_home: &AbsolutePathBuf) {
    let managed_root = codex_home.join("tool_outputs");
    let should_sweep = SWEPT_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(managed_root.clone());
    if !should_sweep {
        return;
    }
    let cutoff = SystemTime::now()
        .checked_sub(OUTPUT_ARTIFACT_RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    match OutputArtifactStore::sweep_expired(managed_root.clone(), cutoff).await {
        Ok(report) => info!(
            rule = "artifact_retention_v1",
            removed_scopes = report.removed_scopes,
            removed_bytes = report.removed_bytes,
            "completed output artifact retention sweep"
        ),
        Err(err) => {
            SWEPT_ROOTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&managed_root);
            warn!(
                rule = "artifact_retention_v1",
                error_kind = ?err.kind(),
                "output artifact retention sweep failed; a later session will retry"
            );
        }
    }
}
