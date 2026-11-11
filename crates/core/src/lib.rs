//! Survival Disc core: a library of ZIM datasets with sanitised rendering and
//! full-text search. Everything here is synchronous and `Send + Sync`; the
//! native shells call it from background threads.

pub mod html;
pub mod library;
pub mod search;

pub use html::{RenderOptions, Theme};
pub use library::{DatasetInfo, DatasetManifest, DiscManifest, Library, Resource, SearchResult, DISC_MANIFEST, INDEX_DIR};
pub use sd_zim as zim;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("archive error: {0}")]
    Zim(#[from] sd_zim::ZimError),
    #[error("search error: {0}")]
    Search(String),
    #[error("render error: {0}")]
    Render(String),
    #[error("invalid disc: {0}")]
    Disc(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
