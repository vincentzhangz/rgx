//! On-disk inverted index: build, save, load (mmap) and query.
//!
//! Layout inside `<root>/.rgx/` (all little-endian, varints are LEB128):
//!
//! * `lookup.dat`  — `"RGXLOOK2"` + `u64 count` + sorted entries of
//!   `(u40 ngram_hash, u40 postings_offset)`, 10 bytes each. Mmap'd and
//!   binary-searched. Offset is absolute into `postings.dat`.
//! * `postings.dat`— `"RGXPOST2"` + `u64 distinct` + `u64 total_ids` +
//!   posting lists back-to-back. Each list is `varint id_count` followed by
//!   `id_count` delta-varint sorted `u32` file ids (first id absolute, then
//!   gaps). Mmap'd; lists are located by lookup offsets.
//! * `files.dat`   — `"RGXFILS2"` + `u64 root_len` + root path bytes +
//!   `u64 count` + length-prefixed root-relative paths. Stored paths are
//!   relative to the indexed root; [`Index::file_path`] joins the root back
//!   on, so results stay absolute. File id = sequence index (0-based).
//! * `meta.dat`    — `"RGXMETA2"` + `u64 count` + length-prefixed
//!   root-relative path + `u64 mtime` + `u64 size`, repeated. Same order as
//!   `files.dat`; used for change detection.
//! * `grams.dat`   — `"RGXGRAM2"` + `u64 count` + per-file records of
//!   `(varint plen, path, varint mtime, varint size, content_hash u128,
//!   varint gram_count, delta-varint grams...)`, paths root-relative.
//!   A per-file n-gram cache keyed by `(mtime, size, content_hash)`; this is
//!   what makes `--update` incremental (unchanged files are never re-read).
//!   Corrupt or missing `grams.dat` simply triggers a full rebuild.
//!
//! Indexed content is ASCII case-folded before n-gram hashing, and the query
//! planner folds extracted literals the same way, so `-i` queries prune
//! correctly; final case sensitivity is decided by exact regex verification.
//!
//! The keyed n-gram hash is `ngram_hash` (FNV-1a truncated to
//! [`NGRAM_HASH_BITS`] bits over the folded gram bytes). Hash collisions can
//! only broaden a posting list, never cause a false negative, so no
//! collision handling is required.
//!
//! Builds are safe against Windows antivirus locks: a first build writes the
//! index in place (no rename), while a rebuild stages the new tables in
//! `.rgx.tmp/` and atomically swaps them in (retrying transient
//! `ERROR_ACCESS_DENIED` renames with backoff).

use crate::fingerprint::content_hash;
use crate::ngram::{MIN_NGRAM_LENGTH, NGRAM_HASH_BITS, build_all_ngrams, ngram_hash};
use crate::scanner::{ScanOptions, is_binary_content, scan};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::UNIX_EPOCH;

const POSTINGS_HEADER: &[u8] = b"RGXPOST2";
const LOOKUP_HEADER: &[u8] = b"RGXLOOK2";
const FILES_HEADER: &[u8] = b"RGXFILS2";
const META_HEADER: &[u8] = b"RGXMETA2";
const GRAMS_HEADER: &[u8] = b"RGXGRAM2";

/// Header layout: 8 magic bytes + 8 count bytes = 16.
const HEADER_LEN: usize = 16;

/// The count field within the 16-byte header.
const COUNT_OFFSET: usize = 8;

/// A `(hash, offset)` lookup entry: 5-byte hash + 5-byte offset.
const LOOKUP_ENTRY_LEN: usize = 10;

/// postings.dat has two extra header fields (`distinct`, `total_ids`) after
/// its 8-byte magic, so its payload starts at 8 + 8 + 8 = 24.
const POSTINGS_DATA_OFFSET: u64 = (HEADER_LEN + 8) as u64;

/// Buffer capacity for writing index files (64 KB).
const INDEX_WRITE_BUF_CAPACITY: usize = 64 * 1024;

/// Records buffered per worker thread while streaming a build to disk.
/// Bounded so a fast worker cannot outrun the merge loop and hold the
/// whole corpus's n-grams in RAM again.
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Smallest number of files handed to one build worker. Finer chunks than
/// one-per-core balance load when file sizes vary widely.
const WORKER_CHUNK_MIN_FILES: usize = 64;

/// Upper bound on build workers, as a multiple of available cores. Keeps
/// thread counts sane while still allowing chunks finer than one-per-core.
const WORKER_THREAD_MULTIPLIER: usize = 8;

