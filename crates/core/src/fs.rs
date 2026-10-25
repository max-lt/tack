//! Filesystem capability given to the file tools.
//!
//! The tools never touch the OS directly: a host can back this with the local
//! disk, a WASI preopen, or a virtual filesystem.

use std::io;
use std::path::Path;

use futures::future::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

pub trait Fs: Send + Sync {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>>;

    fn write<'a>(&'a self, path: &'a Path, contents: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;

    fn create_dir_all<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>>;

    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>>;

    fn read_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<DirEntry>>>;

    fn is_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, bool>;

    fn read_to_string<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<String>> {
        Box::pin(async move {
            let bytes = self.read(path).await?;
            String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        })
    }
}

/// The filesystem of the process: the local disk on a native build, the
/// preopened directories on `wasm32-wasip2`.
pub struct LocalFs;

#[cfg(not(target_arch = "wasm32"))]
impl Fs for LocalFs {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        Box::pin(tokio::fs::read(path))
    }

    fn write<'a>(&'a self, path: &'a Path, contents: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(tokio::fs::write(path, contents))
    }

    fn create_dir_all<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(tokio::fs::create_dir_all(path))
    }

    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(tokio::fs::remove_file(path))
    }

    fn read_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<DirEntry>>> {
        Box::pin(async move {
            let mut read_dir = tokio::fs::read_dir(path).await?;
            let mut entries = Vec::new();

            while let Some(entry) = read_dir.next_entry().await? {
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };

                entries.push(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: kind_of(file_type),
                });
            }

            Ok(entries)
        })
    }

    fn is_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(async move { tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()) })
    }
}

#[cfg(target_arch = "wasm32")]
impl Fs for LocalFs {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        Box::pin(std::future::ready(std::fs::read(path)))
    }

    fn write<'a>(&'a self, path: &'a Path, contents: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(std::future::ready(std::fs::write(path, contents)))
    }

    fn create_dir_all<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(std::future::ready(std::fs::create_dir_all(path)))
    }

    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(std::future::ready(std::fs::remove_file(path)))
    }

    fn read_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<DirEntry>>> {
        let entries = std::fs::read_dir(path).map(|rd| {
            rd.filter_map(|entry| {
                let entry = entry.ok()?;
                let file_type = entry.file_type().ok()?;

                Some(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: kind_of(file_type),
                })
            })
            .collect()
        });

        Box::pin(std::future::ready(entries))
    }

    fn is_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(path.is_dir()))
    }
}

fn kind_of(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    }
}
