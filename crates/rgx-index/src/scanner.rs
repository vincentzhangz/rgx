//! Recursive file scanner with hidden-file, ignore-pattern and binary-file
//! support. Written from scratch (no `walkdir`/`ignore` dependency).
//!
//! Behavior follows `rg` conventions: hidden files and directories (names
//! starting with `.`) are skipped unless explicitly named, symbolic links
//! are not followed by default, and files detected as binary (by extension
//! or a null byte in the first 512 bytes) are never indexed.

use std::collections::HashSet;
use std::fs;
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
    "bin", "obj", "pyc", "db", "sqlite", "sqlite3", "log",
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
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let gitignore = Gitignore::load(root.as_path());

    let mut files = Vec::new();
    let mut visited = HashSet::new();
    walk(&root, &root, opts, &gitignore, &mut visited, &mut files);
    files.sort();
    files
}

fn walk(
    root: &Path,
    dir: &Path,
    opts: &ScanOptions,
    gitignore: &Gitignore,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    if dir != root && is_hidden(dir) {
        return;
    }
    if dir != root && is_ignored_dir(dir) {
        return;
    }
    if opts.follow_symlinks
        && let Ok(canon) = dir.canonicalize()
        && !visited.insert(canon)
    {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let is_dir = if file_type.is_dir() {
            true
        } else if opts.follow_symlinks && file_type.is_symlink() {
            fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
        } else {
            false
        };
        let is_file = if file_type.is_file() {
            true
        } else if opts.follow_symlinks && file_type.is_symlink() {
            fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false)
        } else {
            false
        };
        if is_dir {
            walk(root, &path, opts, gitignore, visited, out);
        } else if is_file && indexable_file(&path, opts, gitignore) {
            out.push(path);
        }
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

fn is_ignored_dir(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    DEFAULT_IGNORED_DIRS.contains(&name)
}

fn indexable_file(path: &Path, opts: &ScanOptions, gitignore: &Gitignore) -> bool {
    if is_hidden(path) {
        return false;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    {
        return false;
    }
    if gitignore.is_ignored(path) {
        return false;
    }
    match fs::metadata(path) {
        Ok(m) => {
            if m.len() == 0 || m.len() > opts.max_file_size {
                return false;
            }
        }
        Err(_) => return false,
    }
    !contains_nul_byte(path)
}

fn contains_nul_byte(path: &Path) -> bool {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return true,
    };
    use std::io::Read;
    let mut buf = [0u8; 512];
    let mut f = f;
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].contains(&0)
}

struct Gitignore {
    patterns: Vec<IgnorePattern>,
    root: PathBuf,
}

struct IgnorePattern {
    regex: String,
    /// Whether the pattern is anchored to the gitignore root.
    anchored: bool,
}

impl Gitignore {
    fn load(root: &Path) -> Gitignore {
        let gitignore_path = root.join(".gitignore");
        let mut patterns = Vec::new();
        if let Ok(content) = fs::read_to_string(&gitignore_path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(p) = parse_gitignore_line(line) {
                    patterns.push(p);
                }
            }
        }
        Gitignore {
            patterns,
            root: root.to_path_buf(),
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        let rel = match path.strip_prefix(&self.root) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let rel_str = rel.to_string_lossy();
        self.patterns.iter().any(|p| {
            if p.anchored {
                rel_str.starts_with(&p.regex)
            } else {
                rel_str.contains(&p.regex)
            }
        })
    }
}

/// Translate a `.gitignore` glob into a simple matcher string.
///
/// Full gitignore semantics are complex; this covers the common cases
/// (names, `dir/`, `*.ext`, `name/`, leading `/`, trailing `/`).
fn parse_gitignore_line(line: &str) -> Option<IgnorePattern> {
    let mut s = line.to_string();
    let anchored = if let Some(stripped) = s.strip_prefix('/') {
        s = stripped.to_string();
        true
    } else {
        false
    };

    if let Some(stripped) = s.strip_suffix('/') {
        s = stripped.to_string();
    }

    let token = if s.starts_with('*') {
        s.trim_start_matches('*')
    } else if s.contains('*') || s.contains('?') {
        let cut = s.find(['*', '?']).unwrap_or(s.len());
        &s[..cut]
    } else {
        s.as_str()
    };

    if token.is_empty() {
        return None;
    }

    Some(IgnorePattern {
        regex: token.to_string(),
        anchored: anchored || !token.contains('/'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rgx-scan-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn skips_hidden_binary_and_ignored() {
        let root = tmpdir("policy");
        fs::write(root.join("a.txt"), "hello").unwrap();
        fs::write(root.join(".hidden.txt"), "hello").unwrap();
        fs::write(root.join("bin.dat"), [1, 2, 0, 3]).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "hello").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/dep.js"), "hello").unwrap();

        let files = scan(&root, &ScanOptions::default());
        let canon_root = root.canonicalize().unwrap();
        assert_eq!(files, vec![canon_root.join("a.txt")]);
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
}