/// Append `v` to `buf` as LEB128.
fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Read a LEB128 varint from `data` starting at `*pos`, advancing it.
fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Read a 5-byte little-endian value.
fn u40_at(data: &[u8], at: usize) -> Option<u64> {
    let s = data.get(at..at + 5)?;
    Some(
        s[0] as u64
            | (s[1] as u64) << 8
            | (s[2] as u64) << 16
            | (s[3] as u64) << 24
            | (s[4] as u64) << 32,
    )
}

/// Append `v` as 5 bytes of `buf` at `at` (little-endian).
fn put_u40(buf: &mut [u8], at: usize, v: u64) {
    debug_assert!(v < (1 << NGRAM_HASH_BITS) || at == 5, "value overflow");
    buf[at] = v as u8;
    buf[at + 1] = (v >> 8) as u8;
    buf[at + 2] = (v >> 16) as u8;
    buf[at + 3] = (v >> 24) as u8;
    buf[at + 4] = (v >> 32) as u8;
}

/// A file known to the index.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Path of the file, relative to the indexed root.
    pub path: PathBuf,
    /// Last-modified time in whole seconds since the Unix epoch.
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
}

/// A cached per-file n-gram entry loaded from `grams.dat`, used to skip
/// re-reading unchanged files during `--update`. Keys are root-relative
/// paths.
#[derive(Clone, Debug)]
struct CachedGrams {
    /// Last-modified time in whole seconds since the Unix epoch.
    mtime: u64,
    /// File size in bytes.
    size: u64,
    /// Content fingerprint; unchanged files reuse their cached grams.
    content_hash: u128,
    /// Distinct n-gram hashes of the file (length >= `MIN_NGRAM_LENGTH`).
    /// Shared so reuse is a refcount bump, not a deep copy.
    grams: Arc<[u64]>,
}

/// One file processed by a worker thread, streamed to the merge loop.
struct ProcessedFile {
    /// Path of the file relative to the indexed root (stored once;
    /// grams.dat and files.dat are both written from it).
    path: PathBuf,
    /// Last-modified time in whole seconds since the Unix epoch.
    mtime: u64,
    /// File size in bytes.
    size: u64,
    /// Content fingerprint for the grams cache.
    content_hash: u128,
    /// Distinct n-gram hashes of the file.
    grams: Arc<[u64]>,
    bytes_read: u64,
    reused: bool,
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
    /// On-disk size of each index table, for `--stats`.
    pub table_sizes: Vec<(&'static str, u64)>,
}

/// Build an index for the tree at `root` into `index_dir` (usually
/// `root/.rgx`). `progress` is invoked with human-readable status lines.
pub fn build_index(
    root: &Path,
    index_dir: &Path,
    opts: &ScanOptions,
    progress: &mut dyn FnMut(&str),
) -> std::io::Result<BuildStats> {
    run_build(root, index_dir, opts, progress, None)
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
    let cache = load_grams_cache(&index_dir.join("grams.dat"));
    let Some(cache) = cache else {
        progress("no usable n-gram cache; full rebuild");
        return build_index(root, index_dir, opts, progress);
    };
    run_build(root, index_dir, opts, progress, Some(Arc::new(cache)))
}

/// Shared build/update pipeline. Worker threads stream `ProcessedFile`
/// records to the merge loop through bounded per-chunk channels (consumed in
/// chunk order, keeping the output deterministic); the merge loop writes
/// grams.dat incrementally and accumulates postings as a flat
/// `(gram, file_id)` vector sorted once at write time — far smaller than the
/// old `HashMap<u64, Vec<u32>>` of single-element lists.
fn run_build(
    root: &Path,
    index_dir: &Path,
    opts: &ScanOptions,
    progress: &mut dyn FnMut(&str),
    cache: Option<Arc<HashMap<PathBuf, CachedGrams>>>,
) -> std::io::Result<BuildStats> {
    let started = std::time::Instant::now();
    let files = Arc::new(scan(root, opts));
    let n = files.len();
    progress(&format!("scanning: {} files", n));

    let (target, staged) = prepare_index_dir(index_dir)?;
    let receivers = spawn_workers(&files, root, cache.as_ref());
    let mut merged = merge_stream(&receivers, &target)?;

    progress(&format!(
        "update: {} files reused, {} re-read",
        merged.reused,
        n.saturating_sub(merged.reused)
    ));
    progress("writing index files");
    merged.postings.sort_unstable();
    let mut table_sizes = write_tables(&target, root, &merged.postings, &merged.entries)?;
    table_sizes.push(("grams.dat", merged.gram_bytes));
    let index_bytes = table_sizes.iter().map(|(_, b)| b).sum::<u64>();
    if staged {
        publish_index(&target, index_dir)?;
    }

    Ok(BuildStats {
        files: merged.entries.len(),
        bytes_read: merged.bytes_read,
        ngrams: distinct_grams(&merged.postings),
        postings: merged.postings.len() as u64,
        index_bytes,
        elapsed: started.elapsed(),
        reused_files: merged.reused,
        table_sizes,
    })
}

/// Decide where the new tables are written: a first build goes directly
/// into `index_dir` (avoiding the rename Windows antivirus can block);
/// rebuilds stage into `<index_dir>.tmp` for an atomic swap. Returns the
/// target directory and whether [`publish_index`] is needed.
fn prepare_index_dir(index_dir: &Path) -> std::io::Result<(PathBuf, bool)> {
    if !index_dir.exists() {
        fs::create_dir_all(index_dir)?;
        Ok((index_dir.to_path_buf(), false))
    } else {
        let staging = PathBuf::from(format!("{}.tmp", index_dir.display()));
        remove_dir_all_retry(&staging);
        fs::create_dir_all(&staging)?;
        Ok((staging, true))
    }
}

/// Spawn one worker per chunk of the file list. Chunks are finer than
/// one-per-core (bounded by [`WORKER_THREAD_MULTIPLIER`]×cores, at least
/// [`WORKER_CHUNK_MIN_FILES`] files each) so a few large files cannot idle
/// the remaining workers. Each worker sends its records through a bounded
/// channel; the returned receivers are in chunk order so the merge loop
/// sees files in scan order.
fn spawn_workers(
    files: &Arc<Vec<PathBuf>>,
    root: &Path,
    cache: Option<&Arc<HashMap<PathBuf, CachedGrams>>>,
) -> Vec<Receiver<ProcessedFile>> {
    let mut receivers = Vec::new();
    if files.is_empty() {
        return receivers;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);
    let max_threads = cores * WORKER_THREAD_MULTIPLIER;
    let chunk_size = files
        .len()
        .div_ceil(max_threads)
        .max(WORKER_CHUNK_MIN_FILES);
    let mut start = 0usize;
    for chunk in files.chunks(chunk_size) {
        let (tx, rx) = sync_channel(STREAM_CHANNEL_CAPACITY);
        let end = start + chunk.len();
        let files = Arc::clone(files);
        let cache = cache.cloned();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            for i in start..end {
                if process_file(&files[i], &root, cache.as_deref(), &tx).is_err() {
                    break;
                }
            }
        });
        start = end;
        receivers.push(rx);
    }
    receivers
}

