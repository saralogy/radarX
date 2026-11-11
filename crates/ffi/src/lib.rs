//! UniFFI surface for the native shells.
//!
//! Every call is synchronous and thread-safe. The shells invoke them off the
//! main thread (`Dispatchers.IO` / a detached `Task`); WebView resource
//! requests already arrive on background threads and must be answered
//! synchronously, so a single Rust worker thread would only serialise image
//! loading.

use std::sync::Arc;

use sd_core::{CoreError, Library, RenderOptions};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum DiscError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Blocked(String),
    #[error("{0}")]
    Corrupt(String),
    #[error("{0}")]
    Io(String),
}

impl From<CoreError> for DiscError {
    fn from(e: CoreError) -> Self {
        let msg = e.to_string();
        match e {
            CoreError::NotFound(_) => Self::NotFound(msg),
            CoreError::Blocked(_) => Self::Blocked(msg),
            CoreError::Io(_) => Self::Io(msg),
            CoreError::Zim(sd_core::zim::ZimError::NotFound(_)) => Self::NotFound(msg),
            CoreError::Zim(sd_core::zim::ZimError::Io(_)) => Self::Io(msg),
            _ => Self::Corrupt(msg),
        }
    }
}

type Result<T> = std::result::Result<T, DiscError>;

#[derive(uniffi::Enum, Clone, Copy)]
pub enum Theme {
    Light,
    Dark,
    Sepia,
}

#[derive(uniffi::Record, Clone)]
pub struct ReaderOptions {
    pub theme: Theme,
    pub font_scale_percent: u32,
}

impl From<&ReaderOptions> for RenderOptions {
    fn from(o: &ReaderOptions) -> Self {
        RenderOptions {
            theme: match o.theme {
                Theme::Light => sd_core::Theme::Light,
                Theme::Dark => sd_core::Theme::Dark,
                Theme::Sepia => sd_core::Theme::Sepia,
            },
            font_scale_percent: o.font_scale_percent,
            highlight: Vec::new(),
        }
    }
}

#[derive(uniffi::Record)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub language: String,
    pub license: String,
    pub attribution: String,
    pub article_count: u32,
    pub size_bytes: u64,
    pub bundled: bool,
    pub full_text: bool,
    pub file_path: String,
}

impl From<sd_core::DatasetInfo> for Dataset {
    fn from(d: sd_core::DatasetInfo) -> Self {
        Self {
            id: d.id,
            name: d.name,
            description: d.description,
            language: d.language,
            license: d.license,
            attribution: d.attribution,
            article_count: d.article_count,
            size_bytes: d.size_bytes,
            bundled: d.bundled,
            full_text: d.full_text,
            file_path: d.file_path,
        }
    }
}

#[derive(uniffi::Record)]
pub struct Resource {
    pub mime: String,
    pub data: Vec<u8>,
    pub path: String,
    pub title: String,
}

#[derive(uniffi::Record)]
pub struct SearchResult {
    pub dataset_id: String,
    pub dataset_name: String,
    pub title: String,
    /// Append to `<base>/<dataset_id>/` to open the hit with highlights.
    pub url_path: String,
    /// HTML; matched terms are wrapped in `<b>`.
    pub snippet: String,
}

#[derive(uniffi::Record)]
pub struct TitleEntry {
    pub title: String,
    pub url_path: String,
}

#[derive(uniffi::Record)]
pub struct LicenseNotice {
    pub dataset_name: String,
    pub license: String,
    pub attribution: String,
    pub source_url: String,
    /// File name inside the disc's `LICENSES/` directory.
    pub license_file: String,
}

#[derive(uniffi::Object)]
pub struct Disc {
    lib: Library,
}

#[uniffi::export]
impl Disc {
    /// Open the bundled disc directory (asset pack / on-demand resource).
    #[uniffi::constructor]
    pub fn open(disc_dir: String) -> Result<Arc<Self>> {
        Ok(Arc::new(Self { lib: Library::open_disc(disc_dir)? }))
    }

