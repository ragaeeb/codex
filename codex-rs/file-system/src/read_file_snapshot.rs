use crate::ExecutorFileSystem;
use crate::FileMetadata;
use crate::FileSystemSandboxContext;
use crate::GetMetadataOptions;
use crate::read_file_window::ReadFileWindow;
use crate::read_file_window::ReadFileWindowBounds;
use codex_utils_path_uri::PathUri;
use std::io;

const FILE_CHANGED_DURING_READ: &str = "file changed during the read; retry read_file";

/// The canonical, metadata-consistent snapshot returned by a bounded streamed file read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFileSnapshot {
    pub canonical_path: PathUri,
    pub metadata: FileMetadata,
    pub window: ReadFileWindow,
}

/// Performs canonicalization, pre-stat, bounded streaming, post-stat, and re-canonicalization as
/// one filesystem-owned transaction. Callers must use this boundary rather than pairing metadata
/// captured by one request with bytes opened by another request.
pub async fn read_file_snapshot_transaction(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
    bounds: ReadFileWindowBounds,
) -> io::Result<ReadFileSnapshot> {
    let canonical_path = fs.canonicalize(path, sandbox).await?;
    let metadata = fs
        .get_metadata(path, GetMetadataOptions::default(), sandbox)
        .await?;
    if !metadata.is_file {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only regular files can be read",
        ));
    }
    if bounds.offset > metadata.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "read offset is beyond the end of the file",
        ));
    }
    let window = read_file_snapshot(fs, path, sandbox, bounds, &metadata, &canonical_path).await?;
    Ok(ReadFileSnapshot {
        canonical_path,
        metadata,
        window,
    })
}

async fn read_file_snapshot(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
    bounds: ReadFileWindowBounds,
    metadata: &FileMetadata,
    canonical_path: &PathUri,
) -> io::Result<ReadFileWindow> {
    if bounds.offset != 0 && !fs.supports_read_file_stream_from() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem does not support efficient offset reads",
        ));
    }
    let window = if bounds.offset == metadata.size {
        ReadFileWindow {
            text: String::new(),
            start_byte: bounds.offset,
            end_byte: bounds.offset,
            line_fragments: 0,
            line_continues: false,
            next_offset: None,
            eof: true,
        }
    } else {
        let stream = fs
            .read_file_stream_from(path, bounds.offset, sandbox)
            .await?;
        crate::read_file_window::read_file_window(stream, bounds, metadata.size).await?
    };

    let after = fs
        .get_metadata(path, GetMetadataOptions::default(), sandbox)
        .await?;
    if !after.is_file
        || after.size != metadata.size
        || after.created_at_ms != metadata.created_at_ms
        || after.modified_at_ms != metadata.modified_at_ms
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            FILE_CHANGED_DURING_READ,
        ));
    }
    let after_canonical = fs.canonicalize(path, sandbox).await?;
    if after_canonical != *canonical_path {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            FILE_CHANGED_DURING_READ,
        ));
    }

    // A second bounded read catches same-size/same-timestamp replacement on filesystems whose
    // metadata does not expose a stable inode/file-id through FileMetadata.
    if window.start_byte != window.end_byte {
        let verification_stream = fs
            .read_file_stream_from(path, bounds.offset, sandbox)
            .await?;
        let verification =
            crate::read_file_window::read_file_window(verification_stream, bounds, metadata.size)
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::Interrupted, FILE_CHANGED_DURING_READ)
                })?;
        if verification != window {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                FILE_CHANGED_DURING_READ,
            ));
        }
    }
    Ok(window)
}

#[cfg(test)]
#[path = "read_file_snapshot_tests.rs"]
mod tests;