/// Path of `path` relative to the index `root`. Paths outside the root
/// (possible via followed symlinks) fall back to the absolute path;
/// `Path::join` with an absolute argument reproduces it unchanged.
fn rel_key(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Read (or reuse the cached grams of) one file and send it downstream.
/// Unreadable, empty or binary files are skipped; per-file errors never
/// abort the rest of a worker's chunk.
fn process_file(
    path: &Path,
    root: &Path,
    cache: Option<&HashMap<PathBuf, CachedGrams>>,
    tx: &SyncSender<ProcessedFile>,
) -> std::io::Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    let mtime = mtime_secs(&meta);
    let size = meta.len();

    let rel = rel_key(path, root);
    let cached = cache.and_then(|c| c.get(&rel));
    let (content_hash, grams, bytes_read, reused) = match cached {
        Some(c) if c.mtime == mtime && c.size == size => {
            (c.content_hash, Arc::clone(&c.grams), 0, true)
        }
        Some(c) => {
            let Ok(content) = fs::read(path) else {
                return Ok(());
            };
            if content.is_empty() || is_binary_content(&content) {
                return Ok(());
            }
            let hash = content_hash(&content);
            let grams: Arc<[u64]> = if hash == c.content_hash {
                Arc::clone(&c.grams)
            } else {
                Arc::from(file_grams(&content))
            };
            (hash, grams, content.len() as u64, hash == c.content_hash)
        }
        None => {
            let Ok(content) = fs::read(path) else {
                return Ok(());
            };
            if content.is_empty() || is_binary_content(&content) {
                return Ok(());
            }
            let hash = content_hash(&content);
            let bytes = content.len() as u64;
            (hash, Arc::from(file_grams(&content)), bytes, false)
        }
    };

    tx.send(ProcessedFile {
        path: rel,
        mtime,
        size,
        content_hash,
        grams,
        bytes_read,
        reused,
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "merge loop closed"))
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(C, packed)]
struct Posting {
    gram: u64,
    file_id: u32,
}

