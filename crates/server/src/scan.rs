//! Shared filesystem, BIG archive, and W3D workspace scanning.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use postcard::ser_flavors::Flavor;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, object_models_in, object_parents_in, references_in,
    AnimationAsset, AssetKind, Definition, FileAsset, ModelAsset, ModuleTagDefinition,
    ReferenceSite,
};
use zerosyntax_analysis::Analyzer;
use zerosyntax_w3d::W3dFile;

use crate::cache::{self, Fingerprint, InputCache};

pub(crate) type ScanEntry = (
    String,
    Vec<Definition>,
    Vec<ReferenceSite>,
    Vec<ModuleTagDefinition>,
    Vec<(String, Vec<String>)>,
    Vec<(String, String)>,
    Vec<ModelAsset>,
    Vec<FileAsset>,
    Option<Arc<str>>,
    Vec<AnimationAsset>,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanProgress {
    Discovering,
    InputsDiscovered {
        total: usize,
        skipped: usize,
    },
    Indexing {
        done: usize,
        total: usize,
        cache_hits: usize,
        cache_misses: usize,
    },
    WritingCache,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanStats {
    pub(crate) discovered_inputs: usize,
    pub(crate) discovery_failures: usize,
    pub(crate) cache_hits: usize,
    pub(crate) cache_misses: usize,
    pub(crate) fingerprint_failures: usize,
    pub(crate) scan_failures: usize,
    /// A usable persistent cache exists after the scan. This is also true
    /// when an unchanged cache did not need to be written again.
    pub(crate) cache_written: bool,
    /// The persistent cache was created or replaced during this scan.
    pub(crate) cache_updated: bool,
}

impl ScanStats {
    pub(crate) fn skipped_inputs(self) -> usize {
        self.discovery_failures + self.fingerprint_failures + self.scan_failures
    }
}

pub(crate) struct ScanOutcome {
    pub(crate) entries: Vec<(bool, ScanEntry)>,
    pub(crate) stats: ScanStats,
}

const MAX_PREVIEW_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHE_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

struct BoundedVec {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedVec {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn grow_to_fit(&mut self, required: usize) -> postcard::Result<()> {
        // Grow geometrically for the ordinary hot path, but never request
        // capacity beyond the configured payload envelope. The small initial
        // allocation avoids retaining 4 KiB for every tiny cached input.
        let target = self
            .bytes
            .capacity()
            .saturating_mul(2)
            .max(256)
            .max(required)
            .min(self.limit);
        self.bytes
            .try_reserve_exact(target - self.bytes.len())
            .map_err(|_| postcard::Error::SerializeBufferFull)?;
        Ok(())
    }
}

impl Flavor for BoundedVec {
    type Output = Vec<u8>;

    fn try_push(&mut self, byte: u8) -> postcard::Result<()> {
        if self.bytes.len() == self.limit {
            return Err(postcard::Error::SerializeBufferFull);
        }
        if self.bytes.len() == self.bytes.capacity() {
            self.grow_to_fit(self.bytes.len() + 1)?;
        }
        self.bytes.push(byte);
        Ok(())
    }

    fn try_extend(&mut self, bytes: &[u8]) -> postcard::Result<()> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(postcard::Error::SerializeBufferFull);
        }
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() {
            self.grow_to_fit(required)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finalize(self) -> postcard::Result<Self::Output> {
        Ok(self.bytes)
    }
}

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    file: String,
    definitions: Vec<Definition>,
    references: Vec<ReferenceSite>,
    tags: Vec<ModuleTagDefinition>,
    object_models: Vec<(String, Vec<String>)>,
    object_parents: Vec<(String, String)>,
    models: Vec<ModelAsset>,
    animations: Vec<AnimationAsset>,
    assets: Vec<FileAsset>,
    text: Option<String>,
}

impl From<&ScanEntry> for CachedEntry {
    fn from(entry: &ScanEntry) -> Self {
        Self {
            file: entry.0.clone(),
            definitions: entry.1.clone(),
            references: entry.2.clone(),
            tags: entry.3.clone(),
            object_models: entry.4.clone(),
            object_parents: entry.5.clone(),
            models: entry.6.clone(),
            animations: entry.9.clone(),
            assets: entry.7.clone(),
            text: entry.8.as_deref().map(str::to_owned),
        }
    }
}

impl From<CachedEntry> for ScanEntry {
    fn from(entry: CachedEntry) -> Self {
        (
            entry.file,
            entry.definitions,
            entry.references,
            entry.tags,
            entry.object_models,
            entry.object_parents,
            entry.models,
            entry.assets,
            entry.text.map(Arc::from),
            entry.animations,
        )
    }
}

fn serialize_cached(entries: &[CachedEntry]) -> Result<Vec<u8>> {
    serialize_bounded(entries, MAX_CACHE_PAYLOAD_BYTES)
        .context("failed to encode cache payload within 256 MiB limit")
}

fn serialize_bounded<T: Serialize + ?Sized>(value: &T, limit: usize) -> postcard::Result<Vec<u8>> {
    postcard::serialize_with_flavor(value, BoundedVec::new(limit))
}

fn deserialize_cached(payload: &[u8]) -> Result<Vec<CachedEntry>> {
    if payload.len() > MAX_CACHE_PAYLOAD_BYTES {
        anyhow::bail!("cache payload exceeds 256 MiB");
    }
    postcard::from_bytes(payload).context("failed to decode cache payload")
}

fn cache_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("zerosyntax");
    }
    #[cfg(not(windows))]
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("zerosyntax");
    }
    std::env::temp_dir().join("zerosyntax")
}

