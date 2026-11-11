//! Disc assembly: datasets -> ZIMs -> search index -> manifest and licences.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use sd_core::html::extract_text;
use sd_core::search::IndexBuilder;
use sd_core::{DatasetManifest, DiscManifest, DISC_MANIFEST, INDEX_DIR};
use sd_zim::{Archive, Item, Source, WriterOptions, ZimWriter};

use crate::images;
use crate::manifest::{self, ImageMode, Manifest};
use crate::sources::{self, Collected};

pub struct BuildOptions {
    pub zstd_level: i32,
    pub cluster_kib: usize,
    pub index: bool,
}

const MB: u64 = 1 << 20;

fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir).into_iter().flatten().filter_map(|f| f.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

pub fn build(manifest_path: &Path, out: &Path, opts: &BuildOptions) -> Result<()> {
    let m = Manifest::load(manifest_path)?;
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        bail!("output directory {} is not empty", out.display());
    }
    std::fs::create_dir_all(out.join("LICENSES"))?;
    let mut disc = DiscManifest { id: m.disc.id.clone(), title: m.disc.title.clone(), version: m.disc.version.clone(), datasets: Vec::new() };
    let mut over_budget = Vec::new();
    let mut notices = String::from("# Content licences and attribution\n\n");

    for d in &m.datasets {
        let started = Instant::now();
        eprintln!("[{}] collecting", d.id);
        let mut c: Collected = match &d.source {
            manifest::Source::Zim { path, include } => sources::from_zim(path, include.as_deref())?,
            manifest::Source::HtmlDir { path } => sources::from_html_dir(path)?,
            manifest::Source::TextDir { path } => sources::from_text_dir(path)?,
        };
        if c.docs.iter().filter(|x| x.front).count() == 0 {
            bail!("dataset {} produced no articles", d.id);
        }
        if c.main_page.is_none() {
            c.main_page = c.docs.iter().find(|x| x.path == "index.html").map(|x| x.path.clone());
        }
        if c.main_page.is_none() {
            let index = sources::generated_index(&d.name, &c.docs);
            c.main_page = Some(index.path.clone());
            c.docs.push(index);
        }

        let tmp = out.join(format!(".tmp-{}", d.id));
        let image_stats = optimise_images(&mut c, &d.images, &tmp)?;

        let mut meta: BTreeMap<String, Vec<u8>> = c.metadata.into_iter().collect();
        meta.entry("Title".into()).or_insert_with(|| d.name.clone().into_bytes());
        meta.insert("Name".into(), d.id.clone().into_bytes());
        meta.insert("License".into(), d.license.clone().into_bytes());
        if !d.source_url.is_empty() {
            meta.insert("Source".into(), d.source_url.clone().into_bytes());
        }
        meta.entry("Creator".into()).or_insert_with(|| d.attribution.clone().into_bytes());
        meta.entry("Language".into()).or_insert_with(|| b"eng".to_vec());

        let mut w = ZimWriter::new(WriterOptions { zstd_level: opts.zstd_level, cluster_size: opts.cluster_kib << 10 });
        for doc in c.docs {
            w.add(Item::content(doc.path, doc.title, doc.mime, doc.data, doc.front));
        }
        for (path, title, target) in c.redirects {
            w.add(Item::redirect(path, title, target));
        }
        for (k, v) in meta {
            w.add(Item::metadata(k, v));
        }
        w.set_main_page(c.main_page.unwrap());
        let file = format!("{}.zim", d.id);
        eprintln!("[{}] writing {} (zstd -{})", d.id, file, opts.zstd_level);
        let stats = w.finish(out.join(&file)).with_context(|| format!("writing {file}"))?;
        let _ = std::fs::remove_dir_all(&tmp);

        eprintln!(
            "[{}] {} articles, {} entries, {} clusters, {:.1} MB -> {:.1} MB ({:.0}%){} in {:.1}s",
            d.id,
            stats.articles,
            stats.entries,
            stats.clusters,
            stats.uncompressed_bytes as f64 / MB as f64,
            stats.file_bytes as f64 / MB as f64,
            100.0 * stats.file_bytes as f64 / stats.uncompressed_bytes.max(1) as f64,
            image_stats,
            started.elapsed().as_secs_f64()
        );
        if d.budget_mb.is_some_and(|b| stats.file_bytes > b * MB) {
            over_budget.push(format!("{}: {:.1} MB > {} MB", d.id, stats.file_bytes as f64 / MB as f64, d.budget_mb.unwrap()));
        }

        let license_file = format!("{}.txt", d.id);
        std::fs::copy(&d.license_file, out.join("LICENSES").join(&license_file))?;
        notices.push_str(&format!(
            "## {}\n\n- Licence: {} (full text: `{}`)\n- Attribution: {}\n- Source: {}\n- Changes: curated subset, recompressed; images may be resized and re-encoded.\n\n",
            d.name,
            d.license,
            license_file,
            d.attribution,
            if d.source_url.is_empty() { "n/a" } else { &d.source_url }
        ));
        disc.datasets.push(DatasetManifest {
            id: d.id.clone(),
            name: d.name.clone(),
            file,
            license: d.license.clone(),
            attribution: d.attribution.clone(),
            source_url: d.source_url.clone(),
            license_file,
            articles: stats.articles,
            bytes: stats.file_bytes,
        });
    }
    std::fs::write(out.join("LICENSES/ATTRIBUTION.md"), notices)?;

    let mut index_bytes = 0;
    if opts.index {
        let started = Instant::now();
        let (docs, text_bytes) = build_index(out, &disc)?;
        index_bytes = dir_size(&out.join(INDEX_DIR));
        eprintln!(
            "[index] {} documents, {:.1} MB index over {:.1} MB of text ({:.1}%) in {:.1}s",
            docs,
            index_bytes as f64 / MB as f64,
            text_bytes as f64 / MB as f64,
            100.0 * index_bytes as f64 / text_bytes.max(1) as f64,
            started.elapsed().as_secs_f64()
        );
    }
    std::fs::write(out.join(DISC_MANIFEST), serde_json::to_vec_pretty(&disc)?)?;

    let total = dir_size(out);
    println!("{:<28} {:>10} {:>12}", "dataset", "articles", "MB");
    for d in &disc.datasets {
        println!("{:<28} {:>10} {:>12.1}", d.id, d.articles, d.bytes as f64 / MB as f64);
    }
    println!("{:<28} {:>10} {:>12.1}", "search-index", "", index_bytes as f64 / MB as f64);
    println!("{:<28} {:>10} {:>12.1}  (budget {} MB)", "TOTAL", "", total as f64 / MB as f64, m.disc.budget_mb);
    if total > m.disc.budget_mb * MB {
        over_budget.push(format!("disc: {:.1} MB > {} MB", total as f64 / MB as f64, m.disc.budget_mb));
    }
    if !over_budget.is_empty() {
        bail!("size budget exceeded:\n  {}", over_budget.join("\n  "));
    }
    Ok(())
}