impl Posting {
    #[inline(always)]
    fn new(gram: u64, file_id: u32) -> Self {
        Posting { gram, file_id }
    }

    #[inline(always)]
    fn gram(self) -> u64 {
        self.gram
    }

    #[inline(always)]
    fn file_id(self) -> u32 {
        self.file_id
    }
}

struct Merged {
    entries: Vec<FileEntry>,
    /// Flat `(gram_hash, file_id)` pairs, sorted at write time.
    postings: Vec<Posting>,
    gram_bytes: u64,
    bytes_read: u64,
    reused: usize,
}

/// Consume worker output in chunk order, writing grams.dat incrementally so
/// per-file gram vectors are dropped as soon as their postings are emitted.
fn merge_stream(receivers: &[Receiver<ProcessedFile>], target: &Path) -> std::io::Result<Merged> {
    let mut grams = GramsWriter::create(target.join("grams.dat"))?;
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut postings: Vec<Posting> = Vec::new();
    let mut bytes_read = 0u64;
    let mut reused = 0usize;

    for rx in receivers {
        for processed in rx {
            if processed.reused {
                reused += 1;
            }
            bytes_read += processed.bytes_read;
            let id = entries.len() as u32;
            postings.extend(processed.grams.iter().map(|&h| Posting::new(h, id)));
            grams.write(&processed)?;
            entries.push(FileEntry {
                path: processed.path,
                mtime: processed.mtime,
                size: processed.size,
            });
        }
    }
    let gram_bytes = grams.finish()?;

    Ok(Merged {
        entries,
        postings,
        gram_bytes,
        bytes_read,
        reused,
    })
}

/// Streaming writer for `grams.dat`. The record count is patched into the
/// header on finish because records are written before the total is known.
struct GramsWriter {
    file: BufWriter<File>,
    count: u64,
    /// Reusable encode buffer, sized to the largest record seen.
    scratch: Vec<u8>,
}

impl GramsWriter {
    fn create(path: PathBuf) -> std::io::Result<GramsWriter> {
        let mut file = BufWriter::with_capacity(INDEX_WRITE_BUF_CAPACITY, File::create(path)?);
        file.write_all(GRAMS_HEADER)?;
        file.write_all(&0u64.to_le_bytes())?;
        Ok(GramsWriter {
            file,
            count: 0,
            scratch: Vec::new(),
        })
    }