fn path_key(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Fingerprint {
        len: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    })
}

pub(crate) fn index_cache_path() -> PathBuf {
    cache::cache_path(&cache_dir())
}

pub(crate) fn clear_index_cache() -> Result<bool> {
    cache::clear(&cache_dir())
}

struct BigEntry {
    name: String,
    offset: u64,
    size: usize,
}

/// Read a file leniently: real INIs predate UTF-8 (Windows-1252 comments).
pub(crate) fn read_lossy(path: &Path) -> Result<String> {
    std::fs::read(path)
        .with_context(|| format!("failed to read {}", path.display()))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Load key names from an INI file's optional sibling Generals `.str` file.
pub(crate) fn load_sibling_str_keys(ini_url: &Url) -> Vec<String> {
    let Ok(path) = ini_url.to_file_path() else {
        return Vec::new();
    };
    for path in [path.with_extension("str"), path.with_extension("STR")] {
        if let Ok(text) = read_lossy(&path) {
            return parse_str_keys(&text);
        }
    }
    Vec::new()
}

fn parse_str_keys(content: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut expect_key = true;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END") {
            expect_key = true;
        } else if expect_key {
            keys.push(trimmed.to_string());
            expect_key = false;
        }
    }
    keys
}

fn read_u32_be<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn read_c_string<R: Read>(reader: &mut R) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte)?;
        if byte[0] == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn big_entries(path: &Path) -> Result<Vec<BigEntry>> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open BIG archive {}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("failed to read BIG archive {}", path.display()))?;
    if &magic != b"BIGF" {
        anyhow::bail!("{} is not a BIGF archive", path.display());
    }

    let _archive_size = read_u32_be(&mut file)?;
    let count = read_u32_be(&mut file)?;
    file.seek(SeekFrom::Start(0x10))?;

    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let offset = read_u32_be(&mut file)? as u64;
        let size = read_u32_be(&mut file)? as usize;
        let name = read_c_string(&mut file)?.replace('\\', "/");
        entries.push(BigEntry { name, offset, size });
    }
    Ok(entries)
}

fn read_big_entry_bytes(path: &Path, entry: &BigEntry) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(entry.offset))?;
    let mut bytes = vec![0; entry.size];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn big_uri(path: &Path, entry: &str) -> String {
    let archive = path.to_string_lossy().replace('\\', "/");
    let mut uri = Url::parse("big:///").expect("static BIG URI is valid");
    uri.set_path(&format!("{archive}!/{entry}"));
    uri.to_string()
}

fn file_stem_str(path: &str) -> String {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .to_string()
}

fn raw_asset(path: &str, uri: &str) -> Option<FileAsset> {
    let name = path.rsplit(['/', '\\']).next()?;
    let (_, extension) = name.rsplit_once('.')?;
    let kind = if extension.eq_ignore_ascii_case("wav") || extension.eq_ignore_ascii_case("mp3") {
        AssetKind::Audio
    } else if extension.eq_ignore_ascii_case("tga") || extension.eq_ignore_ascii_case("dds") {
        AssetKind::Texture
    } else {
        return None;
    };
    Some(FileAsset {
        kind,
        name: name.to_string(),
        uri: uri.to_string(),
    })
}

#[cfg(test)]
pub(crate) fn parse_w3d_models(bytes: &[u8], fallback_name: &str) -> Vec<ModelAsset> {
    parse_w3d_assets(bytes, fallback_name).0
}

