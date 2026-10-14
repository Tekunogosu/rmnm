//! Patch and Repair: bring an install folder in line with the server's manifest.
//!
//! 1. Check every manifest file on disk (size, then XXH3-128). Patch trusts a file whose
//!    recorded hash, size and mtime are unchanged; Repair hashes everything.
//! 2. Download the bundles the missing or wrong files need, several at a time, in the order the
//!    files need them. Downloads run at most `DOWNLOAD_WINDOW` bundles ahead of the assembler,
//!    and a bundle is deleted once every file needing it is assembled, so peak extra disk use is
//!    about the compressed size of the largest file being rebuilt plus that window.
//! 3. Assemble each file from its zstd chunks into `<name>.rmnm-part`, checking every chunk's
//!    CRC-64 and the file's XXH3-128, then rename it into place.
//!
//! Reinstall first deletes every file the manifest or rmnm's records list, the bundle cache and
//! the verified-file records, then runs as a fresh install.
//!
//! Files on disk that neither the manifest nor rmnm's records list are reported, never deleted.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crc_fast::{CrcAlgorithm, Digest};
use xxhash_rust::xxh3::Xxh3;

use crate::api::{Api, GameVersion};
use crate::db::{Db, VerifiedFile};
use crate::manifest::{self, FileEntry, Manifest};
use crate::session::now;

/// Concurrent bundle downloads.
const DOWNLOAD_WORKERS: usize = 4;
/// Downloaded bundles allowed to wait on disk for the assembler (bundles are up to ~45 MB).
const DOWNLOAD_WINDOW: usize = 8;
const DOWNLOAD_ATTEMPTS: usize = 3;
const BUNDLES_DIR: &str = ".bundles";
const PART_SUFFIX: &str = ".rmnm-part";
const READ_BUFFER: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Patch,
    Repair,
    Reinstall,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Patch => "Patch",
            Self::Repair => "Repair",
            Self::Reinstall => "Reinstall",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Manifest,
    Clearing,
    Checking,
    Downloading,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: Phase,
    /// Bytes checked (Checking) or downloaded (Downloading).
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
    pub current: String,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub version: String,
    pub files: usize,
    pub rewritten: usize,
    /// Bytes fetched over the network; bundles reused from an interrupted run are not counted.
    pub downloaded: u64,
    /// Files in the install folder that the manifest does not list, relative to it.
    pub unlisted: Vec<String>,
}

/// Returned (inside `anyhow::Error`) when the job stopped because it was asked to.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub struct Job<'a> {
    pub api: &'a Api,
    pub db_path: &'a Path,
    pub install_dir: &'a Path,
    pub version: &'a GameVersion,
    pub mode: Mode,
    pub cancel: &'a AtomicBool,
}

/// Rate-limits progress callbacks to about ten per second.
struct Reporter<'a> {
    sink: &'a mut dyn FnMut(Progress),
    last: Option<Instant>,
}

impl Reporter<'_> {
    fn send(&mut self, force: bool, progress: impl FnOnce() -> Progress) {
        let due = self
            .last
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
        if force || due {
            (self.sink)(progress());
            self.last = Some(Instant::now());
        }
    }
}

