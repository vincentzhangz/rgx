//! rgx — fast indexed regex search, following Cursor's "Fast regex search"
//! paper.
//!
//! Builds a sparse n-gram inverted index under `<root>/.rgx/` on first use,
//! then searches it. See `rgx_index`'s [`rgx_index::index`] module for the
//! on-disk format and algorithm details.
//!
//! This library crate exposes the full CLI as a callable function
//! ([`execute`]) so it can be tested in-process; the `rgx` binary is a thin
//! wrapper around [`run`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod matcher;

use matcher::{Match, match_content_str};
use rgx_index::{Index, ScanOptions, build_index, index_format_stale, update_index};
use rgx_query::{candidates, decompose};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// Name of the index directory created inside the search root.
pub const INDEX_DIR_NAME: &str = ".rgx";

/// Parsed command-line configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The regex pattern to search for.
    pub pattern: String,
    /// Search root directory (`.` when empty).
    pub root: PathBuf,
    /// Case-insensitive matching (`-i`).
    pub ignore_case: bool,
    /// Rebuild the index before searching (`--build`).
    pub build: bool,
    /// Rebuild the index (`--update`; currently a full rebuild).
    pub update: bool,
    /// Skip the index entirely (`--no-index`).
    pub no_index: bool,
    /// Print index statistics to stderr (`--stats`).
    pub stats: bool,
    /// Print timing breakdown to stderr (`--time`).
    pub time: bool,
    /// Emit JSON Lines with submatch offsets (`--json`).
    pub json: bool,
    /// Follow symbolic links while scanning (`--follow`).
    pub follow_symlinks: bool,
}

const USAGE: &str = "\
rgx — fast indexed regex search (Cursor 'fast regex search' paper)

USAGE:
    rgx [OPTIONS] <PATTERN> [PATH]

ARGUMENTS:
    <PATTERN>          Regex pattern to search for
    [PATH]             Directory to search (default: .)

OPTIONS:
    -h, --help         Print help
    -i, --ignore-case  Case-insensitive search
        --build        (Re)build the index before searching
        --update       Incrementally update the index (only changed files
                       are re-read; falls back to a full rebuild if needed)
        --no-index     Search without using the index (brute force)
        --stats        Print index statistics
        --time         Print timing breakdown
        --json         Emit JSON Lines with submatch byte offsets
        --follow       Follow symbolic links while scanning

EXIT CODES:
    0   matches found
    1   no matches
    2   error
";

/// Entry point used by the `rgx` binary: reads process args and writes to
/// the real stdout/stderr. Returns the process exit code.
pub fn run() -> i32 {
    let out = std::io::stdout();
    let err = std::io::stderr();
    execute(std::env::args().skip(1), &mut out.lock(), &mut err.lock())
}

/// Run rgx with explicit arguments and output sinks.
///
/// All matching results go to `out`; diagnostics, stats and errors go to
/// `err`. Returns the process exit code (0 matches, 1 none, 2 error).
pub fn execute<I>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = String>,
{
    let cfg = match parse_args(args.into_iter().collect()) {
        Ok(Some(c)) => c,
        Ok(None) => {
            let _ = write!(out, "{USAGE}");
            return 0;
        }
        Err(msg) => {
            let _ = writeln!(err, "rgx: {msg}\n\n{USAGE}");
            return 2;
        }
    };
    execute_cfg(cfg, out, err)
}

/// Build the [`ScanOptions`] derived from a parsed [`Config`].
fn scan_opts(cfg: &Config) -> ScanOptions {
    ScanOptions {
        follow_symlinks: cfg.follow_symlinks,
        ..Default::default()
    }
}