fn parse_w3d_assets(bytes: &[u8], fallback_name: &str) -> (Vec<ModelAsset>, Vec<AnimationAsset>) {
    match W3dFile::parse(bytes) {
        Ok(file) => (
            file.catalog(fallback_name)
                .into_iter()
                .map(|model| ModelAsset {
                    name: model.name,
                    members: model.members,
                    hierarchy: model.hierarchy,
                })
                .collect(),
            file.animations()
                .iter()
                .map(|animation| AnimationAsset {
                    name: animation.name.clone(),
                    hierarchy: animation.hierarchy.clone(),
                })
                .collect(),
        ),
        Err(_) if !fallback_name.trim().is_empty() => (
            vec![ModelAsset {
                name: fallback_name.trim().to_string(),
                members: Vec::new(),
                hierarchy: None,
            }],
            Vec::new(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

pub(crate) fn scan_big(analyzer: &Analyzer, path: &Path) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    let mut assets = Vec::new();
    for entry in big_entries(path)? {
        let file = big_uri(path, &entry.name);
        let extension = Path::new(&entry.name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("ini") {
            let bytes = read_big_entry_bytes(path, &entry).with_context(|| {
                format!("failed to read {} from {}", entry.name, path.display())
            })?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let parse = analyzer.parse(&text);
            out.push((
                file.clone(),
                definitions_in(analyzer, &parse, &file),
                references_in(analyzer, &parse),
                module_tags_in(analyzer, &parse),
                object_models_in(analyzer, &parse),
                object_parents_in(&parse),
                Vec::new(),
                Vec::new(),
                Some(Arc::from(text)),
                Vec::new(),
            ));
        } else if extension.eq_ignore_ascii_case("w3d") {
            let bytes = read_big_entry_bytes(path, &entry).with_context(|| {
                format!("failed to read {} from {}", entry.name, path.display())
            })?;
            let (models, animations) = parse_w3d_assets(&bytes, &file_stem_str(&entry.name));
            if !models.is_empty() || !animations.is_empty() {
                out.push((
                    file,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    models,
                    Vec::new(),
                    None,
                    animations,
                ));
            }
        } else if let Some(asset) = raw_asset(&entry.name, &file) {
            assets.push(asset);
        }
    }
    assets.sort_by(|left, right| {
        file_stem_str(&left.name)
            .to_ascii_lowercase()
            .cmp(&file_stem_str(&right.name).to_ascii_lowercase())
            .then_with(|| {
                let rank = |name: &str| {
                    if name
                        .rsplit_once('.')
                        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("dds"))
                    {
                        1
                    } else {
                        0
                    }
                };
                rank(&left.name).cmp(&rank(&right.name))
            })
    });
    if !assets.is_empty() {
        out.push((
            big_uri(path, "__assets__"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            assets,
            None,
            Vec::new(),
        ));
    }
    Ok(out)
}

struct DiscoveryOutcome {
    paths: Vec<PathBuf>,
    skipped: usize,
}

/// Best-effort discovery used by tests and helpers that do not need the
/// skipped-entry count. Interactive indexing consumes `collect_paths`
/// directly so inaccessible configured inputs remain visible in ScanStats.
#[cfg(test)]
pub(crate) fn collect_scan_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    collect_paths(roots, false)
        .map(|outcome| outcome.paths)
        .unwrap_or_default()
}

/// Checked discovery used by the CLI, where skipped inputs must fail visibly.
pub(crate) fn collect_scan_paths_checked(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    collect_paths(roots, true).map(|outcome| outcome.paths)
}

fn collect_paths(roots: &[PathBuf], checked: bool) -> Result<DiscoveryOutcome> {
    let mut out = Vec::new();
    let mut skipped = 0;
    for root in roots {
        let root_start = out.len();
        if root.is_file()
            && root
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("big"))
        {
            out.push(root.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if checked => return Err(error.into()),
                Err(error) => {
                    skipped += 1;
                    tracing::debug!(root = %root.display(), %error, "workspace walk entry skipped");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("big")
                || ext.eq_ignore_ascii_case("ini")
                || ext.eq_ignore_ascii_case("w3d")
                || ext.eq_ignore_ascii_case("wav")
                || ext.eq_ignore_ascii_case("mp3")
                || ext.eq_ignore_ascii_case("tga")
                || ext.eq_ignore_ascii_case("dds")
            {
                out.push(path.to_path_buf());
            }
        }
        out[root_start..].sort_by(|left, right| {
            let left_stem = left
                .with_extension("")
                .to_string_lossy()
                .to_ascii_lowercase();
            let right_stem = right
                .with_extension("")
                .to_string_lossy()
                .to_ascii_lowercase();
            left_stem.cmp(&right_stem).then_with(|| {
                let rank = |path: &Path| {
                    path.extension()
                        .and_then(|value| value.to_str())
                        .map_or(0, |extension| {
                            if extension.eq_ignore_ascii_case("dds") {
                                1
                            } else {
                                0
                            }
                        })
                };
                rank(left).cmp(&rank(right))
            })
        });
    }
    if skipped > 0 {
        tracing::warn!(
            skipped_count = skipped,
            "workspace walk skipped inaccessible entries"
        );
    }
    Ok(DiscoveryOutcome {
        paths: out,
        skipped,
    })
}

/// Best-effort indexing used by the interactive server.
#[cfg(test)]
pub(crate) fn scan_files(
    analyzer: &Analyzer,
    paths: &[PathBuf],
    progress: &mut impl FnMut(usize, usize),
) -> Vec<ScanEntry> {
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        if let Ok(mut entries) = scan_path(analyzer, path) {
            out.append(&mut entries);
        }
        progress(i + 1, paths.len());
    }
    out
}

/// Scan workspace and base roots, reusing unchanged files from the persistent
/// asset index cache. Base entries stay first so workspace definitions retain
/// their existing override order.
pub(crate) fn scan_with_cache(
    analyzer: &Analyzer,
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
    progress: &mut impl FnMut(ScanProgress),
) -> ScanOutcome {
    scan_with_cache_in(
        analyzer,
        workspace_roots,
        base_roots,
        &cache_dir(),
        progress,
    )
}

fn scan_with_cache_in(
    analyzer: &Analyzer,
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
    cache_dir: &Path,
    progress: &mut impl FnMut(ScanProgress),
) -> ScanOutcome {
    let started = Instant::now();
    progress(ScanProgress::Discovering);
    let cache_path = cache::cache_path(cache_dir);
    let mut persistent_cache = cache::producer_id(analyzer)
        .and_then(|producer| InputCache::open(cache_dir, producer))
        .map_err(|error| {
            tracing::warn!(path = %cache_path.display(), %error, "persistent input cache unavailable");
            error
        })
        .ok();

    let mut discovered_inputs = 0;
    let mut discovery_failures = 0;
    let mut fingerprint_failures = 0;
    let mut cache_hits = 0;
    let mut cache_misses = 0;
    let mut scan_failures = 0;
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for (roots, is_base) in [(base_roots, true), (workspace_roots, false)] {
        let discovery = collect_paths(roots, false)
            .expect("best-effort discovery converts walk errors into skipped entries");
        discovery_failures += discovery.skipped;
        for path in discovery.paths {
            let key = path_key(&path);
            if seen.insert(key.clone()) {
                discovered_inputs += 1;
                if let Some(fingerprint) = fingerprint(&path) {
                    paths.push((path, key, fingerprint, is_base));
                } else {
                    fingerprint_failures += 1;
                    tracing::debug!(
                        path = %path.display(),
                        "workspace input skipped because it could not be fingerprinted"
                    );
                }
            }
        }
    }
    progress(ScanProgress::InputsDiscovered {
        total: paths.len(),
        skipped: discovery_failures + fingerprint_failures,
    });

    let mut scanned = Vec::new();
    let total = paths.len();
    for (done, (path, key, fingerprint, is_base)) in paths.into_iter().enumerate() {
        let cached_entries = if let Some(cache) = persistent_cache.as_mut() {
            match cache.lookup(&key, &fingerprint) {
                Ok(Some(payload)) => match deserialize_cached(&payload) {
                    Ok(entries) => Some(entries.into_iter().map(ScanEntry::from).collect()),
                    Err(error) => {
                        cache.invalidate(&key);
                        tracing::debug!(path = %path.display(), %error, "cached input payload is corrupt");
                        None
                    }
                },
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(path = %cache_path.display(), %error, "persistent input cache lookup failed; continuing uncached");
                    persistent_cache = None;
                    None
                }
            }
        } else {
            None
        };

        let entries = if let Some(entries) = cached_entries {
            cache_hits += 1;
            entries
        } else {
            cache_misses += 1;
            match scan_path_stable(analyzer, &path, fingerprint) {
                Ok((fingerprint, entries)) => {
                    if let Some(cache) = persistent_cache.as_mut() {
                        let cached: Vec<_> = entries.iter().map(CachedEntry::from).collect();
                        match serialize_cached(&cached) {
                            Ok(payload) => cache.queue_store(key, fingerprint, payload),
                            Err(error) => tracing::debug!(
                                path = %path.display(),
                                %error,
                                "workspace input could not be serialized for caching"
                            ),
                        }
                    }
                    entries
                }
                Err(error) => {
                    scan_failures += 1;
                    tracing::debug!(path = %path.display(), %error, "workspace input could not be indexed");
                    Vec::new()
                }
            }
        };
        scanned.extend(entries.into_iter().map(|entry| (is_base, entry)));
        progress(ScanProgress::Indexing {
            done: done + 1,
            total,
            cache_hits,
            cache_misses,
        });
    }
    let mut cache_written = persistent_cache.is_some();
    let mut cache_updated = false;
    let mut pruned_cache_records = 0;
    if let Some(cache) = persistent_cache.as_mut() {
        if cache.has_pending_writes() {
            progress(ScanProgress::WritingCache);
        }
        match cache.commit() {
            Ok(outcome) => {
                cache_updated = outcome.updated;
                pruned_cache_records = outcome.pruned;
            }
            Err(error) => {
                cache_written = false;
                tracing::warn!(path = %cache_path.display(), %error, "persistent input cache commit failed");
            }
        }
    }
    let skipped_count = fingerprint_failures + scan_failures;
    if skipped_count > 0 {
        tracing::warn!(
            skipped_count,
            fingerprint_failures,
            scan_failures,
            "workspace indexing skipped inputs"
        );
    }
    tracing::debug!(
        path = %cache_path.display(),
        discovered_inputs,
        discovery_failures,
        cache_hits,
        cache_misses,
        reparsed_files = cache_misses,
        fingerprint_failures,
        scan_failures,
        produced_entries = scanned.len(),
        cache_written,
        pruned_cache_records,
        cache_updated,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "workspace scan completed"
    );
    ScanOutcome {
        entries: scanned,
        stats: ScanStats {
            discovered_inputs,
            discovery_failures,
            cache_hits,
            cache_misses,
            fingerprint_failures,
            scan_failures,
            cache_written,
            cache_updated,
        },
    }
}

pub(crate) fn scan_files_checked(analyzer: &Analyzer, paths: &[PathBuf]) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    for path in paths {
        out.extend(scan_path(analyzer, path)?);
    }
    Ok(out)
}

fn scan_path(analyzer: &Analyzer, path: &Path) -> Result<Vec<ScanEntry>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("big") {
        return scan_big(analyzer, path);
    }

    let uri = Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("cannot convert {} to a file URI", path.display()))?;
    if ext.eq_ignore_ascii_case("ini") {
        let text = read_lossy(path)?;
        let parse = analyzer.parse(&text);
        Ok(vec![(
            uri.to_string(),
            definitions_in(analyzer, &parse, uri.as_str()),
            references_in(analyzer, &parse),
            module_tags_in(analyzer, &parse),
            object_models_in(analyzer, &parse),
            object_parents_in(&parse),
            Vec::new(),
            Vec::new(),
            None,
            Vec::new(),
        )])
    } else if ext.eq_ignore_ascii_case("w3d") {
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let (models, animations) = parse_w3d_assets(&bytes, stem);
        Ok((!models.is_empty() || !animations.is_empty())
            .then_some((
                uri.to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                models,
                Vec::new(),
                None,
                animations,
            ))
            .into_iter()
            .collect())
    } else if let Some(asset) = raw_asset(&path.to_string_lossy(), uri.as_str()) {
        Ok(vec![(
            uri.to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![asset],
            None,
            Vec::new(),
        )])
    } else {
        Ok(Vec::new())
    }
}