impl Job<'_> {
    pub fn run(&self, progress: &mut dyn FnMut(Progress)) -> Result<Report> {
        let mut reporter = Reporter {
            sink: progress,
            last: None,
        };
        let db = Db::open(self.db_path)?;
        let dir_key = self.install_dir.display().to_string();
        fs::create_dir_all(self.install_dir)
            .with_context(|| format!("creating {}", self.install_dir.display()))?;
        db.begin_patch(&dir_key, &self.version.slug, now())?;

        reporter.send(true, || Progress {
            phase: Phase::Manifest,
            done: 0,
            total: 0,
            files_done: 0,
            files_total: 0,
            current: "Downloading the manifest".into(),
        });
        let bytes = self
            .api
            .download(&self.version.manifest_url)?
            .bytes()
            .context("downloading the manifest")?;
        let manifest = manifest::parse(&bytes)?;
        remove_stale_parts(self.install_dir)?;
        if self.mode == Mode::Reinstall {
            self.stop_requested()?;
            reporter.send(true, || Progress {
                phase: Phase::Clearing,
                done: 0,
                total: 0,
                files_done: 0,
                files_total: 0,
                current: "Deleting the installed files".into(),
            });
            clear_install(&db, &dir_key, self.install_dir, &manifest)?;
        }

        let needed = self.check(&db, &dir_key, &manifest, &mut reporter)?;
        let downloaded = if needed.is_empty() {
            0
        } else {
            self.download_and_assemble(&db, &dir_key, &needed, &mut reporter)?
        };

        let _ = fs::remove_dir(self.install_dir.join(BUNDLES_DIR));
        db.finish_patch(&dir_key, &self.version.version, now())?;
        Ok(Report {
            version: self.version.version.clone(),
            files: manifest.files.len(),
            rewritten: needed.len(),
            downloaded,
            unlisted: unlisted_files(self.install_dir, &manifest)?,
        })
    }

    fn stop_requested(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Returns the files that are missing or do not match the manifest.
    fn check<'m>(
        &self,
        db: &Db,
        dir_key: &str,
        manifest: &'m Manifest,
        reporter: &mut Reporter<'_>,
    ) -> Result<Vec<&'m FileEntry>> {
        let records = match self.mode {
            Mode::Patch => db.verified_files(dir_key)?,
            Mode::Repair | Mode::Reinstall => HashMap::new(),
        };
        let total: u64 = manifest.files.iter().map(|f| f.size).sum();
        let files_total = manifest.files.len();
        let mut done = 0;
        let mut needed = Vec::new();
        for (index, file) in manifest.files.iter().enumerate() {
            self.stop_requested()?;
            let path = file.local_path(self.install_dir);
            let base = done;
            let mut report = |done: u64, force: bool| {
                reporter.send(force, || Progress {
                    phase: Phase::Checking,
                    done,
                    total,
                    files_done: index,
                    files_total,
                    current: file.path.clone(),
                })
            };
            report(base, false);
            let meta = match fs::metadata(&path) {
                Ok(meta) => Some(meta),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
            };
            let matches = match meta {
                Some(meta) if meta.is_file() && meta.len() == file.size => {
                    let expected = hash_hex(file.hash);
                    let current = verified_file(&expected, &meta);
                    if records.get(&file.path) == Some(&current) {
                        true
                    } else if hash_file(&path, self.cancel, |n| report(base + n, false))?
                        == file.hash
                    {
                        db.record_verified_file(dir_key, &file.path, &current)?;
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !matches {
                db.forget_verified_file(dir_key, &file.path)?;
                needed.push(file);
            }
            done += file.size;
        }
        reporter.send(true, || Progress {
            phase: Phase::Checking,
            done,
            total,
            files_done: files_total,
            files_total,
            current: format!("{} file(s) to download", needed.len()),
        });
        Ok(needed)
    }

    fn download_and_assemble(
        &self,
        db: &Db,
        dir_key: &str,
        needed: &[&FileEntry],
        reporter: &mut Reporter<'_>,
    ) -> Result<u64> {
        let plan = BundlePlan::new(needed);
        let bundles_dir = self.install_dir.join(BUNDLES_DIR);
        fs::create_dir_all(&bundles_dir)
            .with_context(|| format!("creating {}", bundles_dir.display()))?;

        // `downloaded` drives the progress bar and includes reused bundles; `fetched` is network only.
        let downloaded = AtomicU64::new(0);
        let fetched = AtomicU64::new(0);
        let abort = AtomicBool::new(false);
        let queue = Mutex::new(Queue {
            next: 0,
            permits: DOWNLOAD_WINDOW,
            ready: HashMap::new(),
        });
        let ready = Condvar::new();
        let fetcher = Fetcher {
            api: self.api,
            chunks_url: self.version.chunks_url.trim_end_matches('/'),
            dir: &bundles_dir,
            downloaded: &downloaded,
            fetched: &fetched,
            stop: &|| abort.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed),
        };

        let result = thread::scope(|scope| {
            for _ in 0..DOWNLOAD_WORKERS.min(plan.order.len()) {
                scope.spawn(|| fetcher.work(&plan, &queue, &ready));
            }
            let result = self.assemble(
                db,
                dir_key,
                needed,
                &plan,
                &bundles_dir,
                &queue,
                &ready,
                &downloaded,
                reporter,
            );
            abort.store(true, Ordering::Relaxed);
            ready.notify_all();
            result
        });
        result.map(|()| fetched.load(Ordering::Relaxed))
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        &self,
        db: &Db,
        dir_key: &str,
        needed: &[&FileEntry],
        plan: &BundlePlan,
        bundles_dir: &Path,
        queue: &Mutex<Queue>,
        ready: &Condvar,
        downloaded: &AtomicU64,
        reporter: &mut Reporter<'_>,
    ) -> Result<()> {
        let mut remaining = plan.users.clone();
        let mut consumed = 0;
        let mut assembler = Assembler::new()?;
        for (index, file) in needed.iter().enumerate() {
            let mut report = |force: bool| {
                reporter.send(force, || Progress {
                    phase: Phase::Downloading,
                    done: downloaded.load(Ordering::Relaxed),
                    total: plan.total_bytes,
                    files_done: index,
                    files_total: needed.len(),
                    current: file.path.clone(),
                })
            };
            for bundle in distinct_bundles(file) {
                if plan.position[&bundle] != consumed {
                    continue; // first needed by an earlier file and already on disk
                }
                let mut guard = queue
                    .lock()
                    .map_err(|_| anyhow!("download worker panicked"))?;
                let outcome = loop {
                    if let Some(outcome) = guard.ready.remove(&consumed) {
                        break outcome;
                    }
                    self.stop_requested()?;
                    guard = ready
                        .wait_timeout(guard, Duration::from_millis(100))
                        .map_err(|_| anyhow!("download worker panicked"))?
                        .0;
                    drop(guard);
                    report(false);
                    guard = queue
                        .lock()
                        .map_err(|_| anyhow!("download worker panicked"))?;
                };
                guard.permits += 1;
                drop(guard);
                ready.notify_all();
                consumed += 1;
                outcome.map_err(|e| anyhow!(e))?;
            }
            self.stop_requested()?;
            report(false);
            let verified = assembler.assemble(self.install_dir, bundles_dir, file)?;
            db.record_verified_file(dir_key, &file.path, &verified)?;
            for bundle in distinct_bundles(file) {
                let users = remaining.get_mut(&bundle).expect("bundle in plan");
                *users -= 1;
                if *users == 0 {
                    let _ = fs::remove_file(bundles_dir.join(manifest::bundle_file_name(bundle)));
                }
            }
        }
        reporter.send(true, || Progress {
            phase: Phase::Downloading,
            done: downloaded.load(Ordering::Relaxed),
            total: plan.total_bytes,
            files_done: needed.len(),
            files_total: needed.len(),
            current: "Done".into(),
        });
        Ok(())
    }
}