fn execute_cfg(cfg: Config, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let root = if cfg.root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cfg.root.clone()
    };
    let root_abs = rgx_index::display_root(&root);

    let t_total = Instant::now();
    let mut t_build = None;
    let mut build_stats = None;
    let mut t_load = None;

    let (index, file_ids): (Option<Index>, Vec<u32>) = if cfg.no_index {
        let t0 = Instant::now();
        let files = rgx_index::scanner::scan(&root_abs, &scan_opts(&cfg));
        let ids = (0..files.len() as u32).collect();
        if cfg.time {
            let _ = writeln!(
                err,
                "rgx: scan (no-index): {} ms ({} files)",
                t0.elapsed().as_millis(),
                files.len()
            );
        }
        (None, ids)
    } else {
        let idx_dir = root_abs.join(INDEX_DIR_NAME);
        let index_missing = !idx_dir.join("lookup.dat").exists()
            || !idx_dir.join("postings.dat").exists()
            || !idx_dir.join("files.dat").exists();
        let index_stale = index_format_stale(&idx_dir);
        if index_stale {
            let _ = writeln!(err, "rgx: rebuilding index (format RGXLOOK1 -> RGXLOOK2)");
        }

        if cfg.build || cfg.update || index_missing || index_stale {
            let t0 = Instant::now();
            let mut progress = |msg: &str| {
                if cfg.stats {
                    let _ = writeln!(err, "rgx: {msg}");
                }
            };
            let incremental = cfg.update && !cfg.build && !index_missing && !index_stale;
            let result = if incremental {
                update_index(&root_abs, &idx_dir, &scan_opts(&cfg), &mut progress)
            } else {
                build_index(&root_abs, &idx_dir, &scan_opts(&cfg), &mut progress)
            };
            match result {
                Ok(stats) => {
                    t_build = Some(t0.elapsed());
                    if cfg.time {
                        let _ = writeln!(
                            err,
                            "rgx: index {}: {:?} ({} files, {} reused, {} n-grams)",
                            if incremental { "update" } else { "build" },
                            t0.elapsed(),
                            stats.files,
                            stats.reused_files,
                            stats.ngrams
                        );
                    }
                    build_stats = Some(stats);
                }
                Err(e) => {
                    let _ = writeln!(err, "rgx: index build failed: {e}");
                    return 2;
                }
            }
        }

        let t0 = Instant::now();
        match Index::open(&idx_dir) {
            Ok(idx) => {
                t_load = Some(t0.elapsed());
                (Some(idx), Vec::new())
            }
            Err(e) => {
                let _ = writeln!(err, "rgx: cannot load index at {}: {e}", idx_dir.display());
                return 2;
            }
        }
    };

    let t_plan = Instant::now();
    let plan = decompose(&cfg.pattern, true);
    let mut t_cand = None;
    let cand_ids: Vec<u32> = match &index {
        Some(idx) => {
            let t0 = Instant::now();
            let c = candidates(idx, &plan);
            t_cand = Some(t0.elapsed());
            c.unwrap_or_else(|| (0..idx.file_count() as u32).collect())
        }
        None => file_ids.clone(),
    };
    if cfg.time {
        let _ = writeln!(
            err,
            "rgx: plan: {} ms, candidates: {} ms, {} candidates",
            t_plan.elapsed().as_millis(),
            t_cand.map(|d| d.as_millis()).unwrap_or(0),
            cand_ids.len()
        );
    }

    let re = match regex::bytes::RegexBuilder::new(&cfg.pattern)
        .case_insensitive(cfg.ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(err, "rgx: invalid pattern: {e}");
            return 2;
        }
    };

    let t_match = Instant::now();
    let matches = if let Some(idx) = &index {
        parallel_match(idx, &cand_ids, &re, cfg.json)
    } else {
        use std::io::Read;
        let files = rgx_index::scanner::scan(&root_abs, &scan_opts(&cfg));
        let mut out = Vec::new();
        let mut file_buf = Vec::new();
        for p in &files {
            let Ok(mut f) = std::fs::File::open(p) else {
                continue;
            };
            file_buf.clear();
            if f.read_to_end(&mut file_buf).is_ok() {
                match_content_str(&re, &file_buf, &p.to_string_lossy(), cfg.json, &mut out);
            }
        }
        out
    };
    let match_ms = t_match.elapsed().as_millis();

    let mut matches = matches;
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let mut out = std::io::BufWriter::new(out);
    for m in &matches {
        if cfg.json {
            let _ = out.write_all(b"{\"path\":");
            let _ = write_json_escaped(&mut out, &m.path);
            let _ = write!(out, ",\"line_number\":{},\"line\":", m.line);
            let _ = write_json_escaped(&mut out, &m.text);
            let _ = out.write_all(b",\"submatches\":[");
            for (idx, (s, e)) in m.submatches.iter().enumerate() {
                if idx > 0 {
                    let _ = out.write_all(b",");
                }
                let _ = write!(out, "{{\"start\":{s},\"end\":{e}}}");
            }
            let _ = out.write_all(b"]}\n");
        } else {
            let _ = writeln!(out, "{}:{}:{}", m.path, m.line, m.text);
        }
    }
    let _ = out.flush();

    if cfg.stats {
        if let Some(s) = &build_stats {
            let _ = writeln!(
                err,
                "rgx: build: {} files ({} reused), {} MiB read, {} n-grams, {} postings, index {} bytes ({:?})",
                s.files,
                s.reused_files,
                s.bytes_read / (1 << 20),
                s.ngrams,
                s.postings,
                s.index_bytes,
                s.elapsed
            );
        }
        if let Some(idx) = &index {
            let _ = writeln!(
                err,
                "rgx: index: {} files, {} n-grams, {} postings",
                idx.file_count(),
                idx.ngram_count(),
                idx.posting_count()
            );
        }
        let _ = writeln!(
            err,
            "rgx: query: {} n-grams, {} candidates, {} matches",
            plan.gram_count(),
            cand_ids.len(),
            matches.len()
        );
    }
    if cfg.time {
        if let Some(d) = t_build {
            let _ = writeln!(err, "rgx: build: {:?}", d);
        }
        if let Some(d) = t_load {
            let _ = writeln!(err, "rgx: load: {:?}", d);
        }
        let _ = writeln!(err, "rgx: match: {match_ms} ms");
        let _ = writeln!(err, "rgx: total: {:?}", t_total.elapsed());
    }

    if matches.is_empty() { 1 } else { 0 }
}