    fn write(&mut self, p: &ProcessedFile) -> std::io::Result<()> {
        let buf = &mut self.scratch;
        buf.clear();
        let path = p.path.to_string_lossy();
        push_varint(buf, path.len() as u64);
        buf.extend_from_slice(path.as_bytes());
        push_varint(buf, p.mtime);
        push_varint(buf, p.size);
        buf.extend_from_slice(&p.content_hash.to_le_bytes());
        push_varint(buf, p.grams.len() as u64);
        let mut prev = 0u64;
        for &h in p.grams.iter() {
            push_varint(buf, h - prev);
            prev = h;
        }
        self.file.write_all(buf)?;
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<u64> {
        self.file.flush()?;
        let mut file = self.file.into_inner().map_err(std::io::Error::other)?;
        file.seek(SeekFrom::Start(COUNT_OFFSET as u64))?;
        file.write_all(&self.count.to_le_bytes())?;
        file.flush()?;
        file.metadata().map(|m| m.len())
    }
}

/// Collect the distinct n-grams of a file's content (length >= 3).
///
/// Content is ASCII case-folded first so the index is case-insensitive at
/// the gram level; the query planner folds extracted literals identically.
fn file_grams(content: &[u8]) -> Vec<u64> {
    let folded = content.to_ascii_lowercase();
    let mut ids = Vec::new();
    build_all_ngrams(&folded, &mut |gram| {
        if gram.len() >= MIN_NGRAM_LENGTH {
            ids.push(ngram_hash(gram));
        }
    });
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Number of distinct gram hashes in a sorted flat postings vector.
fn distinct_grams(flat: &[Posting]) -> usize {
    let mut distinct = 0usize;
    let mut prev: Option<u64> = None;
    for p in flat {
        let g = p.gram();
        if prev != Some(g) {
            distinct += 1;
            prev = Some(g);
        }
    }
    distinct
}

/// Write the four non-grams tables (grams.dat is already streamed) for
/// `dir`. `postings` must be sorted by `(gram, file_id)`. Returns the name
/// and size of each of those four files.
///
/// Posting lists are delta-varint encoded; `lookup.dat` records the byte
/// offset of each list. A first build writes into `dir` directly; rebuilds
/// pass the staging directory that [`publish_index`] later swaps in.
fn write_tables(
    dir: &Path,
    root: &Path,
    postings: &[Posting],
    entries: &[FileEntry],
) -> std::io::Result<Vec<(&'static str, u64)>> {
    let distinct = distinct_grams(postings);
    let mut scratch: Vec<u8> = Vec::new();

    let mut pfile = std::io::BufWriter::with_capacity(
        INDEX_WRITE_BUF_CAPACITY,
        File::create(dir.join("postings.dat"))?,
    );
    pfile.write_all(POSTINGS_HEADER)?;
    pfile.write_all(&(distinct as u64).to_le_bytes())?;
    pfile.write_all(&(postings.len() as u64).to_le_bytes())?;
    let mut lookup_entries: Vec<(u64, u64)> = Vec::with_capacity(distinct);
    let mut offset = POSTINGS_DATA_OFFSET;
    let mut i = 0;
    while i < postings.len() {
        let gram = postings[i].gram();
        let mut j = i;
        while j < postings.len() && postings[j].gram() == gram {
            j += 1;
        }
        scratch.clear();
        push_varint(&mut scratch, (j - i) as u64);
        let mut prev = 0u64;
        for p in &postings[i..j] {
            debug_assert_eq!(p.gram(), gram);
            debug_assert!(p.file_id() as u64 >= prev, "posting ids must ascend");
            push_varint(&mut scratch, p.file_id() as u64 - prev);
            prev = p.file_id() as u64;
        }
        lookup_entries.push((gram, offset));
        offset += scratch.len() as u64;
        pfile.write_all(&scratch)?;
        i = j;
    }
    pfile.flush()?;
    let postings_bytes = pfile.get_ref().metadata()?.len();
    drop(pfile);

    let mut lfile = std::io::BufWriter::with_capacity(
        INDEX_WRITE_BUF_CAPACITY,
        File::create(dir.join("lookup.dat"))?,
    );
    lfile.write_all(LOOKUP_HEADER)?;
    lfile.write_all(&(distinct as u64).to_le_bytes())?;
    scratch.clear();
    scratch.reserve(lookup_entries.len() * LOOKUP_ENTRY_LEN);
    for (hash, off) in &lookup_entries {
        let at = scratch.len();
        scratch.resize(at + LOOKUP_ENTRY_LEN, 0);
        put_u40(&mut scratch, at, *hash);
        put_u40(&mut scratch, at + 5, *off);
    }
    lfile.write_all(&scratch)?;
    lfile.flush()?;
    let lookup_bytes = lfile.get_ref().metadata()?.len();
    drop(lfile);

    write_path_table(dir.join("files.dat"), FILES_HEADER, root, entries)?;
    write_meta_table(dir.join("meta.dat"), entries)?;

    Ok(vec![
        ("postings.dat", postings_bytes),
        ("lookup.dat", lookup_bytes),
        ("files.dat", fs::metadata(dir.join("files.dat"))?.len()),
        ("meta.dat", fs::metadata(dir.join("meta.dat"))?.len()),
    ])
}

fn write_path_table(
    path: PathBuf,
    magic: &[u8],
    root: &Path,
    entries: &[FileEntry],
) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::with_capacity(INDEX_WRITE_BUF_CAPACITY, File::create(path)?);
    f.write_all(magic)?;
    let root_s = root.to_string_lossy();
    f.write_all(&(root_s.len() as u64).to_le_bytes())?;
    f.write_all(root_s.as_bytes())?;
    f.write_all(&(entries.len() as u64).to_le_bytes())?;
    for e in entries {
        let p = e.path.to_string_lossy();
        f.write_all(&(p.len() as u64).to_le_bytes())?;
        f.write_all(p.as_bytes())?;
    }
    f.flush()?;
    Ok(())
}

fn write_meta_table(path: PathBuf, entries: &[FileEntry]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::with_capacity(INDEX_WRITE_BUF_CAPACITY, File::create(path)?);
    f.write_all(META_HEADER)?;
    f.write_all(&(entries.len() as u64).to_le_bytes())?;
    for e in entries {
        let p = e.path.to_string_lossy();
        f.write_all(&(p.len() as u64).to_le_bytes())?;
        f.write_all(p.as_bytes())?;
        f.write_all(&e.mtime.to_le_bytes())?;
        f.write_all(&e.size.to_le_bytes())?;
    }
    f.flush()?;
    Ok(())
}

/// Load the per-file n-gram cache keyed by root-relative path. Returns
/// `None` when missing or corrupt (the caller falls back to a full rebuild).
fn load_grams_cache(path: &Path) -> Option<HashMap<PathBuf, CachedGrams>> {
    use std::io::Read;
    let file = File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(INDEX_WRITE_BUF_CAPACITY, file);
    let mut hdr = [0u8; 16];
    reader.read_exact(&mut hdr).ok()?;
    if &hdr[..8] != GRAMS_HEADER {
        return None;
    }
    let count = u64::from_le_bytes(hdr[8..16].try_into().ok()?) as usize;
    let mut out = HashMap::with_capacity(count);
    let mut path_buf = Vec::new();
    let mut hash_buf = [0u8; 16];

    macro_rules! vread {
        () => {
            match read_varint_from(&mut reader) {
                Some(v) => v,
                None => return None,
            }
        };
    }

    for _ in 0..count {
        let plen = vread!() as usize;
        path_buf.resize(plen, 0);
        reader.read_exact(&mut path_buf).ok()?;
        let rel = String::from_utf8_lossy(&path_buf).into_owned();

        let mtime = vread!();
        let size = vread!();

        reader.read_exact(&mut hash_buf).ok()?;
        let content_hash = u128::from_le_bytes(hash_buf);

        let gcount = vread!() as usize;

        let mut grams = Vec::with_capacity(gcount.min(1 << 20));
        let mut prev = 0u64;
        for _ in 0..gcount {
            prev += vread!();
            grams.push(prev);
        }

        out.insert(
            PathBuf::from(rel),
            CachedGrams {
                mtime,
                size,
                content_hash,
                grams: Arc::from(grams),
            },
        );
    }
    Some(out)
}

/// Read a LEB128 varint from an `io::Read` stream.
fn read_varint_from<R: std::io::Read>(reader: &mut R) -> Option<u64> {
    let mut byte = [0u8; 1];
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        reader.read_exact(&mut byte).ok()?;
        let b = byte[0];
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
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
    /// Cached entry count so binary search never re-reads the header.
    lookup_count: usize,
    postings: Mmap,
    files: Mmap,
    /// Indexed root, stored in `files.dat`; joined onto stored relative
    /// paths to reconstruct absolute paths.
    root: PathBuf,
    file_offsets: Vec<u64>,
    meta: Mmap,
    meta_offsets: Vec<u64>,
}

impl Index {
    /// Load an index previously written by `build_index`.
    ///
    /// Validates the structural integrity of every file and returns an error
    /// (never panics) if any of them is truncated or corrupted. An index
    /// written by an older format version fails with `InvalidData`, which
    /// callers can use to trigger a rebuild.
    pub fn open(index_dir: &Path) -> std::io::Result<Index> {
        let lf = File::open(index_dir.join("lookup.dat"))?;
        let pf = File::open(index_dir.join("postings.dat"))?;
        let ff = File::open(index_dir.join("files.dat"))?;
        let mf = File::open(index_dir.join("meta.dat"))?;

        let lookup = unsafe { Mmap::map(&lf)? };
        let postings = unsafe { Mmap::map(&pf)? };
        let files = unsafe { Mmap::map(&ff)? };
        let meta = unsafe { Mmap::map(&mf)? };
        check_header(&lookup, LOOKUP_HEADER)?;
        check_header(&postings, POSTINGS_HEADER)?;
        check_header(&files, FILES_HEADER)?;
        check_header(&meta, META_HEADER)?;

        let lookup_count = read_u64(&lookup, COUNT_OFFSET)? as usize;
        if lookup.len() < HEADER_LEN + lookup_count * LOOKUP_ENTRY_LEN {
            return Err(corrupt("lookup table truncated"));
        }

        let postings_distinct = read_u64(&postings, COUNT_OFFSET)? as usize;
        let postings_total_ids = read_u64(&postings, COUNT_OFFSET + 8)? as usize;
        if postings.len() < POSTINGS_DATA_OFFSET as usize {
            return Err(corrupt("posting table truncated"));
        }
        if postings_distinct != lookup_count || postings_total_ids < postings_distinct {
            return Err(corrupt("posting/lookup counts disagree"));
        }

        if lookup_count > 0 {
            let first_off = u40_at(&lookup, HEADER_LEN + 5)
                .ok_or_else(|| corrupt("lookup table truncated"))?
                as usize;
            let last_off = u40_at(
                &lookup,
                HEADER_LEN + (lookup_count - 1) * LOOKUP_ENTRY_LEN + 5,
            )
            .ok_or_else(|| corrupt("lookup table truncated"))? as usize;
            if first_off < POSTINGS_DATA_OFFSET as usize || last_off >= postings.len() {
                return Err(corrupt("posting list offset out of range"));
            }
        }

        let (root, file_offsets) = read_file_offsets(&files)?;
        let meta_offsets = read_meta_offsets(&meta)?;

        Ok(Index {
            lookup,
            lookup_count,
            postings,
            files,
            root,
            file_offsets,
            meta,
            meta_offsets,
        })
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.file_offsets.len()
    }

