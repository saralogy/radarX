//! ZIM reader: header, MIME list, directory entries, clusters, title listing.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use crate::cache::{Cluster, ClusterCache};
use crate::{Result, COMP_NONE, COMP_NONE_A, COMP_XZ, COMP_ZSTD, MAGIC, MIME_REDIRECT, TITLE_LISTING_V1};

/// Default decompressed-cluster cache budget (64 MiB).
pub const DEFAULT_CACHE_BYTES: usize = 64 << 20;
/// Refuse clusters that claim to decompress beyond this (hostile side-loads).
const MAX_CLUSTER_BYTES: u64 = 1 << 30;
const MAX_REDIRECTS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ZimError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("not a supported ZIM file: {0}")]
    Format(String),
    #[error("corrupt archive: {0}")]
    Corrupt(String),
    #[error("entry not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(ZimError::Corrupt(msg.into()))
}

#[derive(Debug, Clone)]
pub struct Header {
    pub major: u16,
    pub minor: u16,
    pub uuid: [u8; 16],
    pub entry_count: u32,
    pub cluster_count: u32,
    pub path_ptr_pos: u64,
    pub title_ptr_pos: u64,
    pub cluster_ptr_pos: u64,
    pub mime_list_pos: u64,
    pub main_page: u32,
    pub layout_page: u32,
    pub checksum_pos: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Content { mime: u16, cluster: u32, blob: u32 },
    Redirect { target: u32 },
    /// Deprecated link-target / deleted entries.
    Other,
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Position in the path-ordered pointer list.
    pub index: u32,
    pub namespace: u8,
    pub path: String,
    /// Empty when identical to `path`.
    pub title: String,
    pub kind: EntryKind,
}

impl Entry {
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.path
        } else {
            &self.title
        }
    }
    pub fn is_redirect(&self) -> bool {
        matches!(self.kind, EntryKind::Redirect { .. })
    }
}

enum TitleList {
    /// u32 entry indices stored contiguously in the file.
    File { pos: u64, count: u32, front_only: bool },
    Mem(Vec<u32>),
    None,
}

pub struct Archive {
    file: File,
    len: u64,
    header: Header,
    mimes: Vec<String>,
    new_ns: bool,
    cache: Mutex<ClusterCache>,
    titles: OnceLock<TitleList>,
}

/// `Read` adapter over positioned reads, bounded by the file length.
struct FileReader<'a> {
    file: &'a File,
    pos: u64,
    end: u64,
}

