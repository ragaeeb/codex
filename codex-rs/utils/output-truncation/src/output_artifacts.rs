use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_string::take_bytes_at_char_boundary;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::time::Duration;
use std::time::SystemTime;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;

use quota::directory_usage;
use quota::directory_usage_with_budget;
use quota::managed_usage;

mod quota;

const PREFIX: &str = "out_";
const BUFFER_BYTES: usize = 8 * 1024;
const PREVIEW_BYTES: usize = 384;
pub const MAX_ARTIFACT_READ_BYTES: usize = 128 * 1024;
pub const MAX_OUTPUT_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_THREAD_OUTPUT_ARTIFACT_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_GLOBAL_OUTPUT_ARTIFACT_BYTES: usize = 512 * 1024 * 1024;
const MAX_QUOTA_SCAN_ENTRIES: usize = 65_536;
const STORE_LOCK_FILE: &str = ".quota.lock";
const ACCESS_FILE: &str = ".last_access";
const ACCESS_FILE_RESERVED_BYTES: usize = 32;
pub const OUTPUT_ARTIFACT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

static ARTIFACT_STORE_LOCK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OutputArtifactId(String);

impl OutputArtifactId {
    pub fn parse(value: &str) -> io::Result<Self> {
        let digest = value.strip_prefix(PREFIX).ok_or_else(invalid_id)?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid_id());
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn for_text(text: &str) -> Self {
        Self::from_digest(Sha256::digest(text.as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn digest(&self) -> &str {
        &self.0[PREFIX.len()..]
    }

    fn from_digest(digest: impl std::fmt::LowerHex) -> Self {
        Self(format!("{PREFIX}{digest:x}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOutputArtifact {
    pub id: OutputArtifactId,
    pub diagnostic_path: AbsolutePathBuf,
    pub reused: bool,
    pub original_bytes: usize,
    pub original_lines: usize,
    pub preview_head: String,
    pub preview_tail: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OutputArtifactSweepReport {
    pub removed_scopes: usize,
    pub removed_bytes: usize,
}

impl StoredOutputArtifact {
    pub fn envelope(&self, content_type: &str, max_bytes: usize) -> String {
        let render = |head: &str, tail: &str| {
            json!({
            "type": "tool_output_artifact", "version": 1,
            "artifact_id": self.id.as_str(), "content_type": content_type,
            "original_bytes": self.original_bytes, "original_lines": self.original_lines,
            "approximate_tokens": crate::approx_tokens_from_byte_count(self.original_bytes),
            "digest": format!("sha256:{}", self.id.digest()),
            "preview": {"head": head, "tail": tail},
            "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue."
        }).to_string()
        };
        let empty = render("", "");
        if empty.len() >= max_bytes {
            let minimal = json!({
                "type": "tool_output_artifact",
                "artifact_id": self.id.as_str(),
                "retrieval": "Use read_tool_output with this artifact_id."
            })
            .to_string();
            return if minimal.len() <= max_bytes {
                minimal
            } else {
                crate::truncate_text(
                    &format!(
                        "Tool output saved as {}. Use read_tool_output to retrieve it.",
                        self.id.as_str()
                    ),
                    crate::TruncationPolicy::Bytes(max_bytes),
                )
            };
        }
        let preview_budget = max_bytes - empty.len();
        let mut per_side = preview_budget / 2;
        loop {
            let head = take_bytes_at_char_boundary(&self.preview_head, per_side);
            let tail = tail_bytes_at_char_boundary(&self.preview_tail, per_side);
            let rendered = render(head, tail);
            if rendered.len() <= max_bytes || per_side == 0 {
                return rendered;
            }
            per_side = per_side.saturating_sub((rendered.len() - max_bytes).div_ceil(2).max(1));
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputArtifactStore {
    pub(crate) root: AbsolutePathBuf,
    managed_root: AbsolutePathBuf,
}

impl OutputArtifactStore {
    pub fn new(root: AbsolutePathBuf) -> Self {
        let managed_root = root.parent().unwrap_or_else(|| root.clone());
        Self { root, managed_root }
    }

    pub async fn store_text(&self, text: &str) -> io::Result<StoredOutputArtifact> {
        let mut artifact = self.store_bytes(text.as_bytes()).await?;
        artifact.preview_head = take_bytes_at_char_boundary(text, PREVIEW_BYTES).to_string();
        artifact.preview_tail = tail_preview(text);
        Ok(artifact)
    }

    pub async fn store_bytes(&self, bytes: &[u8]) -> io::Result<StoredOutputArtifact> {
        if bytes.len() > MAX_OUTPUT_ARTIFACT_BYTES {
            return Err(quota("output exceeds the per-artifact quota"));
        }
        let _guard = lock_artifact_store().await?;
        let _file_guard = self.lock_store().await?;
        self.ensure_root().await?;
        let id = OutputArtifactId::from_digest(Sha256::digest(bytes));
        let path = self.path(&id);
        let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
        let original_lines =
            newlines + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
        if fs::try_exists(path.as_path()).await? {
            self.verify_existing(&id, bytes.len()).await?;
            if !fs::try_exists(self.root.join(ACCESS_FILE).as_path()).await? {
                self.ensure_capacity(ACCESS_FILE_RESERVED_BYTES).await?;
            }
            self.touch_access().await?;
            return Ok(StoredOutputArtifact {
                id,
                diagnostic_path: path,
                reused: true,
                original_bytes: bytes.len(),
                original_lines,
                preview_head: String::from_utf8_lossy(&bytes[..bytes.len().min(PREVIEW_BYTES)])
                    .into_owned(),
                preview_tail: String::from_utf8_lossy(
                    &bytes[bytes.len().saturating_sub(PREVIEW_BYTES)..],
                )
                .into_owned(),
            });
        }
        let marker_reservation = if fs::try_exists(self.root.join(ACCESS_FILE).as_path()).await? {
            0
        } else {
            ACCESS_FILE_RESERVED_BYTES
        };
        self.ensure_capacity(bytes.len().saturating_add(marker_reservation))
            .await?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path.as_path()).await?;
        if let Err(err) = file.write_all(bytes).await {
            drop(file);
            let _ = fs::remove_file(path.as_path()).await;
            return Err(err);
        }
        self.touch_access().await?;
        Ok(StoredOutputArtifact {
            id,
            diagnostic_path: path,
            reused: false,
            original_bytes: bytes.len(),
            original_lines,
            preview_head: String::from_utf8_lossy(&bytes[..bytes.len().min(PREVIEW_BYTES)])
                .into_owned(),
            preview_tail: String::from_utf8_lossy(
                &bytes[bytes.len().saturating_sub(PREVIEW_BYTES)..],
            )
            .into_owned(),
        })
    }

    pub async fn read_bytes(
        &self,
        id: &OutputArtifactId,
        offset: u64,
        max_bytes: usize,
    ) -> io::Result<(String, u64, u64, Option<u64>)> {
        let max_bytes = bounded(max_bytes, MAX_ARTIFACT_READ_BYTES, "max_bytes")?.max(4);
        let _guard = lock_artifact_store().await?;
        if !existing_dir(&self.managed_root).await? || !existing_dir(&self.root).await? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "output artifact is unavailable",
            ));
        }
        let _file_guard = self.lock_existing_store().await?;
        let (mut file, size) = self.open(id).await?;
        self.touch_access().await?;
        if offset >= size {
            return Ok((String::new(), size, size, None));
        }
        file.seek(io::SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0; max_bytes + 3];
        let read = file.read(&mut bytes).await?;
        bytes.truncate(read);
        let leading = bytes
            .iter()
            .take(3)
            .take_while(|byte| (**byte & 0xc0) == 0x80)
            .count();
        let start = offset + leading as u64;
        let bytes = &bytes[leading..bytes.len().min(leading + max_bytes)];
        let consumed = match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => bytes.len(),
        };
        let text = String::from_utf8_lossy(&bytes[..consumed]).into_owned();
        let end = start + consumed as u64;
        Ok((text, start, end, (end < size).then_some(end)))
    }

    /// Returns the byte length of a safely opened managed artifact.
    pub async fn artifact_size(&self, id: &OutputArtifactId) -> io::Result<u64> {
        self.open(id).await.map(|(_, size)| size)
    }

    pub async fn remove_thread(&self) -> io::Result<bool> {
        let _guard = lock_artifact_store().await?;
        match existing_dir(&self.managed_root).await? {
            true => {}
            false => return Ok(false),
        }
        let _file_guard = self.lock_existing_store().await?;
        let metadata = match fs::symlink_metadata(self.root.as_path()).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(denied("refusing non-directory artifact root"));
        }
        fs::remove_dir_all(self.root.as_path()).await?;
        Ok(true)
    }

    pub async fn copy_to(&self, destination: &Self, ids: &[OutputArtifactId]) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let _guard = lock_artifact_store().await?;
        if self.root == destination.root {
            return Err(invalid("artifact copy source and destination must differ"));
        }
        if self.managed_root != destination.managed_root {
            return Err(invalid("artifact copies must remain in one managed store"));
        }
        if !existing_dir(&self.managed_root).await? || !existing_dir(&self.root).await? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "source output artifacts are unavailable",
            ));
        }
        let _file_guard = self.lock_existing_store().await?;
        destination.ensure_root().await?;
        let mut pending = Vec::new();
        let mut additional_bytes = 0usize;
        for id in ids {
            let source = self.path(id);
            let source_size = self.artifact_size(id).await?;
            let size = usize::try_from(source_size).map_err(|_| quota("artifact is too large"))?;
            self.verify_existing(id, size).await?;
            let target = destination.path(id);
            if fs::try_exists(target.as_path()).await? {
                destination.verify_existing(id, size).await?;
            } else {
                additional_bytes = additional_bytes
                    .checked_add(size)
                    .ok_or_else(|| quota("artifact quota overflow"))?;
                pending.push((source, target));
            }
        }
        if !fs::try_exists(destination.root.join(ACCESS_FILE).as_path()).await? {
            additional_bytes = additional_bytes.saturating_add(ACCESS_FILE_RESERVED_BYTES);
        }
        destination.ensure_capacity(additional_bytes).await?;
        for (source, target) in pending {
            if fs::hard_link(source.as_path(), target.as_path())
                .await
                .is_err()
            {
                let mut source_file = fs::File::open(source.as_path()).await?;
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                let mut target_file = options.open(target.as_path()).await?;
                if let Err(err) = tokio::io::copy(&mut source_file, &mut target_file).await {
                    drop(target_file);
                    let _ = fs::remove_file(target.as_path()).await;
                    return Err(err);
                }
            }
        }
        self.touch_access().await?;
        destination.touch_access().await?;
        Ok(())
    }

