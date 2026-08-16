//! Sparse n-gram generation, ported exactly from GitHub's Blackbird code
//! search engine (the reference implementation behind Cursor's "Fast regex
//! search" paper).
//!
//! See: <https://github.com/danlark1/sparse_ngrams> (Boost License 1.0) and
//! the C# port in dotnet/roslyn `SparseNgramGenerator.cs`.
//!
//! A bigram-weight (hash) function assigns a deterministic weight to every
//! adjacent character pair. N-gram boundaries fall at "hash valleys" (local
//! minima). The result is a sparse subset of *variable-length* n-grams: at
//! most `2n-2` for indexing and at most `n-2` for querying, all of length
//! `>= 3`. Longer n-grams give far higher selectivity than fixed trigrams.
//!
//! # Soundness
//!
//! If a document contains a query substring `Q`, then every covering n-gram
//! of `Q` was emitted by `build_all_ngrams` when that document was indexed
//! (the covering n-grams are a subset of the all n-grams of any string that
//! contains `Q`). This is what makes index-time checking sound; it is
//! verified by the property tests in this module.

/// The shortest n-gram the algorithm can produce (a trigram).
pub const MIN_NGRAM_LENGTH: usize = 3;

/// Default maximum covering n-gram length used at query time.
pub const DEFAULT_MAX_NGRAM_LENGTH: usize = 16;

const MUL1: u64 = 0xc6a4a7935bd1e995;
const MUL2: u64 = 0x228876a7198b743;

/// Deterministic weight of the bigram at `s[0..2]`.
///
/// This is the exact hash function Cursor / GitHub's production code search
/// uses. Any deterministic function works for the algorithm; this one
/// spreads common bigrams widely so boundaries do not cluster.
#[inline]
pub fn hash_bigram(s: &[u8]) -> u32 {
    debug_assert!(s.len() >= 2);
    let a = (s[0] as u64)
        .wrapping_mul(MUL1)
        .wrapping_add((s[1] as u64).wrapping_mul(MUL2));
    (a + (!a >> 47)) as u32
}

/// 64-bit hash of a variable-length n-gram, used as the inverted-index key.
///
/// FNV-1a over the n-gram bytes. Collisions only ever *broaden* a posting
/// list (never produce a false negative), so a plain hash is sound.
#[inline]
pub fn ngram_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Emit every sparse n-gram of `s` (indexing). O(n), at most `2n-2` grams.
///
/// `consumer` is called with each emitted byte slice, in arbitrary order and
/// potentially with duplicates.
pub fn build_all_ngrams(s: &[u8], consumer: &mut impl FnMut(&[u8])) {
    let n = s.len();
    let mut st: Vec<(u32, usize)> = Vec::with_capacity((n / 2 + 1).min(64));
    let mut i = 0usize;
    while i + 2 <= n {
        let hash = hash_bigram(&s[i..]);
        while let Some(&(top_hash, _)) = st.last() {
            if hash > top_hash {
                let pos = st.last().expect("stack non-empty").1;
                consumer(&s[pos..i + 2]);
                while st.len() > 1 && st[st.len() - 1].0 == st[st.len() - 2].0 {
                    st.pop();
                }
                st.pop();
            } else {
                break;
            }
        }
        if !st.is_empty() {
            let pos = st.last().expect("stack non-empty").1;
            consumer(&s[pos..i + 2]);
        }
        st.push((hash, i));
        i += 1;
    }
}

/// Emit the minimal *covering* set of n-grams for `s` (querying).
/// O(n), at most `n-2` grams, each no longer than `max_ngram_length`.
pub fn build_covering_ngrams(s: &[u8], consumer: &mut impl FnMut(&[u8]), max_ngram_length: usize) {
    let n = s.len();
    let mut st: std::collections::VecDeque<(u32, usize)> =
        std::collections::VecDeque::with_capacity((n / 2 + 1).min(32));
    let mut i = 0usize;
    while i + 2 <= n {
        let hash = hash_bigram(&s[i..]);
        if st.len() > 1 && i - st.front().expect("non-empty").1 + 3 >= max_ngram_length {
            let front_pos = st.front().expect("non-empty").1;
            let second_pos = st[1].1;
            consumer(&s[front_pos..second_pos + 2]);
            st.pop_front();
        }
        while let Some(&(back_hash, _)) = st.back() {
            if hash > back_hash {
                if st.front().expect("non-empty").0 == st.back().expect("non-empty").0 {
                    let back_pos = st.back().expect("non-empty").1;
                    consumer(&s[back_pos..i + 2]);
                    while st.len() > 1 {
                        let last_position = st.back().expect("non-empty").1 + 2;
                        st.pop_back();
                        let new_back_pos = st.back().expect("non-empty").1;
                        consumer(&s[new_back_pos..last_position]);
                    }
                }
                st.pop_back();
            } else {
                break;
            }
        }
        st.push_back((hash, i));
        i += 1;
    }
    while st.len() > 1 {
        let last_position = st.back().expect("non-empty").1 + 2;
        st.pop_back();
        let back_pos = st.back().expect("non-empty").1;
        consumer(&s[back_pos..last_position]);
    }
}

