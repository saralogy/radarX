//! Pure-Rust reader and writer for the ZIM archive format (v5/v6).
//!
//! No C++ libzim dependency. The reader does positioned reads only, so one
//! [`Archive`] can be shared across threads without a seek lock.

mod cache;
mod reader;
mod writer;

pub use reader::{Archive, Entry, EntryKind, Header, ZimError};
pub use writer::{Item, Loader, Source, WriteStats, WriterOptions, ZimWriter};

pub type Result<T> = std::result::Result<T, ZimError>;

pub(crate) const MAGIC: u32 = 0x044D_495A;
pub(crate) const MIME_REDIRECT: u16 = 0xFFFF;
pub(crate) const COMP_NONE_A: u8 = 0;
pub(crate) const COMP_NONE: u8 = 1;
pub(crate) const COMP_XZ: u8 = 4;
pub(crate) const COMP_ZSTD: u8 = 5;
pub(crate) const TITLE_LISTING_V1: &str = "listing/titleOrdered/v1";
