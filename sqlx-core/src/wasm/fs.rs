use std::io;
use std::path::{Path, PathBuf};

use crate::fs::{DirEntry, ReadDir};

// WASM32 stub implementations for async fs functions
pub async fn read<P: AsRef<Path>>(_path: P) -> io::Result<Vec<u8>> {
    todo!("fs::read is not implemented for wasm32")
}

pub async fn read_to_string<P: AsRef<Path>>(_path: P) -> io::Result<String> {
    todo!("fs::read_to_string is not implemented for wasm32")
}

pub async fn create_dir_all<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    todo!("fs::create_dir_all is not implemented for wasm32")
}

pub async fn remove_file<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    todo!("fs::remove_file is not implemented for wasm32")
}

pub async fn remove_dir<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    todo!("fs::remove_dir is not implemented for wasm32")
}

pub async fn remove_dir_all<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    todo!("fs::remove_dir_all is not implemented for wasm32")
}

pub async fn read_dir(_path: PathBuf) -> io::Result<ReadDir> {
    todo!("fs::read_dir is not implemented for wasm32")
}

pub async fn next(_read_dir: &mut ReadDir) -> io::Result<Option<DirEntry>> {
    todo!("fs::ReadDir::next is not implemented for wasm32")
}
