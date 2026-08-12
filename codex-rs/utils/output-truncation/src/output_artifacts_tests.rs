use super::*;
use crate::output_artifacts::MAX_GLOBAL_OUTPUT_ARTIFACT_BYTES;
use crate::output_artifacts::MAX_THREAD_OUTPUT_ARTIFACT_BYTES;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use std::time::SystemTime;
use tempfile::tempdir;

fn artifact_store(base: &std::path::Path) -> OutputArtifactStore {
    OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(base)
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    )
}

#[tokio::test]
async fn missing_reads_do_not_create_artifact_directories() {
    let temp = tempdir().expect("tempdir");
    let store = artifact_store(temp.path());
    let missing = OutputArtifactId::parse(&format!("out_{}", "0".repeat(64))).expect("id");

    assert_eq!(
        store
            .read_bytes(&missing, /*offset*/ 0, /*max_bytes*/ 16)
            .await
            .expect_err("missing")
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!temp.path().join("tool_outputs").exists());
}

#[tokio::test]
async fn stores_once_and_rejects_unmanaged_paths() {
    let temp = tempdir().expect("tempdir");
    let store = artifact_store(temp.path());
    let text = "head\nalpha middle omega\ntail\n";
    let first = store.store_text(text).await.expect("store");
    let second = store.store_text(text).await.expect("reuse");
    assert_eq!(first.id, second.id);
    assert_eq!((first.reused, second.reused), (false, true));
    assert_eq!(
        std::fs::read(first.diagnostic_path.as_path()).expect("read"),
        text.as_bytes()
    );
    #[cfg(unix)]
    for (path, mode) in [(&first.diagnostic_path, 0o600), (&store.root, 0o700)] {
        assert_eq!(
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
    assert_eq!(
        store
            .read_bytes(&first.id, /*offset*/ 6, /*max_bytes*/ 8)
            .await
            .expect("window"),
        ("lpha mid".into(), 6, 14, Some(14))
    );

    assert_eq!(
        OutputArtifactId::parse("../outside")
            .expect_err("invalid")
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let missing = OutputArtifactId::parse(&format!("out_{}", "0".repeat(64))).expect("id");
    assert_eq!(
        store
            .read_bytes(&missing, /*offset*/ 0, /*max_bytes*/ 16)
            .await
            .expect_err("missing")
            .kind(),
        io::ErrorKind::NotFound
    );
    store.remove_thread().await.expect("cleanup");
    assert!(!store.root.exists());

    #[cfg(unix)]
    {
        let linked = tempdir().expect("tempdir");
        let outside = tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), linked.path().join("tool_outputs"))
            .expect("symlink");
        assert_eq!(
            artifact_store(linked.path())
                .store_text("secret")
                .await
                .expect_err("reject")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!outside.path().join("secret").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_rejects_a_symlinked_managed_ancestor() {
    let temp = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside tempdir");
    let external_thread = outside.path().join("thread");
    std::fs::create_dir(&external_thread).expect("external thread");
    std::fs::write(external_thread.join("keep.txt"), "keep").expect("external file");
    std::os::unix::fs::symlink(outside.path(), temp.path().join("tool_outputs"))
        .expect("managed-root symlink");

    let error = artifact_store(temp.path())
        .remove_thread()
        .await
        .expect_err("reject ancestor symlink");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read_to_string(external_thread.join("keep.txt")).expect("external file remains"),
        "keep"
    );
}

#[tokio::test]
async fn rejects_artifacts_over_the_per_artifact_quota() {
    let temp = tempdir().expect("tempdir");
    let error = artifact_store(temp.path())
        .store_text(&"x".repeat(MAX_OUTPUT_ARTIFACT_BYTES + 1))
        .await
        .expect_err("quota must fail closed");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn enforces_thread_and_global_storage_quotas() {
    let thread_temp = tempdir().expect("tempdir");
    let thread_store = artifact_store(thread_temp.path());
    std::fs::create_dir_all(thread_store.root.as_path()).expect("create thread root");
    let thread_fill = std::fs::File::create(thread_store.root.join("quota-fill").as_path())
        .expect("create sparse thread fill");
    thread_fill
        .set_len(MAX_THREAD_OUTPUT_ARTIFACT_BYTES as u64)
        .expect("size sparse thread fill");
    assert_eq!(
        thread_store
            .store_text("over thread quota")
            .await
            .expect_err("thread quota")
            .kind(),
        io::ErrorKind::InvalidData
    );

    let global_temp = tempdir().expect("tempdir");
    let global_store = artifact_store(global_temp.path());
    let sibling = AbsolutePathBuf::from_absolute_path(global_temp.path())
        .expect("absolute tempdir")
        .join("tool_outputs/sibling");
    std::fs::create_dir_all(sibling.as_path()).expect("create sibling root");
    let global_fill = std::fs::File::create(sibling.join("quota-fill").as_path())
        .expect("create sparse global fill");
    global_fill
        .set_len(MAX_GLOBAL_OUTPUT_ARTIFACT_BYTES as u64)
        .expect("size sparse global fill");
    assert_eq!(
        global_store
            .store_text("over global quota")
            .await
            .expect_err("global quota")
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn fork_copy_gives_the_child_independent_artifact_ownership() {
    let temp = tempdir().expect("tempdir");
    let parent = artifact_store(temp.path());
    let child = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/child"),
    );
    let original = "parent output that the child must recover";
    let artifact = parent.store_text(original).await.expect("store parent");

    parent
        .copy_to(&child, std::slice::from_ref(&artifact.id))
        .await
        .expect("copy fork artifacts");
    parent
        .remove_thread()
        .await
        .expect("delete parent artifacts");

    assert_eq!(
        child
            .read_bytes(&artifact.id, /*offset*/ 0, /*max_bytes*/ 128)
            .await
            .expect("read child copy")
            .0,
        original
    );
}

#[tokio::test]
async fn fork_copy_fails_closed_for_missing_or_same_thread_artifacts() {
    let temp = tempdir().expect("tempdir");
    let parent = artifact_store(temp.path());
    let child = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/child"),
    );
    let missing =
        OutputArtifactId::parse(&format!("out_{}", "0".repeat(64))).expect("valid missing id");

    assert_eq!(
        parent
            .copy_to(&child, std::slice::from_ref(&missing))
            .await
            .expect_err("missing source artifact")
            .kind(),
        io::ErrorKind::NotFound
    );
    let artifact = parent.store_text("parent").await.expect("store parent");
    assert_eq!(
        parent
            .copy_to(&parent, std::slice::from_ref(&artifact.id))
            .await
            .expect_err("same-thread copy")
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn retention_sweep_reclaims_crash_orphans_and_preserves_recent_access() {
    let temp = tempdir().expect("tempdir");
    let managed_root = AbsolutePathBuf::from_absolute_path(temp.path())
        .expect("absolute tempdir")
        .join("tool_outputs");
    let stale = OutputArtifactStore::new(managed_root.join("stale-thread"));
    let recent = OutputArtifactStore::new(managed_root.join("recent-thread"));
    stale.store_text("stale output").await.expect("store stale");
    let recent_artifact = recent
        .store_text("recent output")
        .await
        .expect("store recent");
    let old = SystemTime::now() - Duration::from_secs(3_600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(stale.root.join(".last_access").as_path())
        .expect("open stale access marker")
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .expect("age stale marker");

    let report = OutputArtifactStore::sweep_expired(
        managed_root,
        SystemTime::now() - Duration::from_secs(60),
    )
    .await
    .expect("sweep expired artifacts");

    assert_eq!(report.removed_scopes, 1);
    assert!(!stale.root.exists());
    assert_eq!(
        recent
            .read_bytes(
                &recent_artifact.id,
                /*offset*/ 0,
                /*max_bytes*/ 64
            )
            .await
            .expect("recent artifact remains")
            .0,
        "recent output"
    );
}
