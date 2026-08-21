use super::*;
use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecutorFileSystem;
use crate::ExecutorFileSystemFuture;
use crate::FileMetadata;
use crate::FileSystemReadStream;
use crate::FileSystemSandboxContext;
use crate::GetMetadataOptions;
use crate::ReadDirectoryEntry;
use crate::ReadFileOptions;
use crate::RemoveOptions;
use crate::WalkOptions;
use crate::WalkOutcome;
use crate::WriteFileOptions;
use bytes::Bytes;
use codex_utils_path_uri::PathUri;
use futures::stream;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

struct SameMetadataReplacementFileSystem {
    streams: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl SameMetadataReplacementFileSystem {
    fn unused<T>() -> ExecutorFileSystemFuture<'static, T> {
        Box::pin(async { Err(io::Error::other("unused fake filesystem operation")) })
    }
}

impl ExecutorFileSystem for SameMetadataReplacementFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move { Ok(path.clone()) })
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: ReadFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Self::unused()
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Self::unused()
    }

    fn read_file_stream_from<'a>(
        &'a self,
        _path: &'a PathUri,
        _offset: u64,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        let streams = Arc::clone(&self.streams);
        Box::pin(async move {
            let bytes = streams
                .lock()
                .expect("stream queue lock")
                .pop_front()
                .ok_or_else(|| io::Error::other("replacement stream was not requested"))?;
            Ok(FileSystemReadStream::new(stream::once(async move {
                Ok(Bytes::from(bytes))
            })))
        })
    }

    fn supports_read_file_stream_from(&self) -> bool {
        true
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _options: WriteFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: GetMetadataOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async {
            Ok(FileMetadata {
                is_directory: false,
                is_file: true,
                is_symlink: false,
                size: 4,
                created_at_ms: 1,
                modified_at_ms: 1,
            })
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Self::unused()
    }

    fn walk<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: WalkOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Self::unused()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }
}

#[test]
fn snapshot_rejects_same_size_same_timestamp_replacement() {
    let path = PathUri::from_host_native_path(std::env::temp_dir().join("snapshot-race.txt"))
        .expect("path URI");
    let filesystem = SameMetadataReplacementFileSystem {
        streams: Arc::new(Mutex::new(VecDeque::from([
            b"old!".to_vec(),
            b"new!".to_vec(),
        ]))),
    };
    let error = futures::executor::block_on(read_file_snapshot_transaction(
        &filesystem,
        &path,
        /*sandbox*/ None,
        ReadFileWindowBounds {
            offset: 0,
            max_bytes: 4,
            max_line_fragments: 1,
            max_line_fragment_chars: 2_000,
        },
    ))
    .expect_err("same-metadata replacement must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(
        error.to_string(),
        "file changed during the read; retry read_file"
    );
}