/// Collect the distinct sparse n-grams of `s` (indexing convenience).
pub fn all_ngrams(s: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    build_all_ngrams(s, &mut |g| {
        if seen.insert(g.to_vec()) {
            out.push(g.to_vec());
        }
    });
    out
}

/// Collect the covering n-grams of `s` (query convenience).
pub fn covering_ngrams(s: &[u8], max_ngram_length: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    build_covering_ngrams(s, &mut |g| out.push(g.to_vec()), max_ngram_length);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn set<const N: usize>(vals: [&str; N]) -> HashSet<String> {
        vals.into_iter().map(String::from).collect()
    }

    fn all(s: &str) -> HashSet<String> {
        all_ngrams(s.as_bytes())
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect()
    }

    fn covering(s: &str) -> HashSet<String> {
        covering_ngrams(s.as_bytes(), DEFAULT_MAX_NGRAM_LENGTH)
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect()
    }

    #[test]
    fn reference_simple() {
        assert!(all("he").is_empty());
        assert_eq!(all("hel"), set(["hel"]));
        assert_eq!(all("hell"), set(["hel", "ell"]));
        assert_eq!(
            all("hello world"),
            set([
                "hel", "ell", "llo", "lo ", "o w", "lo w", " wo", "lo wo", "wor", "orl", "worl",
                "rld"
            ])
        );
    }

    #[test]
    fn reference_simple_covering() {
        assert!(covering("he").is_empty());
        assert_eq!(covering("hel"), set(["hel"]));
        assert_eq!(covering("hell"), set(["hel", "ell"]));
        assert_eq!(
            covering("hello world"),
            set(["hel", "ell", "llo", "rld", "worl", "lo wo"])
        );
    }

    #[test]
    fn reference_split_github_codesearch() {
        assert_eq!(
            all("chester "),
            set([
                "che", "hes", "ches", "est", "chest", "ste", "ter", "ster", "er "
            ])
        );
        assert_eq!(covering("chester "), set(["chest", "ster", "er "]));
        assert_eq!(covering("chester"), set(["chest", "ster"]));
    }

    #[test]
    fn reference_split_for_loop() {
        assert_eq!(
            all("for(int i=42"),
            set([
                "for", "or(", "for(", "r(i", "for(i", "(in", "int", "(int", "nt ", "t i", " i=",
                "t i=", "i=4", "t i=4", "nt i=4", "(int i=4", "=42"
            ])
        );
        assert_eq!(covering("for(int i=42"), set(["for(i", "(int i=4", "=42"]));
    }

    #[test]
    fn min_length_and_bounds() {
        for s in ["a", "ab", "abc", "abcd", "abcde"] {
            let all = all(s);
            assert!(all.iter().all(|g| g.len() >= MIN_NGRAM_LENGTH));
            let cov = covering(s);
            assert!(cov.iter().all(|g| g.len() >= MIN_NGRAM_LENGTH));
            assert!(cov.len() <= s.len().saturating_sub(2));
        }
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    fn random_text(rng: &mut Lcg, len: usize) -> Vec<u8> {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_ .;:{}()[]=+-*/\"'\\\n\t<>!?&#%$@~^|";
        (0..len)
            .map(|_| {
                if rng.next().is_multiple_of(16) {
                    rng.next() as u8
                } else {
                    ALPHABET[(rng.next() as usize) % ALPHABET.len()]
                }
            })
            .collect()
    }

    #[test]
    fn covering_is_subset_of_all_for_same_string() {
        let mut rng = Lcg(0x5eed);
        for _ in 0..2000 {
            let len = (rng.next() % 120) as usize;
            let s = random_text(&mut rng, len);
            let all: HashSet<Vec<u8>> = all_ngrams(&s).into_iter().collect();
            let cov: HashSet<Vec<u8>> = covering_ngrams(&s, DEFAULT_MAX_NGRAM_LENGTH)
                .into_iter()
                .collect();
            assert!(
                cov.is_subset(&all),
                "cov={cov:?} not subset of all={all:?} for {s:?}"
            );
        }
    }

    #[test]
    fn covering_of_substring_is_subset_of_all_of_document() {
        let mut rng = Lcg(0xacce55);
        for _ in 0..2000 {
            let dlen = (rng.next() % 200) as usize;
            let d = random_text(&mut rng, dlen);
            if d.len() < MIN_NGRAM_LENGTH {
                continue;
            }
            let all_d: HashSet<Vec<u8>> = all_ngrams(&d).into_iter().collect();
            for _ in 0..5 {
                let start = (rng.next() as usize) % d.len();
                let remaining = d.len() - start;
                let len = (rng.next() as usize) % remaining + 1;
                let q = &d[start..start + len];
                let cov_q: HashSet<Vec<u8>> = covering_ngrams(q, DEFAULT_MAX_NGRAM_LENGTH)
                    .into_iter()
                    .collect();
                assert!(
                    cov_q.is_subset(&all_d),
                    "covering({q:?}) not subset of all({d:?})\ncov={cov_q:?}\nall={all_d:?}"
                );
            }
        }
    }
}
