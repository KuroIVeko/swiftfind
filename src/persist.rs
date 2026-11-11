//! On-disk index cache, so startup doesn't need a full MFT scan. The format
//! lives next to the index it mirrors, in `index::cache`.

use std::path::PathBuf;

pub use crate::index::cache::{load, save};

pub fn cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("swiftfind").join("index.bin")
}