    pub async fn sweep_expired(
        managed_root: AbsolutePathBuf,
        cutoff: SystemTime,
    ) -> io::Result<OutputArtifactSweepReport> {
        let _guard = lock_artifact_store().await?;
        if !existing_dir(&managed_root).await? {
            return Ok(OutputArtifactSweepReport::default());
        }
        let lock_owner = Self {
            root: managed_root.join("_sweep"),
            managed_root: managed_root.clone(),
        };
        let _file_guard = lock_owner.lock_existing_store().await?;
        existing_dir(&managed_root).await?;

        let mut report = OutputArtifactSweepReport::default();
        let mut scanned = 0usize;
        let mut entries = fs::read_dir(managed_root.as_path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_QUOTA_SCAN_ENTRIES {
                return Err(quota("artifact retention scan entry limit exceeded"));
            }
            let path = AbsolutePathBuf::from_absolute_path(entry.path())
                .map_err(|err| invalid(&err.to_string()))?;
            if path.file_name().is_some_and(|name| name == STORE_LOCK_FILE) {
                continue;
            }
            let metadata = fs::symlink_metadata(path.as_path()).await?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(denied("refusing non-directory artifact scope"));
            }
            let access_path = path.join(ACCESS_FILE);
            let access_metadata = match fs::symlink_metadata(access_path.as_path()).await {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    metadata
                }
                Ok(_) => return Err(denied("refusing non-regular artifact access marker")),
                Err(err) if err.kind() == io::ErrorKind::NotFound => metadata,
                Err(err) => return Err(err),
            };
            if access_metadata.modified()? > cutoff {
                continue;
            }
            let bytes = directory_usage_with_budget(&path, &mut scanned).await?;
            existing_dir(&managed_root).await?;
            existing_dir(&path).await?;
            fs::remove_dir_all(path.as_path()).await?;
            report.removed_scopes = report.removed_scopes.saturating_add(1);
            report.removed_bytes = report.removed_bytes.saturating_add(bytes);
        }
        Ok(report)
    }

    fn path(&self, id: &OutputArtifactId) -> AbsolutePathBuf {
        self.root.join(format!("{}.txt", id.as_str()))
    }

    async fn ensure_root(&self) -> io::Result<()> {
        self.ensure_managed_root().await?;
        ensure_dir(&self.root).await
    }

    async fn ensure_managed_root(&self) -> io::Result<()> {
        let parent = self
            .managed_root
            .parent()
            .ok_or_else(|| invalid("managed artifact root has no parent"))?;
        fs::create_dir_all(parent.as_path()).await?;
        ensure_dir(&self.managed_root).await
    }

    async fn lock_store(&self) -> io::Result<fslock::LockFile> {
        self.ensure_managed_root().await?;
        self.lock_existing_store().await
    }

    async fn lock_existing_store(&self) -> io::Result<fslock::LockFile> {
        let lock_path = self.managed_root.join(STORE_LOCK_FILE).to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut lock = fslock::LockFile::open(&lock_path)?;
            lock.lock()?;
            Ok(lock)
        })
        .await
        .map_err(|err| io::Error::other(format!("artifact lock task failed: {err}")))?
    }

    async fn ensure_capacity(&self, additional_bytes: usize) -> io::Result<()> {
        let thread_bytes = directory_usage(&self.root).await?;
        if thread_bytes.saturating_add(additional_bytes) > MAX_THREAD_OUTPUT_ARTIFACT_BYTES {
            return Err(quota("thread output artifact quota exceeded"));
        }
        let global_bytes = managed_usage(&self.managed_root).await?;
        if global_bytes.saturating_add(additional_bytes) > MAX_GLOBAL_OUTPUT_ARTIFACT_BYTES {
            return Err(quota("global output artifact quota exceeded"));
        }
        Ok(())
    }

    async fn touch_access(&self) -> io::Result<()> {
        let path = self.root.join(ACCESS_FILE);
        match fs::symlink_metadata(path.as_path()).await {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                fs::remove_file(path.as_path()).await?;
            }
            Ok(_) => return Err(denied("refusing non-regular artifact access marker")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        options
            .open(path.as_path())
            .await?
            .write_all(timestamp.as_bytes())
            .await
    }

    async fn open(&self, id: &OutputArtifactId) -> io::Result<(fs::File, u64)> {
        if !existing_dir(&self.managed_root).await? || !existing_dir(&self.root).await? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "output artifact is unavailable",
            ));
        }
        let path = self.path(id);
        let metadata = fs::symlink_metadata(path.as_path()).await?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(denied("refusing non-regular artifact"));
        }
        Ok((fs::File::open(path.as_path()).await?, metadata.len()))
    }

    async fn verify_existing(&self, id: &OutputArtifactId, length: usize) -> io::Result<()> {
        let (mut file, size) = self.open(id).await?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; BUFFER_BYTES];
        while let read @ 1.. = file.read(&mut buffer).await? {
            hasher.update(&buffer[..read]);
        }
        if size != length as u64 || format!("{PREFIX}{:x}", hasher.finalize()) != id.as_str() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact digest mismatch",
            ));
        }
        Ok(())
    }
}

