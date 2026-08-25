use anyhow::Result;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::RemoveOptions;
use codex_exec_server::WalkOptions;
use codex_exec_server::WalkOutcome;
use codex_exec_server::WriteFileOptions;
use codex_utils_path_uri::PathUri;
use futures::TryStreamExt;
use std::io;
use tokio_util::bytes::Bytes;

struct CompatibilityFileSystem;

impl CompatibilityFileSystem {
    fn unused<T>() -> ExecutorFileSystemFuture<'static, T> {
        Box::pin(async { Err(io::Error::other("unused fake filesystem operation")) })
    }
}

impl ExecutorFileSystem for CompatibilityFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move { Ok(path.clone()) })
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: ReadFileOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Self::unused()
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async {
            Ok(FileSystemReadStream::new(futures::stream::once(async {
                Ok(Bytes::from_static(b"stable\n"))
            })))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _options: WriteFileOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: GetMetadataOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async {
            Ok(FileMetadata {
                is_directory: false,
                is_file: true,
                is_symlink: false,
                size: 7,
                created_at_ms: 1,
                modified_at_ms: 1,
            })
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Self::unused()
    }

    fn walk<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: WalkOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Self::unused()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unused()
    }
}

#[tokio::test]
async fn compatibility_offset_stream_fails_closed_without_scanning() -> Result<()> {
    let filesystem = CompatibilityFileSystem;
    assert!(!filesystem.supports_read_file_stream_from());
    let path = PathUri::from_host_native_path(std::env::temp_dir().join("read-file-default"))?;
    let zero = filesystem
        .read_file_stream_from(&path, /*offset*/ 0, /*sandbox*/ None)
        .await?;
    assert_eq!(
        zero.try_collect::<Vec<_>>().await?,
        vec![Bytes::from_static(b"stable\n")]
    );
    for offset in [
        1,
        7,
        codex_exec_server::FILE_READ_CHUNK_SIZE as u64,
        u64::MAX,
    ] {
        let result = filesystem
            .read_file_stream_from(&path, offset, /*sandbox*/ None)
            .await;
        let Err(error) = result else {
            panic!("default nonzero offset must fail closed at {offset}");
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
    Ok(())
}
