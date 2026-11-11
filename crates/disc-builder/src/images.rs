//! Raster image re-encoding. JPEG becomes lossy WebP; PNG (usually diagrams)
//! becomes lossless WebP so line art stays sharp. The result is used only
//! when it is smaller than the original.

use crate::manifest::Images;

pub fn is_raster(mime: &str) -> bool {
    matches!(mime, "image/jpeg" | "image/jpg" | "image/png")
}

#[cfg(feature = "webp")]
pub fn transcode(data: &[u8], mime: &str, cfg: &Images) -> Option<Vec<u8>> {
    let img = image::load_from_memory(data).ok()?;
    let shrunk = img.width() > cfg.max_width;
    let img = if shrunk {
        let h = (img.height() as u64 * cfg.max_width as u64 / img.width() as u64).max(1) as u32;
        img.resize_exact(cfg.max_width, h, image::imageops::FilterType::CatmullRom)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let enc = webp::Encoder::from_rgba(&rgba, rgba.width(), rgba.height());
    let out = if mime == "image/png" { enc.encode_lossless() } else { enc.encode(cfg.quality) };
    (out.len() < data.len()).then(|| out.to_vec())
}

#[cfg(not(feature = "webp"))]
pub fn transcode(_: &[u8], _: &str, _: &Images) -> Option<Vec<u8>> {
    None
}