/// Analyze one stable physical snapshot. A writer can replace an input after
/// discovery but before it is read; retry once so the payload is never stored
/// under a fingerprint for different bytes.
fn scan_path_stable(
    analyzer: &Analyzer,
    path: &Path,
    mut expected: Fingerprint,
) -> Result<(Fingerprint, Vec<ScanEntry>)> {
    for _ in 0..2 {
        let entries = scan_path(analyzer, path)?;
        let observed = fingerprint(path)
            .with_context(|| format!("could not fingerprint {} after indexing", path.display()))?;
        if observed == expected {
            return Ok((observed, entries));
        }
        expected = observed;
    }
    anyhow::bail!("{} changed repeatedly while it was indexed", path.display())
}

pub(crate) fn read_asset_uri(uri: &str) -> Result<Vec<u8>> {
    let url = Url::parse(uri).with_context(|| format!("invalid asset URI `{uri}`"))?;
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file asset URI `{uri}`"))?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_PREVIEW_ASSET_BYTES {
            anyhow::bail!("asset exceeds 128 MiB");
        }
        return std::fs::read(&path)
            .with_context(|| format!("failed to read asset {}", path.display()));
    }
    if url.scheme() != "big" {
        anyhow::bail!("unsupported asset URI scheme `{}`", url.scheme());
    }
    let decoded = percent_decode_str(url.path()).decode_utf8_lossy();
    let (archive, entry_name) = decoded
        .split_once("!/")
        .ok_or_else(|| anyhow::anyhow!("invalid BIG asset URI `{uri}`"))?;
    let archive =
        if cfg!(windows) && archive.as_bytes().get(2) == Some(&b':') && archive.starts_with('/') {
            &archive[1..]
        } else {
            archive
        };
    let path = Path::new(archive);
    let entry = big_entries(path)?
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(entry_name))
        .ok_or_else(|| anyhow::anyhow!("asset `{entry_name}` not found in {}", path.display()))?;
    if entry.size as u64 > MAX_PREVIEW_ASSET_BYTES {
        anyhow::bail!("asset exceeds 128 MiB");
    }
    read_big_entry_bytes(path, &entry)
}