impl Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let max = (self.end.saturating_sub(self.pos)).min(buf.len() as u64) as usize;
        if max == 0 {
            return Ok(0);
        }
        let n = pread(self.file, &mut buf[..max], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, off)
}
#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, off)
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(File::open(path)?)
    }

    /// Open from an already-open file (e.g. an Android SAF file descriptor).
    pub fn from_file(file: File) -> Result<Self> {
        let len = file.metadata()?.len();
        if len < 80 {
            return Err(ZimError::Format("file shorter than a ZIM header".into()));
        }
        let mut h = [0u8; 80];
        read_exact_at(&file, &mut h, 0)?;
        if le32(&h) != MAGIC {
            return Err(ZimError::Format("bad magic number".into()));
        }
        let header = Header {
            major: le16(&h[4..]),
            minor: le16(&h[6..]),
            uuid: h[8..24].try_into().unwrap(),
            entry_count: le32(&h[24..]),
            cluster_count: le32(&h[28..]),
            path_ptr_pos: le64(&h[32..]),
            title_ptr_pos: le64(&h[40..]),
            cluster_ptr_pos: le64(&h[48..]),
            mime_list_pos: le64(&h[56..]),
            main_page: le32(&h[64..]),
            layout_page: le32(&h[68..]),
            checksum_pos: le64(&h[72..]),
        };
        if !(5..=6).contains(&header.major) {
            return Err(ZimError::Format(format!("unsupported major version {}", header.major)));
        }
        let fits = |pos: u64, n: u64, width: u64| {
            n.checked_mul(width).and_then(|s| pos.checked_add(s)).is_some_and(|e| e <= len)
        };
        if !fits(header.path_ptr_pos, header.entry_count as u64, 8)
            || !fits(header.cluster_ptr_pos, header.cluster_count as u64, 8)
            || header.mime_list_pos >= len
        {
            return corrupt("pointer list outside file");
        }

        let mut raw = vec![0u8; (len - header.mime_list_pos).min(1 << 16) as usize];
        read_exact_at(&file, &mut raw, header.mime_list_pos)?;
        let mut mimes = Vec::new();
        for s in raw.split(|b| *b == 0) {
            if s.is_empty() {
                break;
            }
            mimes.push(String::from_utf8_lossy(s).into_owned());
        }

        Ok(Self {
            file,
            len,
            new_ns: header.major >= 6 && header.minor >= 1,
            header,
            mimes,
            cache: Mutex::new(ClusterCache::new(DEFAULT_CACHE_BYTES)),
            titles: OnceLock::new(),
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn file_len(&self) -> u64 {
        self.len
    }
    pub fn entry_count(&self) -> u32 {
        self.header.entry_count
    }
    /// True for the v6.1+ layout where all user content lives in namespace `C`.
    pub fn has_new_namespace_scheme(&self) -> bool {
        self.new_ns
    }
    pub fn set_cache_budget(&self, bytes: usize) {
        self.cache.lock().unwrap().set_budget(bytes);
    }
    pub fn cache_used(&self) -> usize {
        self.cache.lock().unwrap().used()
    }
    pub fn mime_types(&self) -> &[String] {
        &self.mimes
    }

    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
        if off.checked_add(buf.len() as u64).is_none_or(|e| e > self.len) {
            return corrupt("read past end of file");
        }
        Ok(read_exact_at(&self.file, buf, off)?)
    }

    fn u64_at(&self, off: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_at(&mut b, off)?;
        Ok(u64::from_le_bytes(b))
    }

    fn u32_at(&self, off: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_at(&mut b, off)?;
        Ok(u32::from_le_bytes(b))
    }

    // ---- directory entries -------------------------------------------------

    pub fn entry_at(&self, index: u32) -> Result<Entry> {
        if index >= self.header.entry_count {
            return Err(ZimError::NotFound(format!("entry index {index}")));
        }
        let ptr = self.u64_at(self.header.path_ptr_pos + 8 * index as u64)?;
        if ptr >= self.len {
            return corrupt("directory entry pointer outside file");
        }
        let mut size = 256usize;
        loop {
            let take = size.min((self.len - ptr) as usize);
            let mut buf = vec![0u8; take];
            self.read_at(&mut buf, ptr)?;
            if let Some(e) = parse_dirent(index, &buf)? {
                return Ok(e);
            }
            if take < size || size >= 1 << 16 {
                return corrupt("unterminated directory entry");
            }
            size *= 4;
        }
    }

    /// Binary search the path-ordered pointer list.
    pub fn find(&self, namespace: u8, path: &str) -> Result<Option<Entry>> {
        let (mut lo, mut hi) = (0u32, self.header.entry_count);
        let key = (namespace, path.as_bytes());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = self.entry_at(mid)?;
            match (e.namespace, e.path.as_bytes()).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(e)),
            }
        }
        Ok(None)
    }

    fn article_ns(&self) -> u8 {
        if self.new_ns {
            b'C'
        } else {
            b'A'
        }
    }

    /// Look up user content by reader-facing path: `Foo` in new-scheme
    /// archives, `A/Foo` (namespace prefix) in legacy ones.
    pub fn get(&self, path: &str) -> Result<Option<Entry>> {
        let path = path.trim_start_matches('/');
        if self.new_ns {
            return self.find(b'C', path);
        }
        let mut it = path.splitn(2, '/');
        match (it.next(), it.next()) {
            (Some(ns), Some(rest)) if ns.len() == 1 => self.find(ns.as_bytes()[0], rest),
            _ => Ok(None),
        }
    }

    /// Inverse of [`Archive::get`].
    pub fn user_path(&self, e: &Entry) -> String {
        if self.new_ns {
            e.path.clone()
        } else {
            format!("{}/{}", e.namespace as char, e.path)
        }
    }

    pub fn resolve(&self, mut e: Entry) -> Result<Entry> {
        for _ in 0..MAX_REDIRECTS {
            match e.kind {
                EntryKind::Redirect { target } => e = self.entry_at(target)?,
                _ => return Ok(e),
            }
        }
        corrupt("redirect loop")
    }

    pub fn mime(&self, e: &Entry) -> &str {
        match e.kind {
            EntryKind::Content { mime, .. } => self.mimes.get(mime as usize).map_or("", |s| s),
            _ => "",
        }
    }

    /// Read an entry's content, following redirects.
    pub fn read(&self, e: &Entry) -> Result<Vec<u8>> {
        let e = if e.is_redirect() { self.resolve(e.clone())? } else { e.clone() };
        match e.kind {
            EntryKind::Content { cluster, blob, .. } => self.blob(cluster, blob),
            _ => Err(ZimError::NotFound(format!("{} has no content", e.path))),
        }
    }

    pub fn metadata(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.find(b'M', name)? {
            Some(e) => self.read(&e).map(Some),
            None => Ok(None),
        }
    }

    pub fn metadata_str(&self, name: &str) -> Option<String> {
        let v = self.metadata(name).ok()??;
        Some(String::from_utf8_lossy(&v).trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn main_page(&self) -> Result<Option<Entry>> {
        let e = if self.header.main_page != u32::MAX {
            Some(self.entry_at(self.header.main_page)?)
        } else if self.new_ns {
            self.find(b'W', "mainPage")?
        } else {
            None
        };
        e.map(|e| self.resolve(e)).transpose()
    }

    // ---- clusters ----------------------------------------------------------

    fn cluster(&self, n: u32) -> Result<Arc<Cluster>> {
        if n >= self.header.cluster_count {
            return corrupt(format!("cluster {n} out of range"));
        }
        if let Some(c) = self.cache.lock().unwrap().get(n) {
            return Ok(c);
        }
        // Decompress outside the lock; a racing duplicate load is harmless.
        let c = Arc::new(self.load_cluster(n)?);
        self.cache.lock().unwrap().put(n, c.clone());
        Ok(c)
    }

    fn load_cluster(&self, n: u32) -> Result<Cluster> {
        let off = self.u64_at(self.header.cluster_ptr_pos + 8 * n as u64)?;
        let mut info = [0u8; 1];
        self.read_at(&mut info, off)?;
        let extended = info[0] & 0x10 != 0;
        let width = if extended { 8 } else { 4 };
        let raw = FileReader { file: &self.file, pos: off + 1, end: self.len };
        match info[0] & 0x0F {
            COMP_NONE | COMP_NONE_A => {
                let mut r = BufReader::new(raw);
                let offsets = read_offsets(&mut r, width, self.len - off - 1)?;
                Ok(Cluster::Stored { data_start: off + 1, offsets })
            }
            COMP_ZSTD => {
                let mut dec = zstd::stream::read::Decoder::new(raw)?.single_frame();
                dec.window_log_max(31)?;
                read_cluster(&mut dec, width)
            }
            COMP_XZ => {
                // Streaming decode: stops at the declared cluster size, so
                // bytes following the XZ stream in the file are never touched.
                let mut dec = lzma_rust2::XzReader::new(BufReader::new(raw), false);
                read_cluster(&mut dec, width)
            }
            other => Err(ZimError::Format(format!("unsupported cluster compression {other}"))),
        }
    }

    fn blob_range(&self, c: &Cluster, blob: u32) -> Result<(u64, u64)> {
        let o = c.offsets();
        match (o.get(blob as usize), o.get(blob as usize + 1)) {
            (Some(a), Some(b)) => Ok((*a, *b)),
            _ => corrupt(format!("blob {blob} out of range")),
        }
    }

    pub fn blob(&self, cluster: u32, blob: u32) -> Result<Vec<u8>> {
        let c = self.cluster(cluster)?;
        let (a, b) = self.blob_range(&c, blob)?;
        match &*c {
            Cluster::Loaded { data, .. } => Ok(data[a as usize..b as usize].to_vec()),
            Cluster::Stored { data_start, .. } => {
                let mut buf = vec![0u8; (b - a) as usize];
                self.read_at(&mut buf, data_start + a)?;
                Ok(buf)
            }
        }
    }

    pub fn blob_len(&self, cluster: u32, blob: u32) -> Result<u64> {
        let c = self.cluster(cluster)?;
        let (a, b) = self.blob_range(&c, blob)?;
        Ok(b - a)
    }

    /// Decompress cluster `n` bypassing the cache; returns decompressed size.
    /// Used by benchmarks.
    pub fn decompress_cluster_uncached(&self, n: u32) -> Result<(usize, bool)> {
        Ok(match self.load_cluster(n)? {
            Cluster::Loaded { data, .. } => (data.len(), true),
            Cluster::Stored { offsets, .. } => (*offsets.last().unwrap_or(&0) as usize, false),
        })
    }

    pub fn cluster_count(&self) -> u32 {
        self.header.cluster_count
    }

    // ---- title listing -----------------------------------------------------

    fn title_list(&self) -> &TitleList {
        self.titles.get_or_init(|| self.load_title_list().unwrap_or(TitleList::None))
    }

    fn load_title_list(&self) -> Result<TitleList> {
        if self.new_ns {
            if let Some(Entry { kind: EntryKind::Content { cluster, blob, .. }, .. }) =
                self.find(b'X', TITLE_LISTING_V1)?
            {
                let c = self.cluster(cluster)?;
                let (a, b) = self.blob_range(&c, blob)?;
                let count = ((b - a) / 4) as u32;
                return Ok(match &*c {
                    Cluster::Stored { data_start, .. } => {
                        TitleList::File { pos: data_start + a, count, front_only: true }
                    }
                    Cluster::Loaded { data, .. } => TitleList::Mem(
                        data[a as usize..b as usize].chunks_exact(4).map(le32).collect(),
                    ),
                });
            }
        }
        let (pos, count) = (self.header.title_ptr_pos, self.header.entry_count);
        if pos == u64::MAX || pos.checked_add(4 * count as u64).is_none_or(|e| e > self.len) {
            return Ok(TitleList::None);
        }
        Ok(TitleList::File { pos, count, front_only: false })
    }

    fn title_len(&self) -> u32 {
        match self.title_list() {
            TitleList::File { count, .. } => *count,
            TitleList::Mem(v) => v.len() as u32,
            TitleList::None => 0,
        }
    }

    fn title_entry(&self, i: u32) -> Result<Entry> {
        let idx = match self.title_list() {
            TitleList::File { pos, .. } => self.u32_at(pos + 4 * i as u64)?,
            TitleList::Mem(v) => v[i as usize],
            TitleList::None => return Err(ZimError::NotFound("no title listing".into())),
        };
        self.entry_at(idx)
    }

    fn front_only(&self) -> bool {
        !matches!(self.title_list(), TitleList::File { front_only: false, .. })
    }

    /// First title-list position whose (namespace, title) is >= the key.
    fn title_lower_bound(&self, ns: u8, title: &[u8]) -> Result<u32> {
        let (mut lo, mut hi) = (0u32, self.title_len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = self.title_entry(mid)?;
            if (e.namespace, e.display_title().as_bytes()) < (ns, title) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Title-list range holding article entries.
    fn article_range(&self) -> Result<(u32, u32)> {
        if self.front_only() {
            return Ok((0, self.title_len()));
        }
        let ns = self.article_ns();
        Ok((self.title_lower_bound(ns, b"")?, self.title_lower_bound(ns + 1, b"")?))
    }

    /// Number of entries in the article title listing.
    pub fn article_count(&self) -> u32 {
        self.article_range().map(|(a, b)| b - a).unwrap_or(0)
    }

    /// `n`-th article in title order (`n < article_count()`).
    pub fn article_at(&self, n: u32) -> Result<Entry> {
        let (a, b) = self.article_range()?;
        if n >= b - a {
            return Err(ZimError::NotFound(format!("article {n}")));
        }
        self.title_entry(a + n)
    }

    /// Articles whose title starts with `prefix` (byte-wise, title order).
    pub fn titles_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Entry>> {
        let ns = self.article_ns();
        let (_, end) = self.article_range()?;
        let mut out = Vec::new();
        let mut i = self.title_lower_bound(ns, prefix.as_bytes())?;
        while i < end && out.len() < limit {
            let e = self.title_entry(i)?;
            if e.namespace != ns || !e.display_title().starts_with(prefix) {
                break;
            }
            out.push(e);
            i += 1;
        }
        Ok(out)
    }

    /// Verify the trailing MD5 over the whole archive.
    pub fn verify_checksum(&self) -> Result<bool> {
        use md5::{Digest, Md5};
        let end = self.header.checksum_pos;
        if end.checked_add(16).is_none_or(|e| e > self.len) {
            return corrupt("checksum outside file");
        }
        let mut r = FileReader { file: &self.file, pos: 0, end };
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let mut want = [0u8; 16];
        self.read_at(&mut want, end)?;
        Ok(hasher.finalize().as_slice() == want)
    }
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pread(file, buf, off) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Parse a directory entry; `None` means the buffer was too short.
fn parse_dirent(index: u32, b: &[u8]) -> Result<Option<Entry>> {
    if b.len() < 12 {
        return Ok(None);
    }
    let mime = le16(b);
    let namespace = b[3];
    let (kind, strings) = match mime {
        MIME_REDIRECT => (EntryKind::Redirect { target: le32(&b[8..]) }, 12),
        0xFFFE | 0xFFFD => (EntryKind::Other, 8),
        _ => {
            if b.len() < 16 {
                return Ok(None);
            }
            (EntryKind::Content { mime, cluster: le32(&b[8..]), blob: le32(&b[12..]) }, 16)
        }
    };
    let rest = &b[strings..];
    let Some(p) = rest.iter().position(|c| *c == 0) else { return Ok(None) };
    let Some(t) = rest[p + 1..].iter().position(|c| *c == 0) else { return Ok(None) };
    Ok(Some(Entry {
        index,
        namespace,
        path: String::from_utf8_lossy(&rest[..p]).into_owned(),
        title: String::from_utf8_lossy(&rest[p + 1..p + 1 + t]).into_owned(),
        kind,
    }))
}

/// Read and validate a cluster offset table. `limit` bounds the data size.
fn read_offsets(r: &mut impl Read, width: usize, limit: u64) -> Result<Vec<u64>> {
    let mut first = [0u8; 8];
    r.read_exact(&mut first[..width])?;
    let table = u64::from_le_bytes(first);
    if table < width as u64 || table % width as u64 != 0 || table > limit.min(MAX_CLUSTER_BYTES) {
        return corrupt("bad cluster offset table");
    }
    let mut raw = vec![0u8; table as usize - width];
    r.read_exact(&mut raw)?;
    let mut offsets = Vec::with_capacity(raw.len() / width + 1);
    offsets.push(table);
    for c in raw.chunks_exact(width) {
        let mut v = [0u8; 8];
        v[..width].copy_from_slice(c);
        offsets.push(u64::from_le_bytes(v));
    }
    if offsets.windows(2).any(|w| w[0] > w[1]) || *offsets.last().unwrap() > limit {
        return corrupt("cluster offsets not monotonic or out of bounds");
    }
    Ok(offsets)
}

/// Read a whole decompressed cluster from a stream.
fn read_cluster(r: &mut impl Read, width: usize) -> Result<Cluster> {
    let offsets = read_offsets(r, width, MAX_CLUSTER_BYTES)?;
    let (table, total) = (offsets[0], *offsets.last().unwrap());
    // Keep the table region so blob offsets index `data` directly; grow with
    // the bytes actually produced rather than trusting the declared size.
    let mut data = vec![0u8; table as usize];
    r.take(total - table).read_to_end(&mut data)?;
    if data.len() as u64 != total {
        return corrupt("cluster shorter than its offset table claims");
    }
    Ok(Cluster::Loaded { data, offsets })
}