    /// Distinct n-grams in the lookup table.
    pub fn ngram_count(&self) -> u64 {
        u64_at(&self.lookup, COUNT_OFFSET).unwrap_or(0)
    }

    /// Total posting entries.
    pub fn posting_count(&self) -> u64 {
        u64_at(&self.postings, COUNT_OFFSET + 8).unwrap_or(0)
    }

    /// Absolute path of a file id (stored relative path joined onto the
    /// indexed root).
    pub fn file_path(&self, id: u32) -> Option<PathBuf> {
        let &p = self.file_offsets.get(id as usize)?;
        let plen = u64_at(&self.files, p as usize)? as usize;
        let bytes = self.files.get((p as usize + 8)..(p as usize + 8 + plen))?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Some(
                self.root
                    .join(Path::new(std::ffi::OsStr::from_bytes(bytes))),
            )
        }
        #[cfg(not(unix))]
        {
            Some(self.root.join(std::str::from_utf8(bytes).ok()?))
        }
    }

    /// Stored (mtime, size) for a file id, used for change detection.
    pub fn file_meta(&self, id: u32) -> Option<(u64, u64)> {
        let &p = self.meta_offsets.get(id as usize)?;
        let plen = u64_at(&self.meta, p as usize)? as usize;
        let at = p as usize + 8 + plen;
        let mtime = u64_at(&self.meta, at)?;
        let size = u64_at(&self.meta, at + 8)?;
        Some((mtime, size))
    }

