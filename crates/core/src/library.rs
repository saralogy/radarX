//! A library is the set of open datasets: the bundled disc plus side-loads.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use sd_zim::{Archive, Entry};
use serde::{Deserialize, Serialize};

use crate::html::{self, RenderOptions};
use crate::search::{query_terms, SearchIndex};
use crate::{CoreError, Result};

pub const DISC_MANIFEST: &str = "disc.json";
pub const INDEX_DIR: &str = "search-index";
/// Text fed to the snippet generator per result; bounds worst-case latency.
const SNIPPET_SOURCE_CHARS: usize = 20_000;

/// `disc.json`, written by `disc-builder` next to the ZIM files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscManifest {
    pub id: String,
    pub title: String,
    pub version: String,
    pub datasets: Vec<DatasetManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetManifest {
    pub id: String,
    pub name: String,
    pub file: String,
    pub license: String,
    pub attribution: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub license_file: String,
    pub articles: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct DatasetInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub language: String,
    pub license: String,
    pub attribution: String,
    pub article_count: u32,
    pub size_bytes: u64,
    /// Part of the purchased disc (not deletable) vs. user side-load.
    pub bundled: bool,
    /// Covered by the full-text index; otherwise title search only.
    pub full_text: bool,
    pub file_path: String,
}

struct Dataset {
    info: DatasetInfo,
    archive: Archive,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub mime: String,
    pub data: Vec<u8>,
    /// Canonical path after redirects.
    pub path: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub dataset_id: String,
    pub dataset_name: String,
    pub path: String,
    /// Percent-encoded path plus highlight query, ready to append to
    /// `<scheme>://<dataset_id>/`.
    pub url_path: String,
    pub title: String,
    /// HTML with `<b>` around matched terms; empty for title-only hits.
    pub snippet: String,
}

#[derive(Default)]
pub struct Library {
    sets: RwLock<Vec<Arc<Dataset>>>,
    index: Option<SearchIndex>,
    disc: Option<DiscManifest>,
}

fn is_html(mime: &str) -> bool {
    mime.starts_with("text/html") || mime.starts_with("application/xhtml")
}

impl Library {
    /// Empty library; datasets arrive through [`Library::add_archive`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a disc directory produced by `disc-builder`.
    pub fn open_disc(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let raw = std::fs::read(dir.join(DISC_MANIFEST))
            .map_err(|e| CoreError::Disc(format!("cannot read {DISC_MANIFEST}: {e}")))?;
        let disc: DiscManifest = serde_json::from_slice(&raw).map_err(|e| CoreError::Disc(e.to_string()))?;
        let index_dir = dir.join(INDEX_DIR);
        let index = if index_dir.is_dir() { Some(SearchIndex::open(&index_dir)?) } else { None };
        let mut sets = Vec::new();
        for d in &disc.datasets {
            if d.file.contains(['/', '\\']) || d.file.starts_with('.') {
                return Err(CoreError::Disc(format!("dataset file must be a plain name: {}", d.file)));
            }
            let path = dir.join(&d.file);
            let archive = Archive::open(&path)?;
            let mut info = describe(&archive, &d.id, &path);
            info.name = d.name.clone();
            info.license = d.license.clone();
            info.attribution = d.attribution.clone();
            info.bundled = true;
            info.full_text = index.is_some();
            sets.push(Arc::new(Dataset { info, archive }));
        }
        Ok(Self { sets: RwLock::new(sets), index, disc: Some(disc) })
    }

    pub fn disc(&self) -> Option<&DiscManifest> {
        self.disc.as_ref()
    }

    /// Side-load a ZIM by path. Returns the dataset it was registered as.
    pub fn add_archive(&self, path: impl AsRef<Path>) -> Result<DatasetInfo> {
        let path = path.as_ref();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.register(Archive::open(path)?, &stem, path)
    }

    /// Side-load from an open file (Android Storage Access Framework hands
    /// out descriptors, not paths).
    pub fn add_archive_file(&self, file: std::fs::File, name: &str) -> Result<DatasetInfo> {
        self.register(Archive::from_file(file)?, name, Path::new(""))
    }