/// Re-encode or drop raster images. Transcoded bytes are spilled to disk so
/// memory stays flat regardless of dataset size.
fn optimise_images(c: &mut Collected, cfg: &manifest::Images, tmp: &Path) -> Result<String> {
    match cfg.mode {
        ImageMode::Keep => Ok(String::new()),
        ImageMode::Drop => {
            let before = c.docs.len();
            c.docs.retain(|d| !d.mime.starts_with("image/") || d.mime == "image/svg+xml");
            Ok(format!(", dropped {} images", before - c.docs.len()))
        }
        ImageMode::Webp => {
            if !cfg!(feature = "webp") {
                bail!("images.mode = \"webp\" needs disc-builder built with the `webp` feature");
            }
            std::fs::create_dir_all(tmp)?;
            let results: Vec<Result<(u64, u64)>> = c
                .docs
                .par_iter_mut()
                .enumerate()
                .filter(|(_, d)| images::is_raster(&d.mime))
                .map(|(i, d)| {
                    let original = d.data.load()?;
                    let before = original.len() as u64;
                    let Some(webp) = images::transcode(&original, &d.mime, cfg) else { return Ok((before, before)) };
                    drop(original);
                    let file: PathBuf = tmp.join(format!("{i}.webp"));
                    std::fs::write(&file, &webp)?;
                    // The path keeps its extension; readers go by MIME type.
                    d.mime = "image/webp".into();
                    d.data = Source::File(file);
                    Ok((before, webp.len() as u64))
                })
                .collect();
            let (mut before, mut after) = (0, 0);
            for r in results {
                let (b, a) = r?;
                before += b;
                after += a;
            }
            Ok(format!(", images {:.1} -> {:.1} MB", before as f64 / MB as f64, after as f64 / MB as f64))
        }
    }
}

/// Index every article of every built ZIM. Reading back through the reader
/// doubles as an end-to-end check of the archives just written.
fn build_index(out: &Path, disc: &DiscManifest) -> Result<(u64, u64)> {
    let builder = IndexBuilder::create(&out.join(INDEX_DIR))?;
    let mut text_bytes = 0u64;
    for d in &disc.datasets {
        let a = Archive::open(out.join(&d.file))?;
        if !a.verify_checksum()? {
            bail!("{} failed checksum verification", d.file);
        }
        let sizes: Vec<Result<u64>> = (0..a.article_count())
            .into_par_iter()
            .map(|n| {
                let e = a.article_at(n)?;
                if !a.mime(&e).starts_with("text/html") {
                    return Ok(0);
                }
                let text = extract_text(&String::from_utf8_lossy(&a.read(&e)?));
                let title = if text.title.is_empty() { e.display_title() } else { &text.title };
                builder.add(&d.id, &a.user_path(&e), title, &text.headings, &text.body)?;
                Ok(text.body.len() as u64)
            })
            .collect();
        for s in sizes {
            text_bytes += s?;
        }
    }
    Ok((builder.finish()?, text_bytes))
}