    /// The posting list for an n-gram hash, appended to `out` (cleared
    /// first) so callers can reuse one buffer across grams.
    ///
    /// Safe against malformed files: out-of-range data yields an empty list.
    pub fn postings_into(&self, hash: u64, out: &mut Vec<u32>) {
        out.clear();
        let Some(offset) = self.lookup_offset(hash) else {
            return;
        };
        let mut pos = offset as usize;
        let Some(list_len) = read_varint(&self.postings, &mut pos) else {
            return;
        };
        // A posting list can never exceed the number of indexed files; cap
        // the reservation so a corrupt count cannot spike memory.
        out.reserve((list_len as usize).min(self.file_count().max(1)));
        let mut prev = 0u64;
        for _ in 0..list_len {
            match read_varint(&self.postings, &mut pos) {
                Some(delta) => {
                    prev += delta;
                    out.push(prev as u32);
                }
                None => {
                    out.clear();
                    return;
                }
            }
        }
    }

    /// The posting list for an n-gram hash (empty when absent).
    pub fn postings(&self, hash: u64) -> Vec<u32> {
        let mut out = Vec::new();
        self.postings_into(hash, &mut out);
        out
    }

    /// True when `hash` exists in the lookup table.
    pub fn has_ngram(&self, hash: u64) -> bool {
        self.lookup_offset(hash).is_some()
    }

    /// Binary search the sorted lookup table for `hash`, returning its
    /// absolute `postings.dat` offset when present.
    fn lookup_offset(&self, hash: u64) -> Option<u64> {
        let base = HEADER_LEN;
        let mut lo = 0usize;
        let mut hi = self.lookup_count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = base + mid * LOOKUP_ENTRY_LEN;
            let h = u40_at(&self.lookup, at)?;
            if h < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < self.lookup_count {
            let at = base + lo * LOOKUP_ENTRY_LEN;
            let h = u40_at(&self.lookup, at)?;
            if h == hash {
                return u40_at(&self.lookup, at + 5);
            }
        }
        None
    }
}

fn check_header(data: &Mmap, magic: &[u8]) -> std::io::Result<()> {
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

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
}