/// The bundles the files need, in the order the assembler will first need them.
struct BundlePlan {
    order: Vec<u64>,
    position: HashMap<u64, usize>,
    /// Number of files still needing each bundle.
    users: HashMap<u64, usize>,
    /// Lower bound of each bundle's size: the furthest byte any chunk reads from it.
    sizes: HashMap<u64, u64>,
    total_bytes: u64,
}

impl BundlePlan {
    fn new(files: &[&FileEntry]) -> Self {
        let mut order = Vec::new();
        let mut position = HashMap::new();
        let mut users: HashMap<u64, usize> = HashMap::new();
        let mut sizes: HashMap<u64, u64> = HashMap::new();
        for file in files {
            for bundle in distinct_bundles(file) {
                *users.entry(bundle).or_default() += 1;
                position.entry(bundle).or_insert_with(|| {
                    order.push(bundle);
                    order.len() - 1
                });
            }
            for chunk in &file.chunks {
                let end = chunk.bundle_offset + chunk.length_in_bundle;
                let size = sizes.entry(chunk.bundle).or_default();
                *size = (*size).max(end);
            }
        }
        let total_bytes = sizes.values().sum();
        Self {
            order,
            position,
            users,
            sizes,
            total_bytes,
        }
    }
}

fn distinct_bundles(file: &FileEntry) -> Vec<u64> {
    let mut seen = HashSet::new();
    file.chunks
        .iter()
        .map(|c| c.bundle)
        .filter(|b| seen.insert(*b))
        .collect()
}