#[cfg(test)]
pub(crate) fn scan_roots(analyzer: &Analyzer, roots: &[PathBuf]) -> Vec<ScanEntry> {
    scan_files(analyzer, &collect_scan_paths(roots), &mut |_, _| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zerosyntax-{label}-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ))
    }

    fn comparable(entries: &[(bool, ScanEntry)]) -> Vec<(bool, Vec<u8>)> {
        entries
            .iter()
            .map(|(is_base, entry)| {
                (
                    *is_base,
                    postcard::to_stdvec(&CachedEntry::from(entry)).unwrap(),
                )
            })
            .collect()
    }

    fn write_big(path: &Path, entries: &[(&str, &[u8])]) {
        let data_offset = 0x10
            + entries
                .iter()
                .map(|(name, _)| 8 + name.len() + 1)
                .sum::<usize>();
        let archive_size = data_offset + entries.iter().map(|(_, data)| data.len()).sum::<usize>();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BIGF");
        bytes.extend_from_slice(&(archive_size as u32).to_be_bytes());
        bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let mut offset = data_offset;
        for (name, data) in entries {
            bytes.extend_from_slice(&(offset as u32).to_be_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            offset += data.len();
        }
        for (_, data) in entries {
            bytes.extend_from_slice(data);
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn warm_scan_is_identical_and_does_not_rewrite_payloads() {
        let root = unique_temp_dir("warm-cache");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("Weapon.ini"), "Weapon TestWeapon\nEnd\n").unwrap();
        let workspace_roots = vec![workspace];

        let cold = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(cold.stats.cache_misses, 1);
        assert!(cold.stats.cache_updated);

        let mut warm_events = Vec::new();
        let warm = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |event| warm_events.push(event),
        );
        assert_eq!(warm.stats.cache_hits, 1);
        assert_eq!(warm.stats.cache_misses, 0);
        assert!(warm.stats.cache_written);
        assert!(!warm.stats.cache_updated);
        assert!(!warm_events.contains(&ScanProgress::WritingCache));
        assert_eq!(comparable(&cold.entries), comparable(&warm.entries));

        std::fs::write(
            workspace_roots[0].join("Weapon.ini"),
            "Weapon UpdatedWeapon\nEnd\n",
        )
        .unwrap();
        let changed = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(changed.stats.cache_hits, 0);
        assert_eq!(changed.stats.cache_misses, 1);
        assert!(changed.stats.cache_updated);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_serialization_stops_at_the_payload_limit() {
        let value = vec![7u8; 128];
        assert_eq!(
            serialize_bounded(&value, 64).unwrap_err(),
            postcard::Error::SerializeBufferFull
        );

        let payload = serialize_bounded(&value, 256).unwrap();
        assert_eq!(postcard::from_bytes::<Vec<u8>>(&payload).unwrap(), value);
    }

    #[test]
    fn big_archive_is_one_cached_input_with_many_virtual_entries() {
        let root = unique_temp_dir("big-cache");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut animation = vec![0; 44];
        animation[4..7].copy_from_slice(b"Run");
        animation[20..28].copy_from_slice(b"HumanSKL");
        let animation = [
            0x200u32.to_le_bytes().to_vec(),
            52u32.to_le_bytes().to_vec(),
            0x201u32.to_le_bytes().to_vec(),
            44u32.to_le_bytes().to_vec(),
            animation,
        ]
        .concat();
        write_big(
            &workspace.join("Data.big"),
            &[
                ("Data\\INI\\Object.ini", b"Object BigObject\nEnd\n"),
                ("Data\\INI\\Weapon.ini", b"Weapon BigWeapon\nEnd\n"),
                ("Art\\W3D\\Run.w3d", &animation),
            ],
        );
        let workspace_roots = vec![workspace];

        let cold = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(cold.stats.cache_misses, 1);
        assert_eq!(cold.entries.len(), 3);
        let animation_entry = cold
            .entries
            .iter()
            .find(|(_, entry)| !entry.9.is_empty())
            .unwrap();
        assert!(
            animation_entry.1 .6.is_empty(),
            "animation-only file is not a model"
        );
        assert_eq!(animation_entry.1 .9[0].name, "HumanSKL.Run");

        let warm = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(warm.stats.cache_hits, 1);
        assert_eq!(warm.stats.cache_misses, 0);
        assert_eq!(comparable(&cold.entries), comparable(&warm.entries));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_base_roots_are_reused_across_workspaces() {
        let root = unique_temp_dir("cross-workspace-cache");
        let cache_dir = root.join("cache");
        let base_root = root.join("base");
        let workspace_a = root.join("workspace-a");
        let workspace_b = root.join("workspace-b");
        for directory in [&base_root, &workspace_a, &workspace_b] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(base_root.join("Weapon.ini"), "Weapon BaseWeapon\nEnd\n").unwrap();
        std::fs::write(workspace_a.join("Object.ini"), "Object WorkspaceA\nEnd\n").unwrap();
        std::fs::write(workspace_b.join("Object.ini"), "Object WorkspaceB\nEnd\n").unwrap();
        let base_roots = vec![base_root.clone()];
        let workspace_a_roots = vec![workspace_a];
        let workspace_b_roots = vec![workspace_b];

        let cold = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_a_roots,
            &base_roots,
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(cold.stats.cache_hits, 0);
        assert_eq!(cold.stats.cache_misses, 2);

        let switched = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_b_roots,
            &base_roots,
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(
            switched.stats.cache_hits, 1,
            "the unchanged base file should be reused from workspace A's cache"
        );
        assert_eq!(
            switched.stats.cache_misses, 1,
            "only workspace B's file should need indexing"
        );

        let added_base_root = root.join("base-added-later");
        std::fs::create_dir_all(&added_base_root).unwrap();
        std::fs::write(added_base_root.join("Armor.ini"), "Armor TestArmor\nEnd\n").unwrap();
        let extended_base_roots = vec![base_root.clone(), added_base_root];
        let extended = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_b_roots,
            &extended_base_roots,
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(
            extended.stats.cache_hits, 2,
            "the existing base and workspace files should be reused"
        );
        assert_eq!(
            extended.stats.cache_misses, 1,
            "only the newly configured base file should need indexing"
        );

        std::fs::write(
            base_root.join("Weapon.ini"),
            "Weapon UpdatedBaseWeapon\nEnd\n",
        )
        .unwrap();
        let changed = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_a_roots,
            &base_roots,
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(
            changed.stats.cache_hits, 1,
            "the unchanged workspace file should still be reusable"
        );
        assert_eq!(
            changed.stats.cache_misses, 1,
            "a modified base file must not be reused"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_schema_identity_separates_cached_analysis() {
        let root = unique_temp_dir("schema-cache");
        let cache_dir = root.join("cache");
        let base_root = root.join("base");
        let workspace = root.join("workspace");
        for directory in [&base_root, &workspace] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(base_root.join("Weapon.ini"), "Weapon BaseWeapon\nEnd\n").unwrap();
        std::fs::write(workspace.join("Object.ini"), "Object Workspace\nEnd\n").unwrap();
        let base_roots = vec![base_root];
        let workspace_roots = vec![workspace];

        scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &base_roots,
            &cache_dir,
            &mut |_| {},
        );
        let mut schema = zerosyntax_schema::embedded();
        schema.engine_revision.push_str("-custom");
        let custom = Analyzer::new(schema);
        let rescanned = scan_with_cache_in(
            &custom,
            &workspace_roots,
            &base_roots,
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(rescanned.stats.cache_hits, 0);
        assert_eq!(rescanned.stats.cache_misses, 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_role_is_taken_from_the_current_scan_plan() {
        let root = unique_temp_dir("cache-role");
        let cache_dir = root.join("cache");
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("Weapon.ini"), "Weapon SharedWeapon\nEnd\n").unwrap();

        let base = scan_with_cache_in(
            &Analyzer::embedded(),
            &[],
            std::slice::from_ref(&shared),
            &cache_dir,
            &mut |_| {},
        );
        assert!(base.entries.iter().all(|(is_base, _)| *is_base));

        let workspace = scan_with_cache_in(
            &Analyzer::embedded(),
            std::slice::from_ref(&shared),
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(workspace.stats.cache_hits, 1);
        assert!(workspace.entries.iter().all(|(is_base, _)| !*is_base));
        assert_eq!(
            comparable(&base.entries)
                .into_iter()
                .map(|(_, payload)| payload)
                .collect::<Vec<_>>(),
            comparable(&workspace.entries)
                .into_iter()
                .map(|(_, payload)| payload)
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_payloads_follow_current_root_order_and_overlap_deduplication() {
        let root = unique_temp_dir("cache-order");
        let cache_dir = root.join("cache");
        let first = root.join("first");
        let nested = first.join("nested");
        let second = root.join("second");
        for directory in [&nested, &second] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(first.join("First.ini"), "Object FirstObject\nEnd\n").unwrap();
        std::fs::write(nested.join("Nested.ini"), "Object NestedObject\nEnd\n").unwrap();
        std::fs::write(second.join("Second.ini"), "Object SecondObject\nEnd\n").unwrap();

        let cold = scan_with_cache_in(
            &Analyzer::embedded(),
            &[],
            &[first.clone(), second.clone(), nested.clone()],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(cold.stats.discovered_inputs, 3);
        assert_eq!(cold.entries.len(), 3, "nested overlap is emitted once");

        let reordered = scan_with_cache_in(
            &Analyzer::embedded(),
            &[],
            &[second, first, nested],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(reordered.stats.cache_hits, 3);
        assert_eq!(reordered.entries.len(), 3);
        let files: Vec<_> = reordered
            .entries
            .iter()
            .map(|(_, entry)| entry.0.as_str())
            .collect();
        assert!(files[0].ends_with("Second.ini"));
        assert!(files[1].ends_with("First.ini"));
        assert!(files[2].ends_with("Nested.ini"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_physical_input_is_retried_instead_of_cached_empty() {
        let root = unique_temp_dir("failed-input");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("Broken.big"), b"not a BIG archive").unwrap();
        let workspace_roots = vec![workspace];

        for _ in 0..2 {
            let outcome = scan_with_cache_in(
                &Analyzer::embedded(),
                &workspace_roots,
                &[],
                &cache_dir,
                &mut |_| {},
            );
            assert_eq!(outcome.stats.cache_hits, 0);
            assert_eq!(outcome.stats.cache_misses, 1);
            assert_eq!(outcome.stats.scan_failures, 1);
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_payload_invalidates_only_that_input() {
        let root = unique_temp_dir("corrupt-payload");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("Weapon.ini"), "Weapon TestWeapon\nEnd\n").unwrap();
        std::fs::write(workspace.join("Object.ini"), "Object TestObject\nEnd\n").unwrap();
        let workspace_roots = vec![workspace];

        scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        let connection = rusqlite::Connection::open(cache::cache_path(&cache_dir)).unwrap();
        connection
            .execute(
                "UPDATE input_cache SET payload = X'FF' WHERE path LIKE '%weapon.ini'",
                [],
            )
            .unwrap();
        drop(connection);

        let recovered = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(recovered.stats.cache_hits, 1);
        assert_eq!(recovered.stats.cache_misses, 1);
        assert!(recovered.stats.cache_updated);

        let warm = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |_| {},
        );
        assert_eq!(warm.stats.cache_hits, 2);
        assert_eq!(warm.stats.cache_misses, 0);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn many_inputs_share_one_store_and_warm_without_payload_writes() {
        let root = unique_temp_dir("many-inputs");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        const INPUTS: usize = 256;
        for index in 0..INPUTS {
            std::fs::write(
                workspace.join(format!("Object{index:03}.ini")),
                format!("Object CachedObject{index:03}\nEnd\n"),
            )
            .unwrap();
        }
        let workspace_roots = vec![workspace];
        let analyzer = Analyzer::embedded();

        let cold_started = Instant::now();
        let cold = scan_with_cache_in(&analyzer, &workspace_roots, &[], &cache_dir, &mut |_| {});
        let cold_elapsed = cold_started.elapsed();
        assert_eq!(cold.stats.cache_misses, INPUTS);
        assert!(cold.stats.cache_updated);

        let warm_started = Instant::now();
        let warm = scan_with_cache_in(&analyzer, &workspace_roots, &[], &cache_dir, &mut |_| {});
        let warm_elapsed = warm_started.elapsed();
        assert_eq!(warm.stats.cache_hits, INPUTS);
        assert_eq!(warm.stats.cache_misses, 0);
        assert!(!warm.stats.cache_updated);
        assert_eq!(comparable(&cold.entries), comparable(&warm.entries));
        assert!(cache::cache_path(&cache_dir).is_file());
        eprintln!(
            "physical input cache: {INPUTS} inputs, cold={cold_elapsed:?}, warm={warm_elapsed:?}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_failures_reach_progress_and_scan_stats() {
        let missing = std::env::temp_dir().join(format!(
            "zerosyntax-missing-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let cache_dir = unique_temp_dir("missing-root-cache");
        assert!(!missing.exists());
        let workspace_roots = vec![missing];
        let mut events = Vec::new();
        let outcome = scan_with_cache_in(
            &Analyzer::embedded(),
            &workspace_roots,
            &[],
            &cache_dir,
            &mut |event| events.push(event),
        );

        assert_eq!(outcome.stats.discovered_inputs, 0);
        assert_eq!(outcome.stats.discovery_failures, 1);
        assert_eq!(outcome.stats.skipped_inputs(), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            ScanProgress::InputsDiscovered {
                total: 0,
                skipped: 1
            }
        )));

        std::fs::remove_dir_all(cache_dir).unwrap();
    }
}