pub fn content_type(text: &str) -> &'static str {
    if serde_json::from_str::<Value>(text).is_ok() {
        "application/json"
    } else if text.trim_start().starts_with('<') && text.trim_end().ends_with('>') {
        "application/xml"
    } else {
        "text/plain"
    }
}

fn tail_preview(text: &str) -> String {
    let start = (text.len().saturating_sub(PREVIEW_BYTES)..=text.len())
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(text.len());
    text[start..].to_string()
}

fn tail_bytes_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    let start = (text.len().saturating_sub(max_bytes)..=text.len())
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(text.len());
    &text[start..]
}

fn invalid_id() -> io::Error {
    invalid("invalid output artifact identifier")
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn quota(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

async fn lock_artifact_store() -> io::Result<tokio::sync::SemaphorePermit<'static>> {
    ARTIFACT_STORE_LOCK
        .acquire()
        .await
        .map_err(|_| io::Error::other("artifact store lock closed"))
}

fn bounded(value: usize, ceiling: usize, name: &str) -> io::Result<usize> {
    (value != 0)
        .then(|| value.min(ceiling))
        .ok_or_else(|| invalid(&format!("{name} must be greater than zero")))
}

async fn ensure_dir(path: &AbsolutePathBuf) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path.as_path()).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(path.as_path()).await {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
            #[cfg(unix)]
            fs::set_permissions(
                path.as_path(),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .await?;
            fs::symlink_metadata(path.as_path()).await?
        }
        Err(err) => return Err(err),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(denied("refusing symlinked artifact directory"))
    }
}

async fn existing_dir(path: &AbsolutePathBuf) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path.as_path()).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        Err(denied("refusing symlinked artifact directory"))
    } else {
        Ok(true)
    }
}