    fn register(&self, archive: Archive, name: &str, path: &Path) -> Result<DatasetInfo> {
        let mut base: String =
            name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' }).collect();
        // Ids double as URL hosts on Android: one DNS label, so keep them short.
        base.truncate(40);
        let base = match base.trim_matches('-') {
            "" => "zim".to_string(),
            b => b.to_string(),
        };
        let mut sets = self.sets.write().unwrap();
        let mut id = base.clone();
        let mut n = 2;
        while sets.iter().any(|d| d.info.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let info = describe(&archive, &id, path);
        sets.push(Arc::new(Dataset { info: info.clone(), archive }));
        Ok(info)
    }

    /// Close a side-loaded dataset. Deleting the file is the caller's job.
    pub fn remove_archive(&self, dataset_id: &str) -> Result<()> {
        let mut sets = self.sets.write().unwrap();
        match sets.iter().position(|d| d.info.id == dataset_id) {
            Some(i) if sets[i].info.bundled => Err(CoreError::Blocked("bundled datasets cannot be removed".into())),
            Some(i) => {
                sets.remove(i);
                Ok(())
            }
            None => Err(CoreError::NotFound(dataset_id.into())),
        }
    }

    pub fn datasets(&self) -> Vec<DatasetInfo> {
        self.sets.read().unwrap().iter().map(|d| d.info.clone()).collect()
    }

    fn dataset(&self, id: &str) -> Result<Arc<Dataset>> {
        self.sets
            .read()
            .unwrap()
            .iter()
            .find(|d| d.info.id == id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("dataset {id}")))
    }

    /// Split the cluster-cache budget evenly across open datasets.
    pub fn set_cache_budget(&self, total_bytes: usize) {
        let sets = self.sets.read().unwrap();
        let each = total_bytes / sets.len().max(1);
        sets.iter().for_each(|d| d.archive.set_cache_budget(each));
    }

