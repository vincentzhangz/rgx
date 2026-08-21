//! Recursive file scanner using the `ignore` crate (ripgrep ecosystem).
//!
//! Behavior follows `rg` conventions: respects root and nested `.gitignore` files,
//! `.rgxignore` files, global gitignore, skips hidden files/directories (names
//! starting with `.`), skips non-followed symlinks, and skips files detected as
//! binary (by extension or a null byte in the first 512 bytes).

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::path::{Path, PathBuf};

/// Default ignore rules for directories, matching instantgrep's defaults.
const DEFAULT_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "_build",
    "deps",
    ".elixir_ls",
    ".idea",
    ".vscode",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
];

/// Extensions treated as binary (never indexed).
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "woff", "woff2", "ttf", "eot", "mp3", "mp4",
    "avi", "mov", "pdf", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib",
    "o", "a", "beam", "class", "jar", "war", "pyc", "pyo", "DS_Store", "lock", "node", "wasm",
    "bin", "obj", "db", "sqlite", "sqlite3", "log",
];

/// Default maximum indexed file size (bytes).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1 << 20;

/// Tuning knobs for [`scan`].
pub struct ScanOptions {
    /// Files larger than this many bytes are skipped.
    pub max_file_size: u64,
    /// When `true`, symbolic links to files and directories are followed
    /// (directory cycles are detected and pruned).
    pub follow_symlinks: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            follow_symlinks: false,
        }
    }
}

/// Walk `root` and return the list of indexable file paths, sorted.
pub fn scan(root: &Path, opts: &ScanOptions) -> Vec<PathBuf> {
    let root = display_root(root);

    let mut ov_builder = OverrideBuilder::new(&root);
    for dir in DEFAULT_IGNORED_DIRS {
        let _ = ov_builder.add(&format!("!{dir}"));
        let _ = ov_builder.add(&format!("!{dir}/**"));
        let _ = ov_builder.add(&format!("!**/{dir}"));
        let _ = ov_builder.add(&format!("!**/{dir}/**"));
    }
    let overrides = ov_builder
        .build()
        .unwrap_or(ignore::overrides::Override::empty());

    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(true)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(opts.follow_symlinks)
        .max_filesize(Some(opts.max_file_size))
        .overrides(overrides)
        .add_custom_ignore_filename(".rgxignore");

    let mut files = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !(file_type.is_file() || (opts.follow_symlinks && file_type.is_symlink())) {
            continue;
        }
        // Reuse the walker's stat result; no extra syscall per file.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() == 0 || meta.len() > opts.max_file_size {
            continue;
        }
        if is_hidden(path) {
            continue;
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && BINARY_EXTENSIONS
                .iter()
                .any(|&b| ext.eq_ignore_ascii_case(b))
        {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    files
}

/// Canonicalize `root`, stripping the `\\?\` verbatim prefix that
/// `std::fs::canonicalize` prepends to paths on Windows. The prefix is noise
/// in user-facing output and confuses tooling that splits on `:` (drive
/// letters) or parses paths, so returned paths use the conventional `C:\`
/// (or `\\` UNC) form. A no-op on other platforms.
pub fn display_root(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    #[cfg(windows)]
    {
        let s = canon.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return match rest.strip_prefix("UNC\\") {
                Some(unc) => PathBuf::from(format!(r"\\{unc}")),
                None => PathBuf::from(rest),
            };
        }
    }
    canon
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// True when `content` looks binary: a NUL byte within the first 512 bytes.
/// Callers that read file content anyway (indexing, brute-force matching)
/// apply this after reading, so the scanner never opens files twice.
pub fn is_binary_content(content: &[u8]) -> bool {
    let head = &content[..content.len().min(512)];
    head.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::remove_dir_all_retry;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rgx-scan-test-{name}-{}", std::process::id()));
        remove_dir_all_retry(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn skips_hidden_binary_and_ignored() {
        let root = tmpdir("policy");
        fs::write(root.join("a.txt"), "hello").unwrap();
        fs::write(root.join(".hidden.txt"), "hello").unwrap();
        fs::write(root.join("blob.png"), "hello").unwrap();
        fs::write(root.join("nul.txt"), [1, 2, 0, 3]).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "hello").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/dep.js"), "hello").unwrap();

        // Content-based binary detection (NUL bytes) happens when the file
        // is read for indexing, not during the walk; extension-based
        // filtering still happens here.
        let files = scan(&root, &ScanOptions::default());
        let canon_root = display_root(&root);
        assert_eq!(
            files,
            vec![canon_root.join("a.txt"), canon_root.join("nul.txt")]
        );
        assert!(is_binary_content(&[1, 2, 0, 3]));
        assert!(!is_binary_content(b"plain text"));
    }

    #[test]
    fn follows_symlinks_when_enabled() {
        let root = tmpdir("symlink");
        fs::write(root.join("real.txt"), "hello symlink").unwrap();
        let link = root.join("link.txt");
        let _ = fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real.txt"), &link).unwrap();

        let default = scan(&root, &ScanOptions::default());
        assert!(
            !default.iter().any(|p| p.ends_with("link.txt")),
            "symlink followed by default"
        );

        let follow = scan(
            &root,
            &ScanOptions {
                follow_symlinks: true,
                ..Default::default()
            },
        );
        assert!(follow.iter().any(|p| p.ends_with("real.txt")));
    }

    #[test]
    fn gitignore_respected() {
        let root = tmpdir("gitignore");
        fs::write(root.join(".gitignore"), "ignored.txt\nskipme/\n").unwrap();
        fs::write(root.join("ignored.txt"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();
        fs::create_dir_all(root.join("skipme")).unwrap();
        fs::write(root.join("skipme/inner.txt"), "x").unwrap();

        let files = scan(&root, &ScanOptions::default());
        assert!(files.iter().any(|p| p.ends_with("keep.txt")));
        assert!(!files.iter().any(|p| p.ends_with("ignored.txt")));
        assert!(!files.iter().any(|p| p.ends_with("inner.txt")));
    }

    #[test]
    fn nested_gitignore_and_negations() {
        let root = tmpdir("nested-git");
        fs::write(root.join(".gitignore"), "*.bak\n!important.bak\n").unwrap();
        fs::write(root.join("test.bak"), "drop").unwrap();
        fs::write(root.join("important.bak"), "keep").unwrap();

        fs::create_dir_all(root.join("subpkg")).unwrap();
        fs::write(root.join("subpkg/.gitignore"), "sub_ignored.txt\n").unwrap();
        fs::write(root.join("subpkg/sub_ignored.txt"), "drop").unwrap();
        fs::write(root.join("subpkg/sub_keep.txt"), "keep").unwrap();

        let files = scan(&root, &ScanOptions::default());
        assert!(
            files.iter().any(|p| p.ends_with("important.bak")),
            "negated pattern must be kept"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("test.bak")),
            "bak must be ignored"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("sub_ignored.txt")),
            "nested ignore must be respected"
        );
        assert!(
            files.iter().any(|p| p.ends_with("sub_keep.txt")),
            "nested keep must be included"
        );
    }

    #[test]
    fn rgxignore_respected() {
        let root = tmpdir("rgxignore");
        fs::write(root.join(".rgxignore"), "custom_skip.txt\n").unwrap();
        fs::write(root.join("custom_skip.txt"), "drop").unwrap();
        fs::write(root.join("keep.txt"), "keep").unwrap();

        let files = scan(&root, &ScanOptions::default());
        assert!(files.iter().any(|p| p.ends_with("keep.txt")));
        assert!(!files.iter().any(|p| p.ends_with("custom_skip.txt")));
    }
}
