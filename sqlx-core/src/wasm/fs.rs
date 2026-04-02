use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use wasip3::filesystem::{
    preopens,
    types::{DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags},
};

use crate::fs::{DirEntry, ReadDir};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the preopened directory descriptor whose path best covers `path`,
/// and return `(descriptor, relative_path_string)`.
///
/// WASI uses a capability model: all paths must be relative to a preopened
/// descriptor. We pick the preopen with the longest matching prefix so that
/// the most specific sandbox root wins.
fn resolve_path(path: &Path) -> io::Result<(wasip3::filesystem::types::Descriptor, String)> {
    let path_str = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path"))?;

    let dirs = preopens::get_directories();

    // Relative paths: use the first preopen as root.
    if !path_str.starts_with('/') {
        if let Some((desc, _)) = dirs.into_iter().next() {
            return Ok((desc, path_str.trim_start_matches("./").to_string()));
        }
        return Err(io::Error::new(io::ErrorKind::NotFound, "no preopens available"));
    }

    // Absolute paths: find the preopen whose string is the longest prefix.
    let best = dirs
        .into_iter()
        .filter_map(|(desc, root)| {
            let normalized = root.trim_end_matches('/');
            if path_str.starts_with(normalized) {
                let rel = path_str[normalized.len()..]
                    .trim_start_matches('/')
                    .to_string();
                Some((desc, rel, normalized.len()))
            } else {
                None
            }
        })
        .max_by_key(|(_, _, depth)| *depth);

    best.map(|(desc, rel, _)| (desc, rel))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no preopen covers path: {}", path.display()),
            )
        })
}

/// Map a WASI `ErrorCode` to a `std::io::Error`.
fn wasi_err(code: ErrorCode) -> io::Error {
    use io::ErrorKind as K;
    let kind = match code {
        ErrorCode::Access | ErrorCode::NotPermitted => K::PermissionDenied,
        ErrorCode::NoEntry => K::NotFound,
        ErrorCode::Exist => K::AlreadyExists,
        ErrorCode::NotDirectory => K::NotADirectory,
        ErrorCode::IsDirectory => K::IsADirectory,
        ErrorCode::ReadOnly => K::ReadOnlyFilesystem,
        ErrorCode::NotEmpty => K::DirectoryNotEmpty,
        ErrorCode::NameTooLong => K::InvalidInput,
        ErrorCode::InsufficientMemory => K::OutOfMemory,
        ErrorCode::Unsupported => K::Unsupported,
        ErrorCode::InvalidSeek => K::InvalidInput,
        _ => K::Other,
    };
    io::Error::new(kind, format!("{code:?}"))
}

// ---------------------------------------------------------------------------
// Public async fs functions
// ---------------------------------------------------------------------------

pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let (dir, rel) = resolve_path(path.as_ref())?;
    let file = dir
        .open_at(
            PathFlags::SYMLINK_FOLLOW,
            rel,
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .map_err(wasi_err)?;

    // read_via_stream is NOT async — returns (StreamReader<u8>, FutureReader<…>) directly.
    // Await the stream to collect all bytes, then await the completion future for errors.
    let (stream, result_fut) = file.read_via_stream(0);
    let bytes = stream.collect().await;
    result_fut.await.map_err(wasi_err)?;
    Ok(bytes)
}

pub async fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let (dir, rel) = resolve_path(path.as_ref())?;
    if rel.is_empty() {
        return Ok(());
    }

    let mut cur = String::new();
    for component in Path::new(&rel).components() {
        if let Component::Normal(c) = component {
            let c = c.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path component")
            })?;
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(c);
            match dir.create_directory_at(cur.clone()).await {
                Ok(()) | Err(ErrorCode::Exist) => {}
                Err(e) => return Err(wasi_err(e)),
            }
        }
    }
    Ok(())
}

pub async fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let (dir, rel) = resolve_path(path.as_ref())?;
    // unlink_file_at accepts paths with subdirectory components (like unlinkat(2))
    dir.unlink_file_at(rel).await.map_err(wasi_err)
}

pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let (dir, rel) = resolve_path(path.as_ref())?;
    dir.remove_directory_at(rel).await.map_err(wasi_err)
}

pub async fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    remove_dir_all_inner(path.as_ref()).await
}

/// Boxed recursive helper required because async recursion needs an explicit
/// `Box::pin` to give the compiler a known stack size.
fn remove_dir_all_inner(path: &Path) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>> {
    Box::pin(async move {
        let (dir, rel) = resolve_path(path)?;
        let child_dir = dir
            .open_at(
                PathFlags::empty(),
                rel,
                OpenFlags::DIRECTORY,
                DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
            )
            .await
            .map_err(wasi_err)?;

        let (stream, result_fut) = child_dir.read_directory();
        let entries = stream.collect().await;
        result_fut.await.map_err(wasi_err)?;

        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let child_path = path.join(&entry.name);
            if matches!(entry.type_, DescriptorType::Directory) {
                remove_dir_all_inner(&child_path).await?;
            } else {
                remove_file(&child_path).await?;
            }
        }

        remove_dir(path).await
    })
}

pub async fn read_dir(path: PathBuf) -> io::Result<ReadDir> {
    let (dir, rel) = resolve_path(&path)?;
    let child_dir = dir
        .open_at(
            PathFlags::SYMLINK_FOLLOW,
            rel,
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
        )
        .await
        .map_err(wasi_err)?;

    let (stream, result_fut) = child_dir.read_directory();
    let raw_entries = stream.collect().await;
    result_fut.await.map_err(wasi_err)?;

    let mut entries = VecDeque::new();
    for entry in raw_entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let entry_path = path.join(&entry.name);
        // std::fs::metadata is available on wasm32-wasip2 via the standard
        // library's WASI P2 syscall bindings.
        let metadata = std::fs::metadata(&entry_path)?;
        entries.push_back(DirEntry {
            path: entry_path,
            file_name: entry.name.into(),
            metadata,
        });
    }

    Ok(ReadDir { entries })
}

pub async fn next(read_dir: &mut ReadDir) -> io::Result<Option<DirEntry>> {
    Ok(read_dir.entries.pop_front())
}
