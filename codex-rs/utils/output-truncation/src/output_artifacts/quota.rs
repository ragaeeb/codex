use super::MAX_QUOTA_SCAN_ENTRIES;
use super::STORE_LOCK_FILE;
use super::existing_dir;
use super::invalid;
use super::quota;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::io;
use tokio::fs;

pub(super) async fn directory_usage_with_budget(
    root: &AbsolutePathBuf,
    scanned: &mut usize,
) -> io::Result<usize> {
    let mut entries = fs::read_dir(root.as_path()).await?;
    let mut total = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        *scanned = scanned.saturating_add(1);
        if *scanned > MAX_QUOTA_SCAN_ENTRIES {
            return Err(quota("artifact quota scan entry limit exceeded"));
        }
        let metadata = fs::symlink_metadata(entry.path()).await?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(super::denied("refusing non-regular artifact entry"));
        }
        total = total
            .checked_add(
                usize::try_from(metadata.len()).map_err(|_| quota("artifact is too large"))?,
            )
            .ok_or_else(|| quota("artifact quota overflow"))?;
    }
    Ok(total)
}

pub(super) async fn directory_usage(root: &AbsolutePathBuf) -> io::Result<usize> {
    if !existing_dir(root).await? {
        return Ok(0);
    }
    let mut scanned = 0;
    directory_usage_with_budget(root, &mut scanned).await
}

pub(super) async fn managed_usage(root: &AbsolutePathBuf) -> io::Result<usize> {
    if !existing_dir(root).await? {
        return Ok(0);
    }
    let mut entries = fs::read_dir(root.as_path()).await?;
    let mut total = 0usize;
    let mut scanned = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_QUOTA_SCAN_ENTRIES {
            return Err(quota("artifact quota scan entry limit exceeded"));
        }
        let path = AbsolutePathBuf::from_absolute_path(entry.path())
            .map_err(|err| invalid(&err.to_string()))?;
        if path.file_name().is_some_and(|name| name == STORE_LOCK_FILE) {
            continue;
        }
        let metadata = fs::symlink_metadata(path.as_path()).await?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(super::denied("refusing non-directory artifact scope"));
        }
        total = total
            .checked_add(directory_usage_with_budget(&path, &mut scanned).await?)
            .ok_or_else(|| quota("artifact quota overflow"))?;
    }
    Ok(total)
}