/// Parse `files.dat`: returns the indexed root and per-file byte offsets of
/// the length-prefixed relative paths.
fn read_file_offsets(data: &Mmap) -> std::io::Result<(PathBuf, Vec<u64>)> {
    check_header(data, FILES_HEADER)?;
    let root_len = read_u64(data, COUNT_OFFSET)? as usize;
    let count_at = HEADER_LEN + root_len;
    if data.len() < count_at + 8 {
        return Err(corrupt("file table truncated"));
    }
    let root_bytes = data
        .get(HEADER_LEN..count_at)
        .ok_or_else(|| corrupt("file table truncated"))?;
    #[cfg(unix)]
    let root = PathBuf::from(std::ffi::OsStr::from_bytes(root_bytes));
    #[cfg(not(unix))]
    let root = PathBuf::from(std::str::from_utf8(root_bytes).map_err(|_| corrupt("bad root")?));

    let count = read_u64(data, count_at)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut p = count_at + 8;
    for _ in 0..count {
        out.push(p as u64);
        let plen = read_u64(data, p)? as usize;
        p = p
            .checked_add(8 + plen)
            .ok_or_else(|| corrupt("path length overflow"))?;
        if p > data.len() {
            return Err(corrupt("file table truncated"));
        }
    }
    Ok((root, out))
}

/// Parse `meta.dat` into per-file byte offsets.
fn read_meta_offsets(data: &Mmap) -> std::io::Result<Vec<u64>> {
    check_header(data, META_HEADER)?;
    let count = read_u64(data, COUNT_OFFSET)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut p = HEADER_LEN;
    for _ in 0..count {
        out.push(p as u64);
        let plen = read_u64(data, p)? as usize;
        p = p
            .checked_add(24 + plen)
            .ok_or_else(|| corrupt("path length overflow"))?;
        if p > data.len() {
            return Err(corrupt("meta table truncated"));
        }
    }
    Ok(out)
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
    fn binary_content_skipped_at_index_time() {
        let root = tmpdir("binary-content");
        fs::write(root.join("text.txt"), "readable text").unwrap();
        // Text-looking extension but NUL bytes up front: the scanner no
        // longer opens files for a content check, so indexing must skip it.
        let mut fake_text = vec![0u8; 600];
        fake_text.extend_from_slice(b"words after the nul region");
        fs::write(root.join("fake.txt"), &fake_text).unwrap();

        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        let stats = build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.files, 1, "binary content must be skipped");
        let index = Index::open(&idx_dir).unwrap();
        assert!(!index.file_path(0).unwrap().ends_with("fake.txt"));
    }

    #[test]
    fn index_is_case_folded() {
        let root = tmpdir("folded");
        fs::write(root.join("a.txt"), "Hello World\n").unwrap();
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        let index = Index::open(&idx_dir).unwrap();
        for gram in
            crate::ngram::covering_ngrams(b"hello world", crate::ngram::DEFAULT_MAX_NGRAM_LENGTH)
        {
            assert!(
                index.has_ngram(ngram_hash(&gram)),
                "folded gram {:?} missing from folded index",
                String::from_utf8_lossy(&gram)
            );
        }
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
        drop(index);

        fs::remove_file(idx_dir.join("grams.dat")).unwrap();
        let stats = update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.reused_files, 0);
        assert!(Index::open(&idx_dir).is_ok());
    }

    #[test]
    fn parallel_build_and_update_many_files() {
        let root = tmpdir("parallel-many");
        for i in 0..50 {
            fs::write(
                root.join(format!("file_{i:03}.txt")),
                format!("content for file number {i} with unique_token_{i} and common_keyword\n"),
            )
            .unwrap();
        }
        let idx_dir = root.join(".rgx");
        let mut progress = |_: &str| {};
        let stats = build_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(stats.files, 50);

        let index = Index::open(&idx_dir).unwrap();
        assert_eq!(index.file_count(), 50);

        let common_grams = crate::ngram::covering_ngrams(
            b"common_keyword",
            crate::ngram::DEFAULT_MAX_NGRAM_LENGTH,
        );
        for g in &common_grams {
            let post = index.postings(ngram_hash(g));
            assert_eq!(post.len(), 50, "all 50 files should contain common_keyword");
        }
        drop(index);

        // Test incremental update
        fs::write(
            root.join("file_025.txt"),
            "modified content with modified_token_25 and common_keyword\n",
        )
        .unwrap();
        fs::write(
            root.join("file_050.txt"),
            "brand new file with brand_new_token and common_keyword\n",
        )
        .unwrap();

        let update_stats =
            update_index(&root, &idx_dir, &ScanOptions::default(), &mut progress).unwrap();
        assert_eq!(update_stats.files, 51);
        assert_eq!(update_stats.reused_files, 49);

        let updated_index = Index::open(&idx_dir).unwrap();
        assert_eq!(updated_index.file_count(), 51);
    }
}