struct Queue {
    /// Index into `BundlePlan::order` of the next bundle to start downloading.
    next: usize,
    /// Downloads that may still start before the assembler consumes one.
    permits: usize,
    ready: HashMap<usize, Result<(), String>>,
}

struct Fetcher<'a> {
    api: &'a Api,
    chunks_url: &'a str,
    dir: &'a Path,
    downloaded: &'a AtomicU64,
    fetched: &'a AtomicU64,
    stop: &'a (dyn Fn() -> bool + Sync),
}

impl Fetcher<'_> {
    fn work(&self, plan: &BundlePlan, queue: &Mutex<Queue>, ready: &Condvar) {
        loop {
            let index = {
                let Ok(mut guard) = queue.lock() else { return };
                loop {
                    if (self.stop)() || guard.next >= plan.order.len() {
                        return;
                    }
                    if guard.permits > 0 {
                        break;
                    }
                    let Ok((g, _)) = ready.wait_timeout(guard, Duration::from_millis(200)) else {
                        return;
                    };
                    guard = g;
                }
                guard.permits -= 1;
                guard.next += 1;
                guard.next - 1
            };
            let bundle = plan.order[index];
            let outcome = self
                .fetch(bundle, plan.sizes[&bundle])
                .map_err(|e| format!("{e:#}"));
            let Ok(mut guard) = queue.lock() else { return };
            guard.ready.insert(index, outcome);
            drop(guard);
            ready.notify_all();
        }
    }

    /// Makes `<dir>/<crc>.bin` exist with the right CRC, downloading it if needed.
    fn fetch(&self, crc: u64, expected_size: u64) -> Result<()> {
        let name = manifest::bundle_file_name(crc);
        let dest = self.dir.join(&name);
        if dest.exists() {
            if crc64_file(&dest)? == crc {
                self.downloaded.fetch_add(expected_size, Ordering::Relaxed);
                return Ok(());
            }
            fs::remove_file(&dest)
                .with_context(|| format!("removing corrupt {}", dest.display()))?;
        }
        let url = format!("{}/{name}", self.chunks_url);
        let part = self.dir.join(format!("{name}{PART_SUFFIX}"));
        let mut last_error = anyhow!("no download attempted");
        for _ in 0..DOWNLOAD_ATTEMPTS {
            let mut counted = 0;
            match self.download_once(&url, &part, crc, &mut counted) {
                Ok(()) => {
                    fs::rename(&part, &dest)
                        .with_context(|| format!("saving {}", dest.display()))?;
                    return Ok(());
                }
                Err(e) => {
                    let _ = fs::remove_file(&part);
                    self.downloaded.fetch_sub(counted, Ordering::Relaxed);
                    self.fetched.fetch_sub(counted, Ordering::Relaxed);
                    if (self.stop)() {
                        return Err(Cancelled.into());
                    }
                    last_error = e;
                }
            }
        }
        Err(last_error.context(format!(
            "bundle {name} failed after {DOWNLOAD_ATTEMPTS} attempts"
        )))
    }

    fn download_once(&self, url: &str, part: &Path, crc: u64, counted: &mut u64) -> Result<()> {
        let mut response = self.api.download(url)?;
        let mut out = BufWriter::new(
            File::create(part).with_context(|| format!("creating {}", part.display()))?,
        );
        let mut digest = Digest::new(CrcAlgorithm::Crc64Redis);
        let mut buf = vec![0; 64 * 1024];
        loop {
            if (self.stop)() {
                return Err(Cancelled.into());
            }
            let n = response.read(&mut buf).context("reading the download")?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
            out.write_all(&buf[..n])
                .with_context(|| format!("writing {}", part.display()))?;
            *counted += n as u64;
            self.downloaded.fetch_add(n as u64, Ordering::Relaxed);
            self.fetched.fetch_add(n as u64, Ordering::Relaxed);
        }
        out.flush()?;
        if digest.finalize() != crc {
            bail!("downloaded bundle failed its CRC check");
        }
        Ok(())
    }
}

