//! Line-by-line regex matching for rgx.

use regex::bytes::Regex;
use std::sync::Arc;

/// One matching line in one file.
#[derive(Clone, Debug)]
pub struct Match {
    /// Absolute path of the file containing the match. Shared across all
    /// matching lines of the file (one allocation per file, not per line).
    pub path: Arc<str>,
    /// 1-based line number within the file.
    pub line: u32,
    /// The matching line's text (with its trailing newline removed).
    pub text: String,
    /// Byte offsets of matches within `text` (only populated in JSON mode).
    pub submatches: Vec<(usize, usize)>,
}

/// Scan `content` line by line, appending every matching line to `out`.
///
/// Path is lazily wrapped in an `Arc<str>` on the first match in `content`,
/// avoiding any heap allocation when the file does not match. Line numbers
/// are 1-based. In JSON mode submatch byte offsets are recorded; otherwise
/// only a boolean per line is tested (faster).
pub fn match_content_str(re: &Regex, content: &[u8], path: &str, json: bool, out: &mut Vec<Match>) {
    if !re.is_match(content) {
        return;
    }
    let mut path_arc: Option<Arc<str>> = None;
    for (line_no, line) in (1_u32..).zip(content.split(|&b| b == b'\n')) {
        let subs = if json {
            let v: Vec<(usize, usize)> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
            if v.is_empty() {
                continue;
            }
            v
        } else {
            if !re.is_match(line) {
                continue;
            }
            Vec::new()
        };
        let p = match &path_arc {
            Some(arc) => Arc::clone(arc),
            None => {
                let arc: Arc<str> = Arc::from(path);
                path_arc = Some(Arc::clone(&arc));
                arc
            }
        };
        out.push(Match {
            path: p,
            line: line_no,
            text: String::from_utf8_lossy(line).into_owned(),
            submatches: subs,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    #[test]
    fn matches_lines_and_numbers() {
        let mut out = Vec::new();
        match_content_str(
            &re("hello"),
            b"hello world\nnothing here\noh hello again\n",
            "f.txt",
            false,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line, 1);
        assert_eq!(out[0].text, "hello world");
        assert_eq!(out[1].line, 3);
        assert_eq!(out[1].text, "oh hello again");
    }

    #[test]
    fn submatches_record_offsets() {
        let mut out = Vec::new();
        match_content_str(&re("el+"), b"hello\n", "f.txt", true, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].submatches, vec![(1, 4)]);
    }

    #[test]
    fn empty_content_no_matches() {
        let mut out = Vec::new();
        match_content_str(&re("x"), b"", "f.txt", false, &mut out);
        assert!(out.is_empty());
    }
}
