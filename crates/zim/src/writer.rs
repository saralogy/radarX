//! ZIM writer (v6.1, new namespace scheme).
//!
//! Layout: header, MIME list, path pointers, title pointers, directory
//! entries, cluster pointers, clusters, MD5. Text goes into Zstandard
//! clusters compressed in parallel; already-compressed media is stored raw.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use rayon::prelude::*;

use crate::{Result, ZimError, COMP_NONE, COMP_ZSTD, MAGIC, MIME_REDIRECT, TITLE_LISTING_V1};

pub type Loader = Box<dyn Fn() -> std::io::Result<Vec<u8>> + Send + Sync>;

pub enum Source {
    Bytes(Vec<u8>),
    File(PathBuf),
    /// Loaded only while its cluster is being encoded, so a multi-gigabyte
    /// build never holds the corpus in memory. `len` must be exact.
    Lazy { len: u64, load: Loader },
}

impl Source {
    pub fn size(&self) -> Result<u64> {
        Ok(match self {
            Source::Bytes(b) => b.len() as u64,
            Source::File(p) => std::fs::metadata(p)?.len(),
            Source::Lazy { len, .. } => *len,
        })
    }
    pub fn load(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match self {
            Source::Bytes(b) => b.as_slice().into(),
            Source::File(p) => std::fs::read(p)?.into(),
            Source::Lazy { len, load } => {
                let data = load()?;
                if data.len() as u64 != *len {
                    return Err(ZimError::Invalid("lazy source length changed".into()));
                }
                data.into()
            }
        })
    }
}

enum Kind {
    Content { mime: String, data: Source, front: bool },
    Redirect { ns: u8, path: String },
}

pub struct Item {
    ns: u8,
    path: String,
    title: String,
    kind: Kind,
}

impl Item {
    /// User content (namespace `C`). `front` marks a readable article that
    /// belongs in the title listing, as opposed to an image or stylesheet.
    pub fn content(path: impl Into<String>, title: impl Into<String>, mime: impl Into<String>, data: Source, front: bool) -> Self {
        Self { ns: b'C', path: path.into(), title: title.into(), kind: Kind::Content { mime: mime.into(), data, front } }
    }
    pub fn redirect(path: impl Into<String>, title: impl Into<String>, target: impl Into<String>) -> Self {
        Self { ns: b'C', path: path.into(), title: title.into(), kind: Kind::Redirect { ns: b'C', path: target.into() } }
    }
    pub fn metadata(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        let name = name.into();
        let mime = if name.starts_with("Illustration_") { "image/png" } else { "text/plain;charset=utf-8" };
        Self { ns: b'M', path: name, title: String::new(), kind: Kind::Content { mime: mime.into(), data: Source::Bytes(value.into()), front: false } }
    }
}

#[derive(Clone, Debug)]
pub struct WriterOptions {
    /// Zstandard level for text clusters (1..=22).
    pub zstd_level: i32,
    /// Target uncompressed cluster size.
    pub cluster_size: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self { zstd_level: 19, cluster_size: 2 << 20 }
    }
}

#[derive(Debug, Default, Clone)]
pub struct WriteStats {
    pub entries: u32,
    pub articles: u32,
    pub clusters: u32,
    pub uncompressed_bytes: u64,
    pub file_bytes: u64,
}

#[derive(Default)]
pub struct ZimWriter {
    items: Vec<Item>,
    main_page: Option<String>,
    opts: WriterOptions,
}

fn compressible(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("javascript")
        || mime.contains("json")
        || mime.contains("xml")
        || mime == "application/octet-stream+zimlisting"
}

struct Slot {
    cluster: u32,
    blob: u32,
}

impl ZimWriter {
    pub fn new(opts: WriterOptions) -> Self {
        Self { opts, ..Default::default() }
    }

    pub fn add(&mut self, item: Item) {
        self.items.push(item);
    }

