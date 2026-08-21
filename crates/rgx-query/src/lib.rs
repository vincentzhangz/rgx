//! Regex decomposition into sparse n-gram query plans for rgx.
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use rgx_index::{Index, ScanOptions, build_index};
//! use rgx_query::{Candidates, candidates, decompose};
//!
//! let root = std::path::Path::new(".");
//! let index_dir = root.join(".rgx");
//! let mut progress = |_: &str| {};
//! build_index(root, &index_dir, &ScanOptions::default(), &mut progress)?;
//! let index = Index::open(&index_dir)?;
//!
//! let plan = decompose("fn main");
//! let n_cands = match candidates(&index, &plan) {
//!     Candidates::All => index.file_count(),
//!     Candidates::None => 0,
//!     Candidates::Some(ids) => ids.len(),
//! };
//! println!("{n_cands} candidate files");
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod query;

pub use query::{
    Branch, Candidates, QueryPlan, candidates, decompose, intersect_sorted, union_sorted,
};