    /// Start without a bundled disc; only side-loaded archives.
    #[uniffi::constructor]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self { lib: Library::new() })
    }

    /// Side-load a ZIM file by path (iOS security-scoped URL, app storage).
    pub fn add_archive(&self, path: String) -> Result<Dataset> {
        Ok(self.lib.add_archive(path)?.into())
    }

    /// Side-load from a file descriptor (Android SAF). Ownership of `fd`
    /// moves to Rust: pass `ParcelFileDescriptor.detachFd()`.
    pub fn add_archive_fd(&self, fd: i32, name: String) -> Result<Dataset> {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            if fd < 0 {
                return Err(DiscError::Io("invalid file descriptor".into()));
            }
            // SAFETY: the caller transfers ownership of an open descriptor.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            Ok(self.lib.add_archive_file(file, &name)?.into())
        }
        #[cfg(not(unix))]
        {
            let _ = (fd, name);
            Err(DiscError::Io("file descriptors are not supported on this platform".into()))
        }
    }

    pub fn remove_archive(&self, dataset_id: String) -> Result<()> {
        Ok(self.lib.remove_archive(&dataset_id)?)
    }

    pub fn datasets(&self) -> Vec<Dataset> {
        self.lib.datasets().into_iter().map(Into::into).collect()
    }

    /// 48x48 PNG cover, if the archive has one.
    pub fn cover(&self, dataset_id: String) -> Result<Option<Vec<u8>>> {
        Ok(self.lib.cover(&dataset_id)?)
    }

    /// Answer a WebView request. `url_path` is the request URL's path and
    /// query exactly as received (percent-encoded). HTML comes back sanitised.
    pub fn get_resource(&self, dataset_id: String, url_path: String, options: ReaderOptions) -> Result<Resource> {
        let r = self.lib.resource_for_url(&dataset_id, &url_path, &(&options).into())?;
        Ok(Resource { mime: r.mime, data: r.data, path: r.path, title: r.title })
    }

    /// Themed placeholder page for missing or blocked resources.
    pub fn message_page(&self, title: String, message: String, options: ReaderOptions) -> String {
        self.lib.message_page(&title, &message, &(&options).into())
    }

    pub fn search(&self, query: String, limit: u32) -> Result<Vec<SearchResult>> {
        Ok(self
            .lib
            .search(&query, limit as usize)?
            .into_iter()
            .map(|r| SearchResult {
                dataset_id: r.dataset_id,
                dataset_name: r.dataset_name,
                title: r.title,
                url_path: r.url_path,
                snippet: r.snippet,
            })
            .collect())
    }

    pub fn list_titles(&self, dataset_id: String, prefix: String, limit: u32) -> Result<Vec<TitleEntry>> {
        Ok(self
            .lib
            .list_titles(&dataset_id, &prefix, limit as usize)?
            .into_iter()
            .map(|(title, path)| TitleEntry { title, url_path: sd_core::library::percent_encode(&path) })
            .collect())
    }

    /// URL path of a random article.
    pub fn random_article(&self, dataset_id: String) -> Result<String> {
        Ok(sd_core::library::percent_encode(&self.lib.random_article(&dataset_id)?))
    }

    /// URL path of the dataset's main page.
    pub fn main_page(&self, dataset_id: String) -> Result<String> {
        Ok(sd_core::library::percent_encode(&self.lib.main_page(&dataset_id)?))
    }

    /// Attribution data for the Licences screen.
    pub fn licenses(&self) -> Vec<LicenseNotice> {
        self.lib.disc().map_or_else(Vec::new, |d| {
            d.datasets
                .iter()
                .map(|s| LicenseNotice {
                    dataset_name: s.name.clone(),
                    license: s.license.clone(),
                    attribution: s.attribution.clone(),
                    source_url: s.source_url.clone(),
                    license_file: s.license_file.clone(),
                })
                .collect()
        })
    }

    /// Total decompressed-cluster cache across datasets (default 64 MiB each).
    pub fn set_cache_budget(&self, total_bytes: u64) {
        self.lib.set_cache_budget(total_bytes as usize);
    }
}
