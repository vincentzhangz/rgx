//! On-disk inverted index: build, save, load (mmap) and query.
//!
//! Layout inside `<root>/.rgx/` (all little-endian):
//!
//! * `lookup.dat`  — `"RGXLOOK1"` + `u64 count` + sorted entries of
//!   `(u64 ngram_hash, u64 postings_offset)`, 16 bytes each. Mmap'd and
//!   binary-searched. Offset is absolute into `postings.dat`.
//! * `postings.dat`— `"RGXPOST1"` + `u64 count` + `u64 total_ids` + posting
//!   lists. Each list is `u32 id_count` followed by `id_count` sorted `u32`
//!   file ids. Mmap'd; lists are located by lookup offsets.
//! * `files.dat`   — `"RGXFILS1"` + `u64 count` + `u64 len` + path bytes,
//!   repeated. File id = sequence index (0-based).
//! * `meta.dat`    — `"RGXMETA1"` + `u64 count` + `u64 plen` + path +
//!   `u64 mtime` + `u64 size`, repeated. Same order as `files.dat`; used for
//!   change detection.
//! * `grams.dat`   — `"RGXGRAM1"` + `u64 count` + per-file records of
//!   `(plen, path, mtime, size, content_hash u128, gram_count, grams...)`.
//!   A per-file n-gram cache keyed by `(mtime, size, content_hash)`; this is
//!   what makes `--update` incremental (unchanged files are never re-read).
//!   Corrupt or missing `grams.dat` simply triggers a full rebuild.
//!
//! The keyed n-gram hash is `ngram_hash` (FNV-1a over the gram bytes).
//! Hash collisions can only broaden a posting list, never cause a false
//! negative, so no collision handling is required.
//!
//! Builds are safe against Windows antivirus locks: a first build writes the
//! tables directly into `.rgx/` (no rename), while a rebuild stages the new
//! tables in `.rgx.tmp/` and atomically swaps them in (retrying transient
//! `ERROR_ACCESS_DENIED` renames with backoff).

use crate::fingerprint::content_hash;
use crate::ngram::{MIN_NGRAM_LENGTH, build_all_ngrams, ngram_hash};
use crate::scanner::{ScanOptions, scan};
use memmap2::Mmap;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const POSTINGS_HEADER: &[u8] = b"RGXPOST1";
const LOOKUP_HEADER: &[u8] = b"RGXLOOK1";
const FILES_HEADER: &[u8] = b"RGXFILS1";
const META_HEADER: &[u8] = b"RGXMETA1";
const GRAMS_HEADER: &[u8] = b"RGXGRAM1";

/// Header layout: 8 magic bytes + 8 count bytes = 16.
const HEADER_LEN: usize = 16;

/// The count field within the 16-byte header.
const COUNT_OFFSET: usize = 8;

/// A `(hash, offset)` lookup entry: 16 bytes.
const ENTRY_LEN: usize = 16;

/// postings.dat has a third header field (`total_ids`), so its payload
/// starts 8 bytes later than the generic 16-byte header.
const POSTINGS_DATA_OFFSET: u64 = (HEADER_LEN + 8) as u64;

/// A file known to the index.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Last-modified time in whole seconds since the Unix epoch.
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
}

/// A cached per-file n-gram record in `grams.dat`.
#[derive(Clone, Debug)]
pub struct GramRecord {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Last-modified time in whole seconds since the Unix epoch.
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
    /// Content fingerprint; unchanged files reuse their cached grams.
    pub content_hash: u128,
    /// Distinct n-gram hashes of the file (length >= `MIN_NGRAM_LENGTH`).
    pub grams: Vec<u64>,
}

/// Statistics from an index build, printed by `--stats`.
#[derive(Default, Debug, Clone)]
pub struct BuildStats {
    /// Number of files in the index.
    pub files: usize,
    /// Total bytes read from disk during the build.
    pub bytes_read: u64,
    /// Distinct n-grams in the lookup table.
    pub ngrams: usize,
    /// Total posting entries.
    pub postings: u64,
    /// Total on-disk size of all index files.
    pub index_bytes: u64,
    /// Wall-clock time spent building.
    pub elapsed: std::time::Duration,
    /// Files reused from the previous `grams.dat` (not re-read) in an
    /// incremental update.
    pub reused_files: usize,
}

