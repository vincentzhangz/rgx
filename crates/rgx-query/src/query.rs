//! Query-plan construction: turn a regex pattern into a set of literal
//! strings whose covering n-grams must all be present in a matching document.
//!
//! Uses `regex-syntax`'s HIR and literal `Extractor` (the same machinery
//! powering ripgrep's literal optimizations) to find the *prefix* and
//! *suffix* literals that every match must start/end with, then converts
//! each literal into its covering n-grams.

use regex_syntax::hir::literal::{ExtractKind, Extractor};
use rgx_index::Index;
use rgx_index::ngram::{MIN_NGRAM_LENGTH, covering_ngrams, ngram_hash};

/// Compute the candidate file set for a plan against an index.
///
/// Candidates are all files whose postings contain *every* covering n-gram
/// of at least one prefix literal AND at least one suffix literal. Returns
/// `None` when the plan imposes no constraint (every file is a candidate).
pub fn candidates(index: &Index, plan: &QueryPlan) -> Option<Vec<u32>> {
    if plan.none || (plan.prefix.is_empty() && plan.suffix.is_empty()) {
        return None;
    }
    let pref = side_candidates(index, &plan.prefix);
    let suff = side_candidates(index, &plan.suffix);
    match (pref, suff) {
        (Some(a), Some(b)) => Some(intersect_sorted(&a, &b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Union over branches of (intersection over each branch's n-grams).
/// Posting lists are fetched into reused buffers to avoid fresh
/// allocations per gram.
fn side_candidates(index: &Index, branches: &[Branch]) -> Option<Vec<u32>> {
    if branches.is_empty() {
        return None;
    }
    let mut post = Vec::new();
    let mut acc = Vec::new();
    let mut tmp_acc = Vec::new();
    let mut result: Option<Vec<u32>> = None;
    let mut tmp_result = Vec::new();

    for b in branches {
        let mut first = true;
        let mut empty = false;
        for g in &b.grams {
            index.postings_into(ngram_hash(g), &mut post);
            if post.is_empty() {
                empty = true;
                break;
            }
            if first {
                acc.clear();
                acc.extend_from_slice(&post);
                first = false;
            } else {
                intersect_sorted_into(&acc, &post, &mut tmp_acc);
                std::mem::swap(&mut acc, &mut tmp_acc);
                if acc.is_empty() {
                    empty = true;
                    break;
                }
            }
        }
        if !empty && !first {
            match result.as_mut() {
                None => result = Some(std::mem::take(&mut acc)),
                Some(res) => {
                    union_sorted_into(res, &acc, &mut tmp_result);
                    std::mem::swap(res, &mut tmp_result);
                }
            }
        }
    }
    result
}

/// Merge-intersect two sorted `u32` slices, appending to `out` (cleared first).
pub fn intersect_sorted_into(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(a.len().min(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
}

/// Merge-union two sorted `u32` slices, appending to `out` (cleared first).
pub fn union_sorted_into(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

/// Merge-intersect two sorted `u32` vectors.
pub fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    intersect_sorted_into(a, b, &mut out);
    out
}

/// Merge-union two sorted `u32` vectors.
pub fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    union_sorted_into(a, b, &mut out);
    out
}

/// One literal branch of a query: every covering n-gram must be present.
#[derive(Debug, Clone)]
pub struct Branch {
    /// Distinct covering n-grams of the literal (length >= 3).
    pub grams: Vec<Vec<u8>>,
    /// The original literal bytes (for diagnostics / tests).
    pub literal: Vec<u8>,
}

/// A decomposed query.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    /// Literals that every match must *start* with (disjunction).
    pub prefix: Vec<Branch>,
    /// Literals that every match must *end* with (disjunction).
    pub suffix: Vec<Branch>,
    /// True when the pattern is trivially matched by every document
    /// (no useful literals could be extracted).
    pub none: bool,
}

impl QueryPlan {
    /// Total number of distinct covering n-grams across all branches.
    pub fn gram_count(&self) -> usize {
        self.prefix.iter().map(|b| b.grams.len()).sum::<usize>()
            + self.suffix.iter().map(|b| b.grams.len()).sum::<usize>()
    }
}

fn literal_bytes(lit: &regex_syntax::hir::literal::Literal) -> Vec<u8> {
    lit.as_bytes().to_vec()
}

/// Decompose a regex pattern into a query plan.
///
/// `fold_case` lowercases extracted literals so they match a case-folded
/// index (used for both `-i` queries and folded indexes).
pub fn decompose(pattern: &str, fold_case: bool) -> QueryPlan {
    let hir = match regex_syntax::Parser::new().parse(pattern) {
        Ok(h) => h,
        Err(_) => {
            return QueryPlan {
                prefix: Vec::new(),
                suffix: Vec::new(),
                none: true,
            };
        }
    };

    let mut ex = Extractor::new();
    ex.kind(ExtractKind::Prefix)
        .limit_total(128)
        .limit_class(16);
    let prefix_seq = ex.extract(&hir);
    let prefix = branches(&prefix_seq, fold_case);

    let mut exs = Extractor::new();
    exs.kind(ExtractKind::Suffix)
        .limit_total(128)
        .limit_class(16);
    let suffix_seq = exs.extract(&hir);
    let suffix = branches(&suffix_seq, fold_case);

    let none = prefix.is_empty() && suffix.is_empty();

    QueryPlan {
        prefix,
        suffix,
        none,
    }
}

fn branches(seq: &regex_syntax::hir::literal::Seq, fold_case: bool) -> Vec<Branch> {
    if seq.is_empty() || !seq.is_finite() {
        return Vec::new();
    }
    let lits = match seq.literals() {
        Some(l) => l,
        None => return Vec::new(),
    };
    lits.iter()
        .map(|lit| {
            let raw = literal_bytes(lit);
            let lit_bytes = if fold_case {
                raw.iter()
                    .map(|b| b.to_ascii_lowercase())
                    .collect::<Vec<u8>>()
            } else {
                raw.clone()
            };
            let mut grams = covering_ngrams(&lit_bytes, rgx_index::DEFAULT_MAX_NGRAM_LENGTH);
            grams.sort();
            grams.dedup();
            grams.retain(|g| g.len() >= MIN_NGRAM_LENGTH);
            Branch {
                grams,
                literal: lit_bytes,
            }
        })
        .filter(|b| !b.grams.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grams_of(plan: &QueryPlan) -> Vec<Vec<u8>> {
        plan.prefix
            .iter()
            .flat_map(|b| b.grams.clone())
            .chain(plan.suffix.iter().flat_map(|b| b.grams.clone()))
            .collect()
    }

    #[test]
    fn literal_pattern() {
        let p = decompose("hello world", false);
        assert!(!p.none);
        let grams = grams_of(&p);
        let expect = ["hel", "ell", "llo", "rld", "worl", "lo wo"];
        for e in expect {
            assert!(
                grams.iter().any(|g| g.as_slice() == e.as_bytes()),
                "missing gram {e:?} in {grams:?}"
            );
        }
    }

    #[test]
    fn alternation() {
        let p = decompose("cat|dog", false);
        assert!(!p.none);
        let grams = grams_of(&p);
        assert!(grams.iter().any(|g| g.as_slice() == b"cat"));
        assert!(grams.iter().any(|g| g.as_slice() == b"dog"));
    }

    #[test]
    fn dot_wildcard_is_none() {
        let p = decompose(".*", false);
        assert!(p.none);
    }

    #[test]
    fn case_folding() {
        let p = decompose("Hello", true);
        let grams = grams_of(&p);
        for g in &grams {
            assert!(g.iter().all(|b| !b.is_ascii_uppercase()));
        }
        let p2 = decompose("Hello", false);
        let grams2 = grams_of(&p2);
        assert!(grams2.iter().any(|g| g.contains(&b'H')));
    }
}