/// Search an already-loaded [`Index`] for `pattern`, returning every
/// matching line sorted by path then line number.
///
/// This is the programmatic entry point: it decomposes the pattern into a
/// covering n-gram plan, prunes candidates through the index, and verifies
/// each candidate exactly with `regex`. An invalid pattern is returned as a
/// [`regex::Error`]. When the pattern yields no useful literals, every
/// indexed file is treated as a candidate.
///
/// Query plans are ASCII-folded to match the on-disk index. `ignore_case`
/// only affects the `regex` verification step, not pruning.
pub fn search(index: &Index, pattern: &str, ignore_case: bool) -> Result<Vec<Match>, regex::Error> {
    let plan = decompose(pattern, true);
    let cand_ids =
        candidates(index, &plan).unwrap_or_else(|| (0..index.file_count() as u32).collect());
    let re = regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()?;
    let mut matches = parallel_match(index, &cand_ids, &re, false);
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(matches)
}

/// Run the matcher over candidates in parallel (std threads, no rayon).
fn parallel_match(
    index: &Index,
    cand_ids: &[u32],
    re: &regex::bytes::Regex,
    json: bool,
) -> Vec<Match> {
    use std::io::Read;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);
    if cand_ids.is_empty() {
        return Vec::new();
    }
    let chunk = cand_ids.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = cand_ids
            .chunks(chunk)
            .map(|slice| {
                s.spawn(move || {
                    let mut out = Vec::new();
                    let mut file_buf = Vec::new();
                    for &id in slice {
                        let Some(path) = index.file_path(id) else {
                            continue;
                        };
                        let Ok(mut file) = std::fs::File::open(path) else {
                            continue;
                        };
                        file_buf.clear();
                        if file.read_to_end(&mut file_buf).is_err() {
                            continue;
                        }
                        match_content_str(re, &file_buf, &path.to_string_lossy(), json, &mut out);
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

fn write_json_escaped(w: &mut dyn Write, s: &str) -> std::io::Result<()> {
    w.write_all(b"\"")?;
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            _ if (c as u32) < 0x20 => {
                w.write_all(&bytes[start..i])?;
                write!(w, "\\u{:04x}", c as u32)?;
                start = i + c.len_utf8();
                continue;
            }
            _ => continue,
        };
        w.write_all(&bytes[start..i])?;
        w.write_all(esc.as_bytes())?;
        start = i + c.len_utf8();
    }
    w.write_all(&bytes[start..])?;
    w.write_all(b"\"")?;
    Ok(())
}

/// Escape a string for a JSON string literal.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parse command-line arguments (program args, without the binary name).
///
/// Returns `Ok(None)` when `-h`/`--help` was requested (the caller prints
/// [`USAGE`]).
pub fn parse_args(args: Vec<String>) -> Result<Option<Config>, String> {
    let mut cfg = Config {
        pattern: String::new(),
        root: PathBuf::new(),
        ignore_case: false,
        build: false,
        update: false,
        no_index: false,
        stats: false,
        time: false,
        json: false,
        follow_symlinks: false,
    };
    let mut positional = Vec::new();
    let mut end_flags = false;
    for a in args {
        if !end_flags && a == "--" {
            end_flags = true;
            continue;
        }
        if !end_flags && a.starts_with('-') && a.len() > 1 {
            match a.as_str() {
                "-h" | "--help" => return Ok(None),
                "-i" | "--ignore-case" => cfg.ignore_case = true,
                "--build" => cfg.build = true,
                "--update" => cfg.update = true,
                "--no-index" => cfg.no_index = true,
                "--stats" => cfg.stats = true,
                "--time" => cfg.time = true,
                "--json" => cfg.json = true,
                "--follow" => cfg.follow_symlinks = true,
                _ => return Err(format!("unknown option: {a}")),
            }
        } else {
            positional.push(a);
        }
    }
    match positional.len() {
        0 => Err("missing PATTERN".to_string()),
        1 => {
            cfg.pattern = positional.remove(0);
            Ok(Some(cfg))
        }
        2 => {
            cfg.pattern = positional.remove(0);
            cfg.root = PathBuf::from(positional.remove(0));
            Ok(Some(cfg))
        }
        _ => Err("too many arguments".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let cfg = parse_args(vec!["foo".into(), "src".into()])
            .unwrap()
            .unwrap();
        assert_eq!(cfg.pattern, "foo");
        assert_eq!(cfg.root, PathBuf::from("src"));
        assert!(!cfg.ignore_case);
    }

    #[test]
    fn parse_flags() {
        let cfg = parse_args(vec!["-i".into(), "--json".into(), "foo".into()])
            .unwrap()
            .unwrap();
        assert!(cfg.ignore_case);
        assert!(cfg.json);
        assert_eq!(cfg.pattern, "foo");
    }

    #[test]
    fn parse_double_dash_pattern() {
        let cfg = parse_args(vec!["--".into(), "-weird".into()])
            .unwrap()
            .unwrap();
        assert_eq!(cfg.pattern, "-weird");
    }

    #[test]
    fn parse_missing_pattern_errors() {
        assert!(parse_args(vec![]).is_err());
        assert!(parse_args(vec!["--stats".into()]).is_err());
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("line1\nline2"), "\"line1\\nline2\"");
    }

    fn remove_all_retry(path: &std::path::Path) {
        for attempt in 0..5 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    ) && attempt + 1 < 5 =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (1 << attempt)));
                }
                Err(_) => return,
            }
        }
    }

    fn search_index(name: &str, files: &[(&str, &str)]) -> Index {
        let root =
            std::env::temp_dir().join(format!("rgx-search-test-{name}-{}", std::process::id()));
        remove_all_retry(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        let idx_dir = root.join(INDEX_DIR_NAME);
        let mut progress = |_: &str| {};
        rgx_index::build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        Index::open(&idx_dir).unwrap()
    }

    #[test]
    fn search_returns_sorted_matches() {
        let index = search_index(
            "sorted",
            &[
                ("a.txt", "line one\nhello there\n"),
                ("b.txt", "hello world\n"),
            ],
        );
        let matches = search(&index, "hello", false).unwrap();
        assert_eq!(matches.len(), 2);
        assert!(
            matches[0].path.ends_with("a.txt"),
            "path: {}",
            matches[0].path
        );
        assert!(
            matches[1].path.ends_with("b.txt"),
            "path: {}",
            matches[1].path
        );
        assert_eq!(matches[0].line, 2);
        assert_eq!(matches[1].line, 1);
    }

    #[test]
    fn search_respects_ignore_case() {
        let index = search_index(
            "case",
            &[("a.txt", "Hello world\n"), ("b.txt", "goodbye\n")],
        );
        assert!(search(&index, "hello", false).unwrap().is_empty());
        let matches = search(&index, "hello", true).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn search_no_match_is_empty() {
        let index = search_index("none", &[("a.txt", "nothing here\n")]);
        let matches = search(&index, "zzzzqqqq", false).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn search_invalid_pattern_errors() {
        let index = search_index("badpat", &[("a.txt", "x\n")]);
        assert!(search(&index, "[", false).is_err());
    }

    #[test]
    fn search_mixed_case_literal_prunes_and_matches() {
        let index = search_index(
            "mixed",
            &[
                ("a.txt", "alpha beta gamma\n"),
                ("b.txt", "needle_token_UNIQUE_1 sits here\n"),
                ("c.txt", "omega\n"),
            ],
        );
        let matches = search(&index, "needle_token_UNIQUE_1", false).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0].path.ends_with("b.txt"),
            "path: {}",
            matches[0].path
        );

        let plan = decompose("needle_token_UNIQUE_1", true);
        let cands = candidates(&index, &plan).expect("mixed-case literal must prune");
        assert_eq!(
            cands.len(),
            1,
            "mixed-case literal must not scan the whole tree"
        );
    }

    #[test]
    fn search_without_literals_scans_all() {
        let index = search_index("dot", &[("a.txt", "abc\n"), ("b.txt", "def\n")]);
        let matches = search(&index, ".", false).unwrap();
        assert_eq!(
            matches.len(),
            2,
            "no-literal pattern must consider all files"
        );
    }
}
