//! disc-builder: content pipeline and inspection tool for Survival Disc.

mod build;
mod images;
mod manifest;
mod sources;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sd_core::{Library, RenderOptions};
use sd_zim::{Archive, EntryKind};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a disc directory (ZIMs, search index, licences) from a manifest.
    Build {
        manifest: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Zstandard level for text clusters. Drop to 19 if a build host
        /// cannot afford the level-22 encode time.
        #[arg(long, default_value_t = 22)]
        zstd_level: i32,
        /// Uncompressed cluster size in KiB.
        #[arg(long, default_value_t = 2048)]
        cluster_kib: usize,
        #[arg(long)]
        no_index: bool,
    },
    /// Print header, metadata and counts of a ZIM file.
    Inspect {
        zim: PathBuf,
        /// Also verify the trailing MD5.
        #[arg(long)]
        verify: bool,
    },
    /// List article titles (optionally by prefix).
    Ls {
        zim: PathBuf,
        #[arg(default_value = "")]
        prefix: String,
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Write one entry to stdout.
    Cat {
        zim: PathBuf,
        path: String,
        /// Emit the sanitised HTML the reader would display.
        #[arg(long)]
        sanitized: bool,
    },
    /// Query a built disc.
    Search {
        disc: PathBuf,
        query: String,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Measure cluster decompression, article rendering and search latency.
    Bench {
        /// A disc directory or a single ZIM file.
        target: PathBuf,
        #[arg(long, default_value_t = 200)]
        samples: u32,
        /// Queries for the search benchmark (disc directories only).
        #[arg(long, value_delimiter = ',', default_value = "water purification,burn treatment,fracture splint,fever child,snake bite")]
        queries: Vec<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build { manifest, out, zstd_level, cluster_kib, no_index } => {
            build::build(&manifest, &out, &build::BuildOptions { zstd_level, cluster_kib, index: !no_index })
        }
        Cmd::Inspect { zim, verify } => inspect(&zim, verify),
        Cmd::Ls { zim, prefix, limit } => {
            let a = Archive::open(&zim)?;
            let entries = if prefix.is_empty() {
                (0..a.article_count().min(limit as u32)).map(|n| a.article_at(n)).collect::<Result<Vec<_>, _>>()?
            } else {
                a.titles_with_prefix(&prefix, limit)?
            };
            for e in entries {
                println!("{}\t{}", a.user_path(&e), e.display_title());
            }
            Ok(())
        }
        Cmd::Cat { zim, path, sanitized } => {
            use std::io::Write;
            let lib = Library::new();
            let ds = lib.add_archive(&zim)?;
            let data = if sanitized {
                lib.resource(&ds.id, &path, &RenderOptions::default())?.data
            } else {
                let a = Archive::open(&zim)?;
                let e = a.get(&path)?.with_context(|| format!("{path} not found"))?;
                a.read(&e)?
            };
            std::io::stdout().write_all(&data)?;
            Ok(())
        }
        Cmd::Search { disc, query, limit } => {
            let lib = Library::open_disc(&disc)?;
            let t = Instant::now();
            let results = lib.search(&query, limit)?;
            let took = t.elapsed();
            for r in &results {
                println!("[{}] {}\n    {}\n    {}", r.dataset_name, r.title, r.url_path, r.snippet);
            }
            eprintln!("{} results in {:.1} ms", results.len(), took.as_secs_f64() * 1e3);
            Ok(())
        }
        Cmd::Bench { target, samples, queries } => bench(&target, samples, &queries),
    }
}

fn inspect(path: &Path, verify: bool) -> Result<()> {
    let a = Archive::open(path)?;
    let h = a.header();
    println!("file            {} ({:.1} MB)", path.display(), a.file_len() as f64 / 1048576.0);
    println!("version         {}.{} ({} namespace scheme)", h.major, h.minor, if a.has_new_namespace_scheme() { "new" } else { "legacy" });
    println!("uuid            {}", h.uuid.iter().map(|b| format!("{b:02x}")).collect::<String>());
    println!("entries         {}", h.entry_count);
    println!("articles        {}", a.article_count());
    println!("clusters        {}", h.cluster_count);
    println!("main page       {}", a.main_page()?.map(|e| a.user_path(&e)).unwrap_or_else(|| "-".into()));
    let (mut redirects, mut by_mime) = (0u32, std::collections::BTreeMap::<String, u32>::new());
    for i in 0..h.entry_count {
        let e = a.entry_at(i)?;
        match e.kind {
            EntryKind::Redirect { .. } => redirects += 1,
            EntryKind::Content { .. } => *by_mime.entry(a.mime(&e).to_string()).or_default() += 1,
            EntryKind::Other => {}
        }
    }
    println!("redirects       {redirects}");
    for (m, n) in by_mime {
        println!("  {m:<40} {n}");
    }
    for key in ["Title", "Description", "Language", "Creator", "Publisher", "Date", "License", "Source"] {
        if let Some(v) = a.metadata_str(key) {
            println!("M/{key:<14}{v}");
        }
    }
    if verify {
        println!("checksum        {}", if a.verify_checksum()? { "ok" } else { "MISMATCH" });
    }
    Ok(())
}

fn summarize(label: &str, mut times: Vec<Duration>) {
    if times.is_empty() {
        return;
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let pct = |p: f64| ms(times[((times.len() - 1) as f64 * p) as usize]);
    println!("{label:<34} n={:<5} p50={:>8.2} ms  p95={:>8.2} ms  max={:>8.2} ms", times.len(), pct(0.5), pct(0.95), ms(*times.last().unwrap()));
}

fn bench(target: &Path, samples: u32, queries: &[String]) -> Result<()> {
    let (lib, zims): (Library, Vec<PathBuf>) = if target.is_dir() {
        let lib = Library::open_disc(target)?;
        let zims = lib.datasets().iter().map(|d| PathBuf::from(&d.file_path)).collect();
        (lib, zims)
    } else {
        let lib = Library::new();
        lib.add_archive(target)?;
        (lib, vec![target.to_path_buf()])
    };
    if samples == 0 {
        bail!("--samples must be positive");
    }
    for (zim, ds) in zims.iter().zip(lib.datasets()) {
        println!("== {} ({})", ds.name, zim.display());
        let a = Archive::open(zim)?;
        let (mut times, mut bytes) = (Vec::new(), 0usize);
        let step = (a.cluster_count() / samples).max(1);
        for n in (0..a.cluster_count()).step_by(step as usize) {
            let t = Instant::now();
            let (size, compressed) = a.decompress_cluster_uncached(n)?;
            if compressed {
                times.push(t.elapsed());
                bytes += size;
            }
        }
        let total: f64 = times.iter().map(Duration::as_secs_f64).sum();
        summarize("cluster decompress (cold cache)", times);
        if total > 0.0 {
            println!("{:<34} {:.0} MB/s", "decompress throughput", bytes as f64 / 1048576.0 / total);
        }
        let mut render = Vec::new();
        for _ in 0..samples {
            let path = lib.random_article(&ds.id)?;
            let t = Instant::now();
            lib.resource(&ds.id, &path, &RenderOptions::default())?;
            render.push(t.elapsed());
        }
        summarize("random article read + sanitise", render);
    }
    if target.is_dir() {
        let mut times = Vec::new();
        for q in queries.iter().cycle().take(queries.len() * 5) {
            let t = Instant::now();
            lib.search(q, 10)?;
            times.push(t.elapsed());
        }
        println!("== search");
        summarize("top-10 with snippets", times);
    }
    Ok(())
}
