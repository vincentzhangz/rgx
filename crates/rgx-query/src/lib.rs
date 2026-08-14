//! Regex decomposition into sparse n-gram query plans for rgx.
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use rgx_index::{Index, ScanOptions, build_index};
//! use rgx_query::{candidates, decompose};
//!
//! let root = std::path::Path::new(".");
//! let index_dir = root.join(".rgx");
//! let mut progress = |_: &str| {};
//! build_index(root, &index_dir, &ScanOptions::default(), &mut progress)?;
//! let index = Index::open(&index_dir)?;
//!
//! let plan = decompose("fn main", true);
//! let cands = candidates(&index, &plan)
//!     .unwrap_or_else(|| (0..index.file_count() as u32).collect());
//! println!("{} candidate files", cands.len());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod query;

pub use query::{Branch, QueryPlan, candidates, decompose, intersect_sorted, union_sorted};
