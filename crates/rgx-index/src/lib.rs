//! Sparse n-gram inverted index for fast regex search, following Cursor's
//! "Fast regex search" paper (<https://cursor.com/blog/fast-regex-search>).
//!
//! The index is built from `BuildAllNgrams` output and queried with the
//! `BuildCoveringNgrams` covering set (the algorithm from GitHub's Blackbird
//! code search). Storage uses the paper's two-file layout: a sorted,
//! mmap'd `lookup.dat` of `(ngram_hash, offset)` and a `postings.dat` of
//! length-prefixed posting lists read at the returned offset.
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use rgx_index::{Index, ScanOptions, build_index};
//!
//! let root = std::path::Path::new(".");
//! let index_dir = root.join(".rgx");
//! let mut progress = |_: &str| {};
//! build_index(root, &index_dir, &ScanOptions::default(), &mut progress)?;
//!
//! let index = Index::open(&index_dir)?;
//! println!("indexed {} files", index.file_count());
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

pub mod fingerprint;
pub mod index;
pub mod ngram;
pub mod scanner;

pub use index::{BuildStats, Index, build_index, update_index};
pub use ngram::{DEFAULT_MAX_NGRAM_LENGTH, MIN_NGRAM_LENGTH};
pub use scanner::{DEFAULT_MAX_FILE_SIZE, ScanOptions};