/// Rebuilds files from bundles, reusing its buffers and zstd context across files.
struct Assembler {
    zstd: zstd::bulk::Decompressor<'static>,
    compressed: Vec<u8>,
    plain: Vec<u8>,
}

impl Assembler {
    fn new() -> Result<Self> {
        Ok(Self {
            zstd: zstd::bulk::Decompressor::new()?,
            compressed: Vec::new(),
            plain: Vec::new(),
        })
    }

    fn assemble(
        &mut self,
        install_dir: &Path,
        bundles_dir: &Path,
        file: &FileEntry,
    ) -> Result<VerifiedFile> {
        let dest = file.local_path(install_dir);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let part = part_path(&dest);
        let written = self.write_part(bundles_dir, file, &part);
        if let Err(e) = written {
            let _ = fs::remove_file(&part);
            return Err(e.context(format!("assembling {}", file.path)));
        }
        fs::rename(&part, &dest).with_context(|| format!("replacing {}", dest.display()))?;
        let meta = fs::metadata(&dest)?;
        Ok(verified_file(&hash_hex(file.hash), &meta))
    }

    fn write_part(&mut self, bundles_dir: &Path, file: &FileEntry, part: &Path) -> Result<()> {
        let mut out = BufWriter::with_capacity(READ_BUFFER, File::create(part)?);
        let mut hasher = Xxh3::new();
        let mut open: Option<(u64, File)> = None;
        for chunk in &file.chunks {
            let bundle = match &mut open {
                Some((crc, handle)) if *crc == chunk.bundle => handle,
                _ => {
                    let path = bundles_dir.join(manifest::bundle_file_name(chunk.bundle));
                    let handle =
                        File::open(&path).with_context(|| format!("opening {}", path.display()))?;
                    &mut open.insert((chunk.bundle, handle)).1
                }
            };
            self.compressed.resize(chunk.length_in_bundle as usize, 0);
            bundle.seek(SeekFrom::Start(chunk.bundle_offset))?;
            bundle
                .read_exact(&mut self.compressed)
                .context("bundle is shorter than the manifest says")?;

            self.plain.clear();
            self.plain.reserve(chunk.length as usize);
            self.zstd
                .decompress_to_buffer(&self.compressed, &mut self.plain)
                .context("chunk does not decompress")?;
            if self.plain.len() as u64 != chunk.length {
                bail!(
                    "chunk at offset {} decompressed to the wrong size",
                    chunk.offset
                );
            }
            if crc_fast::checksum(CrcAlgorithm::Crc64Redis, &self.plain) != chunk.crc {
                bail!("chunk at offset {} failed its CRC check", chunk.offset);
            }
            hasher.update(&self.plain);
            out.write_all(&self.plain)?;
        }
        let handle = out.into_inner().map_err(|e| e.into_error())?;
        handle.sync_all()?;
        if hasher.digest128() != file.hash {
            bail!("assembled file does not match its manifest hash");
        }
        Ok(())
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(PART_SUFFIX);
    dest.with_file_name(name)
}

pub fn hash_hex(hash: u128) -> String {
    format!("{hash:032x}")
}

fn verified_file(hash: &str, meta: &fs::Metadata) -> VerifiedFile {
    VerifiedFile {
        hash: hash.to_string(),
        size: meta.len() as i64,
        mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
    }
}

/// XXH3-128 of a file, reporting bytes read so far.
fn hash_file(path: &Path, cancel: &AtomicBool, mut on_progress: impl FnMut(u64)) -> Result<u128> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0; READ_BUFFER];
    let mut read = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            return Ok(hasher.digest128());
        }
        hasher.update(&buf[..n]);
        read += n as u64;
        on_progress(read);
    }
}

fn crc64_file(path: &Path) -> Result<u64> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut digest = Digest::new(CrcAlgorithm::Crc64Redis);
    let mut buf = vec![0; READ_BUFFER];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(digest.finalize());
        }
        digest.update(&buf[..n]);
    }
}

/// Calls `visit` with every regular file under `dir`, skipping the bundle cache.
fn walk_files(root: &Path, dir: &Path, visit: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if dir == root && entry.file_name() == BUNDLES_DIR {
                continue;
            }
            walk_files(root, &path, visit)?;
        } else if kind.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