/// Build an index for the tree at `root` into `index_dir` (usually
/// `root/.rgx`). `progress` is invoked with human-readable status lines.
pub fn build_index(
    root: &Path,
    index_dir: &Path,
    opts: &ScanOptions,
    progress: &mut dyn FnMut(&str),
) -> std::io::Result<BuildStats> {
    let started = std::time::Instant::now();
    let files = scan(root, opts);
    let n = files.len();
    progress(&format!("scanning: {} files", n));

    let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut entries: Vec<FileEntry> = Vec::with_capacity(n);
    let mut grams: Vec<GramRecord> = Vec::with_capacity(n);
    let mut total_postings: u64 = 0;
    let mut bytes_read: u64 = 0;

    for (id, path) in files.iter().enumerate() {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = mtime_secs(&meta);
        let size = meta.len();
        let content = match fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        bytes_read += content.len() as u64;

        let record = GramRecord {
            path: path.clone(),
            mtime,
            size,
            content_hash: content_hash(&content),
            grams: file_grams(&content),
        };
        accumulate(&mut postings, &mut total_postings, id as u32, &record.grams);
        entries.push(FileEntry {
            path: path.clone(),
            mtime,
            size,
        });
        grams.push(record);
    }

    let index_bytes = write_index_files(index_dir, &postings, &entries, &grams, progress)?;

    Ok(BuildStats {
        files: entries.len(),
        bytes_read,
        ngrams: postings.len(),
        postings: total_postings,
        index_bytes,
        elapsed: started.elapsed(),
        reused_files: 0,
    })
}