    /// Cover illustration (48x48 PNG) if the archive carries one.
    pub fn cover(&self, dataset_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.dataset(dataset_id)?.archive.metadata("Illustration_48x48@1")?)
    }

    /// Serve one archive entry. HTML is sanitised; scripts are refused.
    pub fn resource(&self, dataset_id: &str, path: &str, opts: &RenderOptions) -> Result<Resource> {
        let ds = self.dataset(dataset_id)?;
        let a = &ds.archive;
        let entry = match a.get(path)? {
            Some(e) => a.resolve(e)?,
            None => return Err(CoreError::NotFound(path.into())),
        };
        let mime = a.mime(&entry).to_string();
        if mime.contains("javascript") || mime.contains("ecmascript") {
            return Err(CoreError::Blocked(format!("script resource {path}")));
        }
        let mut data = a.read(&entry)?;
        let mut out_mime = mime.clone();
        if is_html(&mime) {
            data = html::sanitize(&String::from_utf8_lossy(&data), opts)?.into_bytes();
            out_mime = "text/html; charset=utf-8".into();
        }
        Ok(Resource { mime: out_mime, data, path: a.user_path(&entry), title: entry.display_title().to_string() })
    }

    /// Serve the path component of a WebView request URL (percent-encoded,
    /// optionally with the `?sd-hl=` highlight query from search results).
    pub fn resource_for_url(&self, dataset_id: &str, url_path: &str, opts: &RenderOptions) -> Result<Resource> {
        let no_fragment = url_path.split('#').next().unwrap_or("");
        let (raw_path, query) = no_fragment.split_once('?').unwrap_or((no_fragment, ""));
        let mut opts = opts.clone();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("sd-hl=") {
                opts.highlight = query_terms(&percent_decode(&v.replace('+', " ")));
            }
        }
        let path = percent_decode(raw_path);
        match self.resource(dataset_id, &path, &opts) {
            // ZIM paths may legitimately contain '?', which the URL parser split off.
            Err(CoreError::NotFound(_)) if !query.is_empty() => {
                self.resource(dataset_id, &percent_decode(no_fragment), &opts)
            }
            r => r,
        }
    }

    pub fn main_page(&self, dataset_id: &str) -> Result<String> {
        let ds = self.dataset(dataset_id)?;
        match ds.archive.main_page()? {
            Some(e) => Ok(ds.archive.user_path(&e)),
            None => self.random_article(dataset_id),
        }
    }

    pub fn random_article(&self, dataset_id: &str) -> Result<String> {
        let ds = self.dataset(dataset_id)?;
        let a = &ds.archive;
        let count = a.article_count();
        if count == 0 {
            return Err(CoreError::NotFound("dataset has no articles".into()));
        }
        let mut last = None;
        for _ in 0..16 {
            let e = a.resolve(a.article_at(fastrand::u32(..count))?)?;
            let html = is_html(a.mime(&e));
            last = Some(a.user_path(&e));
            if html {
                break;
            }
        }
        last.ok_or_else(|| CoreError::NotFound("no article".into()))
    }

    /// Title-prefix listing, tolerant of the first letter's case.
    pub fn list_titles(&self, dataset_id: &str, prefix: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let ds = self.dataset(dataset_id)?;
        Ok(prefix_entries(&ds.archive, prefix, limit)?
            .into_iter()
            .map(|e| (e.display_title().to_string(), ds.archive.user_path(&e)))
            .collect())
    }

    /// Full-text search over indexed datasets, topped up with title matches
    /// from datasets that have no index (side-loads).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let terms = query_terms(query);
        let hl = if terms.is_empty() { String::new() } else { format!("?sd-hl={}", terms.join("+")) };
        let mut out = Vec::new();
        if let Some(index) = &self.index {
            let hits = index.search(query, limit)?;
            for h in &hits.hits {
                let Ok(ds) = self.dataset(&h.dataset) else { continue };
                let snippet = ds
                    .archive
                    .get(&h.path)
                    .ok()
                    .flatten()
                    .and_then(|e| ds.archive.read(&e).ok())
                    .and_then(|raw| {
                        let text = html::extract_text(&String::from_utf8_lossy(&raw)).body;
                        let cut = text.char_indices().nth(SNIPPET_SOURCE_CHARS).map_or(text.len(), |(i, _)| i);
                        hits.snippet(&text[..cut])
                    })
                    .unwrap_or_default();
                out.push(SearchResult {
                    dataset_id: ds.info.id.clone(),
                    dataset_name: ds.info.name.clone(),
                    url_path: format!("{}{hl}", percent_encode(&h.path)),
                    path: h.path.clone(),
                    title: h.title.clone(),
                    snippet,
                });
            }
        }
        let query = query.trim();
        if !query.is_empty() {
            let sets: Vec<_> = self.sets.read().unwrap().iter().filter(|d| !d.info.full_text).cloned().collect();
            for ds in sets {
                let room = limit.saturating_sub(out.len());
                for e in prefix_entries(&ds.archive, query, room)? {
                    let path = ds.archive.user_path(&e);
                    out.push(SearchResult {
                        dataset_id: ds.info.id.clone(),
                        dataset_name: ds.info.name.clone(),
                        url_path: percent_encode(&path),
                        path,
                        title: e.display_title().to_string(),
                        snippet: String::new(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Minimal page for dangling links and blocked resources.
    pub fn message_page(&self, title: &str, message: &str, opts: &RenderOptions) -> String {
        let page = format!(
            "<!doctype html><html><head><title>{t}</title></head><body><h1>{t}</h1><p>{m}</p></body></html>",
            t = html::escape(title),
            m = html::escape(message)
        );
        html::sanitize(&page, opts).unwrap_or(page)
    }
}

fn describe(a: &Archive, id: &str, path: &Path) -> DatasetInfo {
    let meta = |k: &str| a.metadata_str(k).unwrap_or_default();
    let title = meta("Title");
    DatasetInfo {
        id: id.to_string(),
        name: if title.is_empty() { id.to_string() } else { title },
        description: meta("Description"),
        language: meta("Language"),
        license: meta("License"),
        attribution: meta("Creator"),
        article_count: a.article_count(),
        size_bytes: a.file_len(),
        bundled: false,
        full_text: false,
        file_path: PathBuf::from(path).to_string_lossy().into_owned(),
    }
}

fn prefix_entries(a: &Archive, prefix: &str, limit: usize) -> Result<Vec<Entry>> {
    let mut out = a.titles_with_prefix(prefix, limit)?;
    let mut chars = prefix.chars();
    if let Some(first) = chars.next() {
        let flipped: String = if first.is_uppercase() {
            first.to_lowercase().chain(chars).collect()
        } else {
            first.to_uppercase().chain(chars).collect()
        };
        if flipped != prefix && out.len() < limit {
            out.extend(a.titles_with_prefix(&flipped, limit - out.len())?);
        }
    }
    Ok(out)
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h << 4 | l);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encode a ZIM path as a URL path, keeping `/` as the separator.
pub fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