/// Deletes every file the manifest lists or rmnm recorded as verified (an earlier build's files),
/// the bundle cache, and the records. Anything else in the folder, such as `game.db`, is kept.
fn clear_install(db: &Db, dir_key: &str, install_dir: &Path, manifest: &Manifest) -> Result<()> {
    let mut paths: HashSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    for recorded in db.verified_files(dir_key)?.into_keys() {
        paths.insert(manifest::relative_path(&recorded)?);
    }
    for path in paths {
        remove_if_present(&install_dir.join(path), |p| fs::remove_file(p))?;
    }
    remove_if_present(&install_dir.join(BUNDLES_DIR), |p| fs::remove_dir_all(p))?;
    db.forget_verified_files(dir_key)
}

fn remove_if_present(path: &Path, remove: impl FnOnce(&Path) -> io::Result<()>) -> Result<()> {
    match remove(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Deletes `*.rmnm-part` files left by an interrupted run; they are only ever rmnm's own.
fn remove_stale_parts(install_dir: &Path) -> Result<()> {
    walk_files(install_dir, install_dir, &mut |path| {
        if path.to_string_lossy().ends_with(PART_SUFFIX) {
            fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    })
}

fn unlisted_files(install_dir: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let listed: HashSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut unlisted = Vec::new();
    walk_files(install_dir, install_dir, &mut |path| {
        let relative = path
            .strip_prefix(install_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        if !listed.contains(relative.as_str()) {
            unlisted.push(relative);
        }
        Ok(())
    })?;
    unlisted.sort();
    Ok(unlisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Chunk;

    #[test]
    fn crc64_redis_check_value() {
        assert_eq!(
            crc_fast::checksum(CrcAlgorithm::Crc64Redis, b"123456789"),
            0xe9c6d914c4b8d9ca
        );
    }

    #[test]
    fn xxh3_128_matches_xxhsum() {
        // `printf 123456789 | xxhsum -H2`
        let mut hasher = Xxh3::new();
        hasher.update(b"123456789");
        assert_eq!(
            hash_hex(hasher.digest128()),
            "33119477ede5dcd5e9716427681d5860"
        );
    }

    /// Builds bundles the way the server lays them out (independent zstd frames, back to
    /// back) and a manifest entry pointing into them, including a bundle shared by two files.
    fn fixture(dir: &Path) -> (FileEntry, FileEntry) {
        let pieces: [&[u8]; 3] = [b"hello, ", b"monsters and ", b"memories"];
        let mut shared = Vec::new();
        let mut chunks = Vec::new();
        let mut offset = 0;
        for piece in pieces {
            let frame = zstd::bulk::compress(piece, 3).unwrap();
            chunks.push(Chunk {
                offset,
                length: piece.len() as u64,
                crc: crc_fast::checksum(CrcAlgorithm::Crc64Redis, piece),
                bundle: 0,
                bundle_offset: shared.len() as u64,
                length_in_bundle: frame.len() as u64,
            });
            offset += piece.len() as u64;
            shared.extend_from_slice(&frame);
        }
        let shared_crc = crc_fast::checksum(CrcAlgorithm::Crc64Redis, &shared);
        for c in &mut chunks {
            c.bundle = shared_crc;
        }
        fs::write(dir.join(manifest::bundle_file_name(shared_crc)), &shared).unwrap();

        let whole: Vec<u8> = pieces.concat();
        let hash = xxhash_rust::xxh3::xxh3_128(&whole);
        let first = FileEntry {
            path: "a/first.bin".into(),
            hash,
            size: whole.len() as u64,
            chunks: chunks.clone(),
        };
        // A second file reusing the last chunk of the same bundle.
        let last = *chunks.last().unwrap();
        let second = FileEntry {
            path: "second.bin".into(),
            hash: xxhash_rust::xxh3::xxh3_128(b"memories"),
            size: last.length,
            chunks: vec![Chunk { offset: 0, ..last }],
        };
        (first, second)
    }

    #[test]
    fn assembles_files_from_shared_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = tmp.path().join(BUNDLES_DIR);
        fs::create_dir(&bundles).unwrap();
        let (first, second) = fixture(&bundles);

        let mut assembler = Assembler::new().unwrap();
        assembler.assemble(tmp.path(), &bundles, &first).unwrap();
        assembler.assemble(tmp.path(), &bundles, &second).unwrap();
        assert_eq!(
            fs::read(tmp.path().join("a/first.bin")).unwrap(),
            b"hello, monsters and memories"
        );
        assert_eq!(
            fs::read(tmp.path().join("second.bin")).unwrap(),
            b"memories"
        );
        assert!(!part_path(&tmp.path().join("second.bin")).exists());
    }

    #[test]
    fn corrupt_chunk_leaves_existing_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = tmp.path().join(BUNDLES_DIR);
        fs::create_dir(&bundles).unwrap();
        let (mut first, _) = fixture(&bundles);
        first.chunks[1].crc ^= 1;
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/first.bin"), b"old contents").unwrap();

        let err = Assembler::new()
            .unwrap()
            .assemble(tmp.path(), &bundles, &first)
            .unwrap_err();
        assert!(format!("{err:#}").contains("CRC"), "{err:#}");
        assert_eq!(
            fs::read(tmp.path().join("a/first.bin")).unwrap(),
            b"old contents"
        );
        assert!(!part_path(&tmp.path().join("a/first.bin")).exists());
    }

    #[test]
    fn plan_orders_bundles_by_first_use_and_counts_users() {
        let chunk = |bundle: u64, end: u64| Chunk {
            offset: 0,
            length: 1,
            crc: 0,
            bundle,
            bundle_offset: end - 1,
            length_in_bundle: 1,
        };
        let a = FileEntry {
            path: "a".into(),
            hash: 0,
            size: 1,
            chunks: vec![chunk(7, 10), chunk(3, 4)],
        };
        let b = FileEntry {
            path: "b".into(),
            hash: 0,
            size: 1,
            chunks: vec![chunk(3, 9), chunk(5, 2)],
        };
        let plan = BundlePlan::new(&[&a, &b]);
        assert_eq!(plan.order, [7, 3, 5]);
        assert_eq!(plan.users[&3], 2);
        assert_eq!(plan.sizes[&3], 9);
        assert_eq!(plan.total_bytes, 10 + 9 + 2);
    }

    #[test]
    fn clear_install_removes_listed_and_recorded_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let manifest = Manifest {
            files: vec![FileEntry {
                path: "one.bin".into(),
                hash: 0,
                size: 1,
                chunks: Vec::new(),
            }],
        };
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join(BUNDLES_DIR)).unwrap();
        for f in ["one.bin", "sub/old.bin", "game.db", ".bundles/x.bundle"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let db = Db::open_in_memory().unwrap();
        let key = root.display().to_string();
        db.begin_patch(&key, "mnm", 1).unwrap();
        let meta = fs::metadata(root.join("game.db")).unwrap();
        db.record_verified_file(&key, "sub/old.bin", &verified_file("00", &meta))
            .unwrap();

        clear_install(&db, &key, root, &manifest).unwrap();

        assert!(!root.join("one.bin").exists());
        assert!(!root.join("sub/old.bin").exists());
        assert!(!root.join(BUNDLES_DIR).exists());
        assert!(root.join("game.db").exists());
        assert!(db.verified_files(&key).unwrap().is_empty());
    }

    #[test]
    fn lists_unlisted_files_but_skips_bundle_cache() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(BUNDLES_DIR)).unwrap();
        fs::write(tmp.path().join(BUNDLES_DIR).join("x.bin"), b"").unwrap();
        fs::write(tmp.path().join("mnm.exe"), b"").unwrap();
        fs::create_dir_all(tmp.path().join("Screenshots")).unwrap();
        fs::write(tmp.path().join("Screenshots/shot.png"), b"").unwrap();
        let manifest = Manifest {
            files: vec![FileEntry {
                path: "mnm.exe".into(),
                hash: 0,
                size: 0,
                chunks: vec![],
            }],
        };
        assert_eq!(
            unlisted_files(tmp.path(), &manifest).unwrap(),
            ["Screenshots/shot.png"]
        );
    }
}