    /// Path (in namespace `C`) of the entry shown when the archive opens.
    pub fn set_main_page(&mut self, path: impl Into<String>) {
        self.main_page = Some(path.into());
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn finish(mut self, out: impl AsRef<Path>) -> Result<WriteStats> {
        if !(1..=22).contains(&self.opts.zstd_level) {
            return Err(ZimError::Invalid("zstd level must be 1..=22".into()));
        }
        let front_count = self.items.iter().filter(|i| matches!(i.kind, Kind::Content { front: true, .. })).count();
        // M/Counter: per-MIME entry counts, which libzim uses for article counts.
        if !self.items.iter().any(|i| i.ns == b'M' && i.path == "Counter") {
            let mut counts = std::collections::BTreeMap::<&str, u32>::new();
            for i in self.items.iter().filter(|i| i.ns == b'C') {
                if let Kind::Content { mime, .. } = &i.kind {
                    *counts.entry(mime).or_default() += 1;
                }
            }
            let counter = counts.iter().map(|(m, n)| format!("{m}={n}")).collect::<Vec<_>>().join(";");
            self.items.push(Item::metadata("Counter", counter));
        }
        if let Some(main) = self.main_page.clone() {
            self.items.push(Item { ns: b'W', path: "mainPage".into(), title: String::new(), kind: Kind::Redirect { ns: b'C', path: main } });
        }
        // Placeholder with its final size; filled once entry indices are known.
        self.items.push(Item {
            ns: b'X',
            path: TITLE_LISTING_V1.into(),
            title: String::new(),
            kind: Kind::Content { mime: "application/octet-stream+zimlisting".into(), data: Source::Bytes(vec![0; front_count * 4]), front: false },
        });

        for it in &self.items {
            if it.path.is_empty() || it.path.contains('\0') || it.title.contains('\0') {
                return Err(ZimError::Invalid(format!("bad path or title: {:?}", it.path)));
            }
        }
        self.items.sort_by(|a, b| (a.ns, a.path.as_bytes()).cmp(&(b.ns, b.path.as_bytes())));
        if let Some(w) = self.items.windows(2).find(|w| w[0].ns == w[1].ns && w[0].path == w[1].path) {
            return Err(ZimError::Invalid(format!("duplicate path {}/{}", w[0].ns as char, w[0].path)));
        }
        if self.items.len() >= u32::MAX as usize {
            return Err(ZimError::Invalid("too many entries".into()));
        }
        let n = self.items.len();
        let index: HashMap<(u8, &str), u32> =
            self.items.iter().enumerate().map(|(i, it)| ((it.ns, it.path.as_str()), i as u32)).collect();

        let title_key = |i: &Item| -> (u8, Vec<u8>) {
            (i.ns, if i.title.is_empty() { i.path.as_bytes().to_vec() } else { i.title.as_bytes().to_vec() })
        };
        let mut by_title: Vec<u32> = (0..n as u32).collect();
        by_title.sort_by_cached_key(|i| title_key(&self.items[*i as usize]));

        let listing: Vec<u8> = by_title
            .iter()
            .filter(|i| matches!(self.items[**i as usize].kind, Kind::Content { front: true, .. }))
            .flat_map(|i| i.to_le_bytes())
            .collect();
        let listing_idx = index[&(b'X', TITLE_LISTING_V1)] as usize;
        let mut redirects = HashMap::new();
        for (i, it) in self.items.iter().enumerate() {
            if let Kind::Redirect { ns, path } = &it.kind {
                let target = index
                    .get(&(*ns, path.as_str()))
                    .ok_or_else(|| ZimError::Invalid(format!("redirect {} -> missing {}", it.path, path)))?;
                redirects.insert(i, *target);
            }
        }
        let main_idx = self.main_page.as_ref().map(|p| index[&(b'C', p.as_str())]);
        drop(index);
        if let Kind::Content { data, .. } = &mut self.items[listing_idx].kind {
            *data = Source::Bytes(listing);
        }

        let mimes: Vec<String> = self
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                Kind::Content { mime, .. } => Some(mime.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mime_id: HashMap<&str, u16> = mimes.iter().enumerate().map(|(i, m)| (m.as_str(), i as u16)).collect();

        // ---- plan clusters by size (no data loaded yet) ----
        let mut clusters: Vec<(bool, Vec<usize>)> = Vec::new();
        let mut slots: Vec<Option<Slot>> = (0..n).map(|_| None).collect();
        let mut open: [Option<(usize, u64)>; 2] = [None, None]; // [raw, compressed] -> (cluster, bytes)
        let mut uncompressed_bytes = 0u64;
        for (i, it) in self.items.iter().enumerate() {
            let Kind::Content { mime, data, .. } = &it.kind else { continue };
            let size = data.size()?;
            if size >= u32::MAX as u64 {
                return Err(ZimError::Invalid(format!("{} exceeds 4 GiB", it.path)));
            }
            uncompressed_bytes += size;
            let comp = compressible(mime);
            let slot = &mut open[comp as usize];
            if slot.is_some_and(|(_, used)| used + size > self.opts.cluster_size as u64) {
                *slot = None;
            }
            let (c, used) = slot.get_or_insert_with(|| {
                clusters.push((comp, Vec::new()));
                (clusters.len() - 1, 0)
            });
            *used += size;
            slots[i] = Some(Slot { cluster: *c as u32, blob: clusters[*c].1.len() as u32 });
            clusters[*c].1.push(i);
        }

        // ---- directory entries ----
        let mut dirents: Vec<Vec<u8>> = Vec::with_capacity(n);
        for (i, it) in self.items.iter().enumerate() {
            let mut d = Vec::with_capacity(18 + it.path.len() + it.title.len());
            match &it.kind {
                Kind::Content { mime, .. } => {
                    let s = slots[i].as_ref().unwrap();
                    d.extend(mime_id[mime.as_str()].to_le_bytes());
                    d.extend([0, it.ns, 0, 0, 0, 0]);
                    d.extend(s.cluster.to_le_bytes());
                    d.extend(s.blob.to_le_bytes());
                }
                Kind::Redirect { .. } => {
                    d.extend(MIME_REDIRECT.to_le_bytes());
                    d.extend([0, it.ns, 0, 0, 0, 0]);
                    d.extend(redirects[&i].to_le_bytes());
                }
            }
            d.extend(it.path.as_bytes());
            d.push(0);
            if it.title != it.path {
                d.extend(it.title.as_bytes());
            }
            d.push(0);
            dirents.push(d);
        }

        // ---- fixed-size prefix ----
        let mut mime_blob = Vec::new();
        for m in &mimes {
            mime_blob.extend(m.as_bytes());
            mime_blob.push(0);
        }
        mime_blob.push(0);
        let mime_list_pos = 80u64;
        let path_ptr_pos = mime_list_pos + mime_blob.len() as u64;
        let title_ptr_pos = path_ptr_pos + 8 * n as u64;
        let dirent_pos = title_ptr_pos + 4 * n as u64;
        let cluster_ptr_pos = dirent_pos + dirents.iter().map(|d| d.len() as u64).sum::<u64>();
        let clusters_pos = cluster_ptr_pos + 8 * clusters.len() as u64;

        // Content-derived UUID keeps builds reproducible.
        let mut h = Md5::new();
        for d in &dirents {
            h.update(d);
        }
        h.update(uncompressed_bytes.to_le_bytes());
        let uuid: [u8; 16] = h.finalize().into();

        let file = File::options().read(true).write(true).create(true).truncate(true).open(&out)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        w.write_all(&[0u8; 80])?;
        w.write_all(&mime_blob)?;
        let mut pos = dirent_pos;
        for d in &dirents {
            w.write_all(&pos.to_le_bytes())?;
            pos += d.len() as u64;
        }
        for i in &by_title {
            w.write_all(&i.to_le_bytes())?;
        }
        for d in &dirents {
            w.write_all(d)?;
        }
        w.write_all(&vec![0u8; 8 * clusters.len()])?;

        // ---- clusters: compress in parallel batches, write in order ----
        let mut cluster_offsets = Vec::with_capacity(clusters.len());
        let mut pos = clusters_pos;
        let batch = rayon::current_num_threads().max(1) * 2;
        let (items, level) = (&self.items, self.opts.zstd_level);
        for chunk in clusters.chunks(batch) {
            let encoded: Vec<Result<Vec<u8>>> =
                chunk.par_iter().map(|(comp, members)| encode_cluster(items, members, *comp, level)).collect();
            for e in encoded {
                let e = e?;
                cluster_offsets.push(pos);
                w.write_all(&e)?;
                pos += e.len() as u64;
            }
        }
        let checksum_pos = pos;

        // ---- patch header and cluster pointers ----
        let mut header = Vec::with_capacity(80);
        header.extend(MAGIC.to_le_bytes());
        header.extend(6u16.to_le_bytes());
        header.extend(1u16.to_le_bytes());
        header.extend(uuid);
        header.extend((n as u32).to_le_bytes());
        header.extend((clusters.len() as u32).to_le_bytes());
        header.extend(path_ptr_pos.to_le_bytes());
        header.extend(title_ptr_pos.to_le_bytes());
        header.extend(cluster_ptr_pos.to_le_bytes());
        header.extend(mime_list_pos.to_le_bytes());
        header.extend(main_idx.unwrap_or(u32::MAX).to_le_bytes());
        header.extend(u32::MAX.to_le_bytes());
        header.extend(checksum_pos.to_le_bytes());
        w.seek(SeekFrom::Start(0))?;
        w.write_all(&header)?;
        w.seek(SeekFrom::Start(cluster_ptr_pos))?;
        for o in &cluster_offsets {
            w.write_all(&o.to_le_bytes())?;
        }
        let mut file = w.into_inner().map_err(|e| ZimError::Io(e.into_error()))?;

        // ---- trailing MD5 over everything written ----
        file.seek(SeekFrom::Start(0))?;
        let mut h = Md5::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let k = file.read(&mut buf)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
        }
        file.seek(SeekFrom::Start(checksum_pos))?;
        file.write_all(&h.finalize())?;
        file.sync_all()?;

        Ok(WriteStats {
            entries: n as u32,
            articles: front_count as u32,
            clusters: clusters.len() as u32,
            uncompressed_bytes,
            file_bytes: checksum_pos + 16,
        })
    }
}

fn encode_cluster(items: &[Item], members: &[usize], compress: bool, level: i32) -> Result<Vec<u8>> {
    let mut blobs = Vec::with_capacity(members.len());
    for i in members {
        let Kind::Content { data, .. } = &items[*i].kind else { unreachable!() };
        blobs.push(data.load()?);
    }
    let table = 4 * (blobs.len() as u64 + 1);
    let total = table + blobs.iter().map(|b| b.len() as u64).sum::<u64>();
    let extended = total > u32::MAX as u64;
    let width = if extended { 8 } else { 4 };
    let mut raw = Vec::with_capacity(total as usize + blobs.len() * 4 + 8);
    let mut off = (width * (blobs.len() + 1)) as u64;
    for len in blobs.iter().map(|b| b.len() as u64).chain([0]) {
        raw.extend(&off.to_le_bytes()[..width]);
        off += len;
    }
    for b in &blobs {
        raw.extend(b.iter());
    }
    let info = if extended { 0x10 } else { 0 };
    let mut out = vec![info | if compress { COMP_ZSTD } else { COMP_NONE }];
    if compress {
        out.extend(zstd::bulk::compress(&raw, level)?);
    } else {
        out.extend(raw);
    }
    Ok(out)
}