/// Incrementally update an existing index in `index_dir`.
///
/// Only files whose `(mtime, size)` changed are re-read. Among those, files
/// whose content fingerprint still matches the cached one (metadata noise
/// from `touch`/`git checkout`) reuse their cached n-grams. Removed files are
/// dropped. A missing or corrupt `grams.dat` falls back to a full rebuild.
pub fn update_index(
    root: &Path,
    index_dir: &Path,
    opts: &ScanOptions,
    progress: &mut dyn FnMut(&str),
) -> std::io::Result<BuildStats> {
    let started = std::time::Instant::now();
    let files = scan(root, opts);
    let n = files.len();
    progress(&format!("scanning: {} files", n));

    let old = match load_grams_cache(&index_dir.join("grams.dat")) {
        Some(c) => c,
        None => {
            progress("no usable n-gram cache; full rebuild");
            return build_index(root, index_dir, opts, progress);
        }
    };
    let mut old_by_path: HashMap<PathBuf, GramRecord> = HashMap::with_capacity(old.len());
    for g in old {
        old_by_path.insert(g.path.clone(), g);
    }

    let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut entries: Vec<FileEntry> = Vec::with_capacity(n);
    let mut grams: Vec<GramRecord> = Vec::with_capacity(n);
    let mut total_postings: u64 = 0;
    let mut bytes_read: u64 = 0;
    let mut reused: usize = 0;

    for (id, path) in files.iter().enumerate() {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = mtime_secs(&meta);
        let size = meta.len();

        let cached = old_by_path.remove(path);
        let mut record = match cached {
            Some(c) if c.mtime == mtime && c.size == size => {
                reused += 1;
                c
            }
            Some(c) => {
                let content = match fs::read(path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                bytes_read += content.len() as u64;
                let hash = content_hash(&content);
                if hash == c.content_hash {
                    reused += 1;
                    c
                } else {
                    GramRecord {
                        path: path.clone(),
                        mtime,
                        size,
                        content_hash: hash,
                        grams: file_grams(&content),
                    }
                }
            }
            None => {
                let content = match fs::read(path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                bytes_read += content.len() as u64;
                GramRecord {
                    path: path.clone(),
                    mtime,
                    size,
                    content_hash: content_hash(&content),
                    grams: file_grams(&content),
                }
            }
        };
        record.mtime = mtime;
        record.size = size;

        accumulate(&mut postings, &mut total_postings, id as u32, &record.grams);
        entries.push(FileEntry {
            path: path.clone(),
            mtime,
            size,
        });
        grams.push(record);
    }

    progress(&format!(
        "update: {} files reused, {} re-read",
        reused,
        files.len().saturating_sub(reused)
    ));
    let index_bytes = write_index_files(index_dir, &postings, &entries, &grams, progress)?;

    Ok(BuildStats {
        files: entries.len(),
        bytes_read,
        ngrams: postings.len(),
        postings: total_postings,
        index_bytes,
        elapsed: started.elapsed(),
        reused_files: reused,
    })
}

/// Collect the distinct n-grams of a file's content (length >= 3).
fn file_grams(content: &[u8]) -> Vec<u64> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut ids = Vec::new();
    build_all_ngrams(content, &mut |gram| {
        if gram.len() < MIN_NGRAM_LENGTH {
            return;
        }
        let h = ngram_hash(gram);
        if seen.insert(h) {
            ids.push(h);
        }
    });
    ids
}

/// Add a file's n-grams to the global postings map (postings stay sorted
/// because files are processed in ascending id order).
fn accumulate(
    postings: &mut HashMap<u64, Vec<u32>>,
    total_postings: &mut u64,
    id: u32,
    grams: &[u64],
) {
    for &h in grams {
        match postings.get_mut(&h) {
            Some(list) => list.push(id),
            None => {
                postings.insert(h, vec![id]);
            }
        }
        *total_postings += 1;
    }
}

/// Write the five index files for `index_dir`.
///
/// A first build (`index_dir` absent) writes the tables directly into place,
/// avoiding the directory rename that Windows antivirus can block. Rebuilds
/// stage the new tables in `<index_dir>.tmp` and atomically swap them in via
/// [`publish_index`]. Returns the total on-disk index size.
fn write_index_files(
    index_dir: &Path,
    postings: &HashMap<u64, Vec<u32>>,
    entries: &[FileEntry],
    grams: &[GramRecord],
    progress: &mut dyn FnMut(&str),
) -> std::io::Result<u64> {
    progress("writing index files");

    if !index_dir.exists() {
        fs::create_dir_all(index_dir)?;
        return write_tables(index_dir, postings, entries, grams);
    }

    let staging = PathBuf::from(format!("{}.tmp", index_dir.display()));
    remove_dir_all_retry(&staging);
    fs::create_dir_all(&staging)?;
    let index_bytes = write_tables(&staging, postings, entries, grams)?;
    publish_index(&staging, index_dir)?;
    Ok(index_bytes)
}

/// Write the five index data tables into `dir`. Returns the total on-disk
/// index size.
fn write_tables(
    dir: &Path,
    postings: &HashMap<u64, Vec<u32>>,
    entries: &[FileEntry],
    grams: &[GramRecord],
) -> std::io::Result<u64> {
    let mut keys: Vec<u64> = postings.keys().copied().collect();
    keys.sort_unstable();

    let mut pfile = File::create(dir.join("postings.dat"))?;
    pfile.write_all(POSTINGS_HEADER)?;
    pfile.write_all(&(keys.len() as u64).to_le_bytes())?;
    let total_ids: u64 = postings.values().map(|v| v.len() as u64).sum();
    pfile.write_all(&total_ids.to_le_bytes())?;
    for k in &keys {
        let list = &postings[k];
        pfile.write_all(&(list.len() as u32).to_le_bytes())?;
        for id in list {
            pfile.write_all(&id.to_le_bytes())?;
        }
    }
    let postings_bytes = pfile.metadata()?.len();
    drop(pfile);

    let mut lfile = File::create(dir.join("lookup.dat"))?;
    lfile.write_all(LOOKUP_HEADER)?;
    lfile.write_all(&(keys.len() as u64).to_le_bytes())?;
    let mut offset: u64 = POSTINGS_DATA_OFFSET;
    for k in &keys {
        let list = &postings[k];
        lfile.write_all(&k.to_le_bytes())?;
        lfile.write_all(&offset.to_le_bytes())?;
        offset += 4 + list.len() as u64 * 4;
    }
    let lookup_bytes = lfile.metadata()?.len();
    drop(lfile);

    write_path_table(dir.join("files.dat"), FILES_HEADER, entries)?;
    write_meta_table(dir.join("meta.dat"), entries)?;
    write_grams_table(dir.join("grams.dat"), grams)?;

    Ok(postings_bytes
        + lookup_bytes
        + fs::metadata(dir.join("files.dat"))?.len()
        + fs::metadata(dir.join("meta.dat"))?.len()
        + fs::metadata(dir.join("grams.dat"))?.len())
}

fn write_path_table(path: PathBuf, magic: &[u8], entries: &[FileEntry]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(magic)?;
    f.write_all(&(entries.len() as u64).to_le_bytes())?;
    for e in entries {
        let p = e.path.to_string_lossy();
        f.write_all(&(p.len() as u64).to_le_bytes())?;
        f.write_all(p.as_bytes())?;
    }
    Ok(())
}

fn write_meta_table(path: PathBuf, entries: &[FileEntry]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(META_HEADER)?;
    f.write_all(&(entries.len() as u64).to_le_bytes())?;
    for e in entries {
        let p = e.path.to_string_lossy();
        f.write_all(&(p.len() as u64).to_le_bytes())?;
        f.write_all(p.as_bytes())?;
        f.write_all(&e.mtime.to_le_bytes())?;
        f.write_all(&e.size.to_le_bytes())?;
    }
    Ok(())
}

fn write_grams_table(path: PathBuf, grams: &[GramRecord]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(GRAMS_HEADER)?;
    f.write_all(&(grams.len() as u64).to_le_bytes())?;
    for g in grams {
        let p = g.path.to_string_lossy();
        f.write_all(&(p.len() as u64).to_le_bytes())?;
        f.write_all(p.as_bytes())?;
        f.write_all(&g.mtime.to_le_bytes())?;
        f.write_all(&g.size.to_le_bytes())?;
        f.write_all(&g.content_hash.to_le_bytes())?;
        f.write_all(&(g.grams.len() as u64).to_le_bytes())?;
        for h in &g.grams {
            f.write_all(&h.to_le_bytes())?;
        }
    }
    Ok(())
}

/// Load the per-file n-gram cache. Returns `None` when missing or corrupt
/// (the caller falls back to a full rebuild).
fn load_grams_cache(path: &Path) -> Option<Vec<GramRecord>> {
    let data = fs::read(path).ok()?;
    if data.len() < HEADER_LEN || &data[..8] != GRAMS_HEADER {
        return None;
    }
    let count = read_u64(&data, COUNT_OFFSET).ok()? as usize;
    let mut out = Vec::with_capacity(count);
    let mut p = HEADER_LEN;
    for _ in 0..count {
        let plen = read_u64(&data, p).ok()? as usize;
        p += 8;
        let end = p.checked_add(plen)?;
        let path = String::from_utf8_lossy(data.get(p..end)?).into_owned();
        p = end;
        let mtime = read_u64(&data, p).ok()?;
        let size = read_u64(&data, p + 8).ok()?;
        let content_hash = read_u128(&data, p + 16).ok()?;
        let gcount = read_u64(&data, p + 32).ok()? as usize;
        p += 40;
        let mut grams = Vec::with_capacity(gcount);
        for _ in 0..gcount {
            grams.push(read_u64(&data, p).ok()?);
            p += 8;
        }
        out.push(GramRecord {
            path: PathBuf::from(path),
            mtime,
            size,
            content_hash,
            grams,
        });
    }
    Some(out)
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomically swap the freshly built `staging` directory into place as
/// `index_dir`. A crash at any point leaves either the previous index
/// intact or no index at all (which is rebuilt on the next run) — never a
/// partially written index. The old index is moved to `<index_dir>.old`
/// first so a failed swap can be rolled back.
///
/// This is robust on Windows, where renames and deletes under `%TEMP%` can
/// fail with `ERROR_ACCESS_DENIED` while antivirus holds a freshly written
/// file open (notably on CI runners) and where a directory cannot be renamed
/// over an existing directory (`MoveFileExW` limitation); transient failures
/// are retried with a long backoff, since the lock can persist for seconds.
/// On Unix the swap is POSIX-atomic, so a single attempt suffices.
fn publish_index(staging: &Path, index_dir: &Path) -> std::io::Result<()> {
    let stale = PathBuf::from(format!("{}.old", index_dir.display()));
    for attempt in 0..PUBLISH_ATTEMPTS {
        remove_dir_all_retry(&stale);
        if index_dir.exists() {
            match fs::rename(index_dir, &stale) {
                Ok(()) => {}
                Err(e) if retryable(&e) && attempt + 1 < PUBLISH_ATTEMPTS => {
                    publish_sleep(attempt);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        match fs::rename(staging, index_dir) {
            Ok(()) => {
                remove_dir_all_retry(&stale);
                return Ok(());
            }
            Err(e) if retryable(&e) && attempt + 1 < PUBLISH_ATTEMPTS => {
                let _ = fs::rename(&stale, index_dir);
                publish_sleep(attempt);
            }
            Err(e) => {
                let _ = fs::rename(&stale, index_dir);
                return Err(e);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "could not publish index directory",
    ))
}

/// Number of attempts to atomically publish a rebuilt index directory.
/// Windows retries because antivirus can hold a freshly written index open
/// for seconds; Unix needs no retry.
#[cfg(windows)]
const PUBLISH_ATTEMPTS: u32 = 6;
#[cfg(not(windows))]
const PUBLISH_ATTEMPTS: u32 = 1;

/// Number of attempts for a filesystem operation that may fail transiently.
const RETRY_ATTEMPTS: u32 = 5;

/// True for errors that may be transient on Windows (a file lock held by
/// antivirus/indexing or a momentarily open handle), so a retry can succeed.
#[cfg(windows)]
fn retryable(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            5 /* ERROR_ACCESS_DENIED */
                | 32 /* ERROR_SHARING_VIOLATION */
                | 33 /* ERROR_LOCK_VIOLATION */
        )
    )
}

#[cfg(not(windows))]
fn retryable(_e: &std::io::Error) -> bool {
    false
}

/// Backoff between index-publish retries: 1s, 2s, 4s, 8s, ...
#[cfg(windows)]
fn publish_sleep(attempt: u32) {
    let ms = 1000 << attempt.min(3);
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

#[cfg(not(windows))]
fn publish_sleep(_attempt: u32) {}

/// Short backoff between cleanup retries (50ms, 100ms, 200ms, ...).
fn transient_sleep(attempt: u32) {
    let ms = 50 * (1 << attempt.min(3));
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Best-effort recursive delete that retries on transient Windows errors.
/// Missing paths count as success (idempotent like `rm -rf`).
pub(crate) fn remove_dir_all_retry(path: &Path) {
    for attempt in 0..RETRY_ATTEMPTS {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if retryable(&e) && attempt + 1 < RETRY_ATTEMPTS => transient_sleep(1),
            Err(_) => return,
        }
    }
}

/// A loaded, mmap'd index.
pub struct Index {
    lookup: Mmap,
    postings: Mmap,
    files: Vec<PathBuf>,
    meta: Vec<(PathBuf, u64, u64)>,
}

impl Index {
    /// Load an index previously written by `build_index`.
    ///
    /// Validates the structural integrity of every file and returns an error
    /// (never panics) if any of them is truncated or corrupted.
    pub fn open(index_dir: &Path) -> std::io::Result<Index> {
        let lf = File::open(index_dir.join("lookup.dat"))?;
        let pf = File::open(index_dir.join("postings.dat"))?;
        let ff = File::open(index_dir.join("files.dat"))?;
        let mf = File::open(index_dir.join("meta.dat"))?;

        let lookup = unsafe { Mmap::map(&lf)? };
        let postings = unsafe { Mmap::map(&pf)? };
        check_header(&lookup, LOOKUP_HEADER)?;
        check_header(&postings, POSTINGS_HEADER)?;

        let lookup_count = read_u64(&lookup, COUNT_OFFSET)? as usize;
        if lookup.len() < HEADER_LEN + lookup_count * ENTRY_LEN {
            return Err(corrupt("lookup table truncated"));
        }

        for i in 0..lookup_count {
            let at = HEADER_LEN + i * ENTRY_LEN;
            let off = read_u64(&lookup, at + 8)? as usize;
            if off < POSTINGS_DATA_OFFSET as usize || off + 4 > postings.len() {
                return Err(corrupt("posting list offset out of range"));
            }
            let list_len = read_u32(&postings, off)? as usize;
            if off + 4 + list_len * 4 > postings.len() {
                return Err(corrupt("posting list truncated"));
            }
        }

        let files = read_path_table(&ff, FILES_HEADER)?;
        let meta = read_meta_table(&mf)?;

        Ok(Index {
            lookup,
            postings,
            files,
            meta,
        })
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Distinct n-grams in the lookup table.
    pub fn ngram_count(&self) -> u64 {
        u64_at(&self.lookup, COUNT_OFFSET).unwrap_or(0)
    }

    /// Total posting entries.
    pub fn posting_count(&self) -> u64 {
        u64_at(&self.postings, COUNT_OFFSET + 8).unwrap_or(0)
    }

    /// Absolute path of a file id.
    pub fn file_path(&self, id: u32) -> Option<&Path> {
        self.files.get(id as usize).map(|p| p.as_path())
    }

    /// Stored (mtime, size) for a file id, used for change detection.
    pub fn file_meta(&self, id: u32) -> Option<(PathBuf, u64, u64)> {
        self.meta.get(id as usize).cloned()
    }

    /// The posting list for an n-gram hash (empty when absent).
    ///
    /// Safe against malformed files: out-of-range data yields an empty list.
    pub fn postings(&self, hash: u64) -> Vec<u32> {
        let Some(offset) = lookup_offset(&self.lookup, hash) else {
            return Vec::new();
        };
        let offset = offset as usize;
        let Some(list_len) = u32_at(&self.postings, offset) else {
            return Vec::new();
        };
        let list_len = list_len as usize;
        let ids_start = offset + 4;
        let mut out = Vec::with_capacity(list_len);
        for i in 0..list_len {
            let s = ids_start + i * 4;
            match u32_at(&self.postings, s) {
                Some(id) => out.push(id),
                None => break,
            }
        }
        out
    }

    /// True when `hash` exists in the lookup table.
    pub fn has_ngram(&self, hash: u64) -> bool {
        lookup_offset(&self.lookup, hash).is_some()
    }
}

/// Binary search the sorted lookup table for `hash`, returning its
/// absolute `postings.dat` offset when present.
fn lookup_offset(lookup: &Mmap, hash: u64) -> Option<u64> {
    let count = u64_at(lookup, COUNT_OFFSET)? as usize;
    let base = HEADER_LEN;
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let at = base + mid * ENTRY_LEN;
        let h = u64_at(lookup, at)?;
        if h < hash {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo < count {
        let at = base + lo * ENTRY_LEN;
        let h = u64_at(lookup, at)?;
        if h == hash {
            return u64_at(lookup, at + 8);
        }
    }
    None
}

fn check_header(data: &Mmap, magic: &[u8]) -> std::io::Result<()> {
    if data.len() < HEADER_LEN || &data[..8] != magic {
        return Err(corrupt("bad magic"));
    }
    Ok(())
}

fn check_header_bytes(data: &[u8], magic: &[u8]) -> std::io::Result<()> {
    if data.len() < HEADER_LEN || &data[..8] != magic {
        return Err(corrupt("bad magic"));
    }
    Ok(())
}

fn corrupt(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

fn read_u64(data: &[u8], at: usize) -> std::io::Result<u64> {
    u64_at(data, at).ok_or_else(|| corrupt("truncated index file"))
}

fn read_u32(data: &[u8], at: usize) -> std::io::Result<u32> {
    u32_at(data, at).ok_or_else(|| corrupt("truncated index file"))
}

fn read_u128(data: &[u8], at: usize) -> std::io::Result<u128> {
    data.get(at..at + 16)
        .and_then(|s| s.try_into().ok())
        .map(u128::from_le_bytes)
        .ok_or_else(|| corrupt("truncated index file"))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
}

fn read_path_table(file: &File, magic: &[u8]) -> std::io::Result<Vec<PathBuf>> {
    let data = read_all(file)?;
    check_header_bytes(&data, magic)?;
    let count = read_u64(&data, COUNT_OFFSET)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut p = HEADER_LEN;
    for _ in 0..count {
        let plen = read_u64(&data, p)? as usize;
        p += 8;
        let end = p
            .checked_add(plen)
            .ok_or_else(|| corrupt("path length overflow"))?;
        if end > data.len() {
            return Err(corrupt("file table truncated"));
        }
        out.push(PathBuf::from(
            String::from_utf8_lossy(&data[p..end]).into_owned(),
        ));
        p = end;
    }
    Ok(out)
}

fn read_meta_table(file: &File) -> std::io::Result<Vec<(PathBuf, u64, u64)>> {
    let data = read_all(file)?;
    check_header_bytes(&data, META_HEADER)?;
    let count = read_u64(&data, COUNT_OFFSET)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut p = HEADER_LEN;
    for _ in 0..count {
        let plen = read_u64(&data, p)? as usize;
        p += 8;
        let end = p
            .checked_add(plen)
            .ok_or_else(|| corrupt("path length overflow"))?;
        if end > data.len() {
            return Err(corrupt("meta table truncated"));
        }
        let path = String::from_utf8_lossy(&data[p..end]).into_owned();
        p = end;
        let mtime = read_u64(&data, p)?;
        let size = read_u64(&data, p + 8)?;
        p += 16;
        out.push((PathBuf::from(path), mtime, size));
    }
    Ok(out)
}

fn read_all(file: &File) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    let mut f = file;
    f.read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rgx-index-test-{name}-{}", std::process::id()));
        remove_dir_all_retry(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_build_and_load() {
        let root = tmpdir("roundtrip");
        fs::write(
            root.join("a.txt"),
            "the quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        fs::write(
            root.join("b.rs"),
            "fn main() { println!(\"hello world\"); }",
        )
        .unwrap();
        fs::write(root.join("c.bin"), [0u8, 1, 2, 0, 5, 0, 0, 3]).unwrap();

        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        let stats = build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.files, 2, "binary file with nul bytes must be skipped");

        let index = Index::open(&idx_dir).unwrap();
        assert_eq!(index.file_count(), 2);

        for gram in crate::ngram::covering_ngrams(b"hello", crate::ngram::DEFAULT_MAX_NGRAM_LENGTH)
        {
            let h = ngram_hash(&gram);
            assert!(
                index.has_ngram(h),
                "missing ngram {:?}",
                String::from_utf8_lossy(&gram)
            );
            let post = index.postings(h);
            assert!(
                post.iter()
                    .any(|&id| index.file_path(id).unwrap().ends_with("b.rs"))
            );
        }

        let absent =
            crate::ngram::covering_ngrams(b"zzzqqqxxxyyy", crate::ngram::DEFAULT_MAX_NGRAM_LENGTH);
        let absent_hashes: Vec<u64> = absent.iter().map(|g| ngram_hash(g)).collect();
        for h in absent_hashes {
            assert!(!index.has_ngram(h));
        }
    }

    #[test]
    fn postings_sorted_and_complete() {
        let root = tmpdir("postings");
        for i in 0..5 {
            fs::write(
                root.join(format!("f{i}.txt")),
                format!("shared token number {i}\n"),
            )
            .unwrap();
        }
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        let index = Index::open(&idx_dir).unwrap();

        let shared = b"shared";
        for gram in crate::ngram::covering_ngrams(shared, crate::ngram::DEFAULT_MAX_NGRAM_LENGTH) {
            let post = index.postings(ngram_hash(&gram));
            assert_eq!(
                post.len(),
                5,
                "gram {:?} should appear in all 5 files",
                gram
            );
            let mut sorted = post.clone();
            sorted.sort_unstable();
            assert_eq!(post, sorted, "posting list must be sorted");
        }
    }

    #[test]
    fn empty_tree_produces_empty_index() {
        let root = tmpdir("empty");
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        let index = Index::open(&idx_dir).unwrap();
        assert_eq!(index.file_count(), 0);
        assert_eq!(index.ngram_count(), 0);
    }

    #[test]
    fn corrupt_index_is_rejected_not_panicked() {
        let root = tmpdir("corrupt");
        fs::write(
            root.join("a.txt"),
            "the quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert!(Index::open(&idx_dir).is_ok());

        let corrupt_lookup = root.join("corrupt_lookup");
        fs::create_dir_all(&corrupt_lookup).unwrap();
        fs::copy(
            idx_dir.join("postings.dat"),
            corrupt_lookup.join("postings.dat"),
        )
        .unwrap();
        fs::copy(idx_dir.join("files.dat"), corrupt_lookup.join("files.dat")).unwrap();
        fs::copy(idx_dir.join("meta.dat"), corrupt_lookup.join("meta.dat")).unwrap();
        fs::write(corrupt_lookup.join("lookup.dat"), b"not-a-lookup").unwrap();
        assert!(Index::open(&corrupt_lookup).is_err());

        let truncated = root.join("truncated");
        fs::create_dir_all(&truncated).unwrap();
        fs::copy(idx_dir.join("lookup.dat"), truncated.join("lookup.dat")).unwrap();
        fs::copy(idx_dir.join("files.dat"), truncated.join("files.dat")).unwrap();
        fs::copy(idx_dir.join("meta.dat"), truncated.join("meta.dat")).unwrap();
        let bytes = fs::read(idx_dir.join("postings.dat")).unwrap();
        fs::write(truncated.join("postings.dat"), &bytes[..bytes.len() - 4]).unwrap();
        assert!(Index::open(&truncated).is_err());
    }

    #[test]
    fn atomic_publish_leaves_no_staging_and_rebuilds_cleanly() {
        let root = tmpdir("atomic");
        fs::write(
            root.join("a.txt"),
            "the quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();

        let tmp_marker = format!("{}.tmp", idx_dir.display());
        let old_marker = format!("{}.old", idx_dir.display());
        assert!(!Path::new(&tmp_marker).exists());
        assert!(!Path::new(&old_marker).exists());
        assert!(idx_dir.join("lookup.dat").exists());

        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert!(!Path::new(&tmp_marker).exists());
        assert!(!Path::new(&old_marker).exists());
        assert!(Index::open(&idx_dir).is_ok());
    }

    fn index_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let names = [
            "lookup.dat",
            "postings.dat",
            "files.dat",
            "meta.dat",
            "grams.dat",
        ];
        names
            .iter()
            .map(|n| (n.to_string(), fs::read(dir.join(n)).unwrap()))
            .collect()
    }

    #[test]
    fn no_op_update_reuses_everything_and_matches_fresh_build() {
        let root = tmpdir("update-equiv");
        fs::write(
            root.join("a.txt"),
            "the quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        fs::write(
            root.join("b.rs"),
            "fn main() { println!(\"hello world\"); }",
        )
        .unwrap();
        fs::write(root.join("c.md"), "# heading\nsome tokens here\n").unwrap();

        let fresh = root.join("fresh");
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &fresh, &ScanOptions::default(), &mut progress).unwrap();
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();

        let stats = update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.reused_files, 3, "no changes must reuse every file");
        assert_eq!(stats.bytes_read, 0, "unchanged files must not be re-read");
        assert_eq!(index_files(&fresh), index_files(&idx_dir));
    }

    #[test]
    fn update_reflects_add_modify_remove() {
        let root = tmpdir("update-delta");
        fs::write(root.join("a.txt"), "alpha token").unwrap();
        fs::write(root.join("b.txt"), "bravo token").unwrap();
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();
        fs::write(root.join("a.txt"), "alpha token extra content").unwrap();
        fs::write(root.join("c.txt"), "charlie token").unwrap();
        update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();

        let fresh = root.join("fresh");
        build_index(&root, &fresh, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(index_files(&fresh), index_files(&idx_dir));

        let index = Index::open(&idx_dir).unwrap();
        assert_eq!(index.file_count(), 2);
        let alpha_grams =
            crate::ngram::covering_ngrams(b"alpha", crate::ngram::DEFAULT_MAX_NGRAM_LENGTH);
        for g in &alpha_grams {
            assert!(
                !index.postings(ngram_hash(g)).is_empty(),
                "gram {:?} of modified file must be present",
                String::from_utf8_lossy(g)
            );
        }
        let bravo_grams =
            crate::ngram::covering_ngrams(b"bravo", crate::ngram::DEFAULT_MAX_NGRAM_LENGTH);
        for g in &bravo_grams {
            assert!(
                index.postings(ngram_hash(g)).is_empty(),
                "gram {:?} of removed file must be gone",
                String::from_utf8_lossy(g)
            );
        }
    }

    #[test]
    fn corrupt_or_missing_grams_cache_falls_back_to_full() {
        let root = tmpdir("update-corrupt");
        fs::write(root.join("a.txt"), "alpha token").unwrap();
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();

        fs::write(idx_dir.join("grams.dat"), b"junk").unwrap();
        let stats = update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(
            stats.reused_files, 0,
            "corrupt cache must force a full rebuild"
        );
        let index = Index::open(&idx_dir).unwrap();
        assert_eq!(index.file_count(), 1);

        fs::remove_file(idx_dir.join("grams.dat")).unwrap();
        let stats = update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.reused_files, 0);
        assert!(Index::open(&idx_dir).is_ok());
    }
}
