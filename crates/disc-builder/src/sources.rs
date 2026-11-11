//! Dataset ingestion: every source type is normalised to a list of
//! documents that the ZIM writer consumes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use lol_html::{element, rewrite_str, RewriteStrSettings};
use sd_core::html::{escape, extract_text};
use sd_core::library::percent_decode;
use sd_zim::{Archive, EntryKind, Source};
use walkdir::WalkDir;

pub struct Doc {
    pub path: String,
    pub title: String,
    pub mime: String,
    pub data: Source,
    /// A readable article (listed, searchable) rather than a resource.
    pub front: bool,
}

#[derive(Default)]
pub struct Collected {
    pub docs: Vec<Doc>,
    /// (path, title, target path)
    pub redirects: Vec<(String, String, String)>,
    pub metadata: Vec<(String, Vec<u8>)>,
    pub main_page: Option<String>,
}

fn is_html(mime: &str) -> bool {
    mime.starts_with("text/html")
}

/// Resolve `href` against the document at `base`, staying inside the archive.
pub fn resolve_ref(base: &str, href: &str) -> Option<String> {
    let href = href.split(['#', '?']).next()?.trim();
    if href.is_empty() || href.starts_with("//") {
        return None;
    }
    if let Some(i) = href.find(':') {
        if !href[..i].contains('/') && !href.starts_with("./") {
            return None; // has a scheme
        }
    }
    let mut parts: Vec<&str> =
        if href.starts_with('/') { Vec::new() } else { base.split('/').rev().skip(1).collect::<Vec<_>>().into_iter().rev().collect() };
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Some(percent_decode(&parts.join("/")))
}

/// Paths of images, stylesheets and media an article needs to render.
fn html_refs(base: &str, html: &str) -> Vec<String> {
    let found = std::cell::RefCell::new(Vec::new());
    let push = |v: Option<String>| {
        if let Some(p) = v.and_then(|v| resolve_ref(base, &v)) {
            found.borrow_mut().push(p);
        }
    };
    let _ = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![
                element!("img[src], source[src], video[src], audio[src], track[src], input[src]", |el| {
                    push(el.get_attribute("src"));
                    Ok(())
                }),
                element!("video[poster]", |el| {
                    push(el.get_attribute("poster"));
                    Ok(())
                }),
                element!("link[href]", |el| {
                    push(el.get_attribute("href"));
                    Ok(())
                }),
                element!("img[srcset], source[srcset]", |el| {
                    for c in el.get_attribute("srcset").unwrap_or_default().split(',') {
                        push(c.split_whitespace().next().map(str::to_string));
                    }
                    Ok(())
                }),
            ],
            ..RewriteStrSettings::new()
        },
    );
    found.into_inner()
}

fn css_refs(base: &str, css: &str) -> Vec<String> {
    css.split("url(")
        .skip(1)
        .filter_map(|s| s.split(')').next())
        .filter_map(|u| resolve_ref(base, u.trim().trim_matches(['"', '\''])))
        .collect()
}

/// Re-pack an existing ZIM, optionally narrowed to an include list. Search
/// indexes, scripts and anything unreferenced by a kept article are dropped.
pub fn from_zim(path: &Path, include: Option<&Path>) -> Result<Collected> {
    let a = Arc::new(Archive::open(path).with_context(|| format!("opening {}", path.display()))?);
    a.set_cache_budget(512 << 20);
    let content_ns: &[u8] = if a.has_new_namespace_scheme() { b"C" } else { b"AIJ-" };
    let norm = |s: &str| s.replace('_', " ").to_lowercase();
    let wanted: Option<HashSet<String>> = match include {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .with_context(|| format!("reading {}", p.display()))?
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(norm)
                .collect(),
        ),
        None => None,
    };

    let mut out = Collected::default();
    let mut entries = Vec::new();
    for i in 0..a.entry_count() {
        let e = a.entry_at(i)?;
        if e.namespace == b'M' {
            if e.path != "Counter" {
                out.metadata.push((e.path.clone(), a.read(&e)?));
            }
        } else if content_ns.contains(&e.namespace) {
            entries.push(e);
        }
    }

    // Decide what to keep.
    let by_path: HashMap<String, usize> = entries.iter().enumerate().map(|(i, e)| (a.user_path(e), i)).collect();
    let mut keep: HashSet<usize> = HashSet::new();
    let mut matched: HashSet<String> = HashSet::new();
    match &wanted {
        None => keep.extend(0..entries.len()),
        Some(w) => {
            for (i, e) in entries.iter().enumerate() {
                let EntryKind::Content { .. } = e.kind else { continue };
                if !is_html(a.mime(e)) {
                    continue;
                }
                let hit = [norm(&e.path), norm(&a.user_path(e)), norm(e.display_title())].into_iter().find(|k| w.contains(k));
                let Some(hit) = hit else { continue };
                matched.insert(hit);
                keep.insert(i);
                let base = a.user_path(e);
                for r in html_refs(&base, &String::from_utf8_lossy(&a.read(e)?)) {
                    let Some(&j) = by_path.get(&r) else { continue };
                    if !is_html(a.mime(&entries[j])) && keep.insert(j) && a.mime(&entries[j]).starts_with("text/css") {
                        for r2 in css_refs(&r, &String::from_utf8_lossy(&a.read(&entries[j])?)) {
                            by_path.get(&r2).map(|k| keep.insert(*k));
                        }
                    }
                }
            }
            let missing: Vec<_> = w.iter().filter(|t| !matched.contains(*t)).collect();
            if !missing.is_empty() {
                bail!(
                    "{} include-list entries not found in {} (first: {:?}); the manifest is a completeness contract, fix the list",
                    missing.len(),
                    path.display(),
                    missing.iter().take(5).collect::<Vec<_>>()
                );
            }
        }
    }

    let index_to_pos: HashMap<u32, usize> = entries.iter().enumerate().map(|(i, e)| (e.index, i)).collect();
    for (i, e) in entries.iter().enumerate() {
        let upath = a.user_path(e);
        match e.kind {
            EntryKind::Content { cluster, blob, .. } if keep.contains(&i) => {
                let mime = a.mime(e).to_string();
                if mime.contains("javascript") {
                    continue;
                }
                let (arc, entry) = (a.clone(), e.clone());
                out.docs.push(Doc {
                    path: upath,
                    title: e.title.clone(),
                    front: is_html(&mime),
                    mime,
                    data: Source::Lazy {
                        len: a.blob_len(cluster, blob)?,
                        load: Box::new(move || arc.read(&entry).map_err(std::io::Error::other)),
                    },
                });
            }
            EntryKind::Redirect { .. } => {
                let Ok(target) = a.resolve(e.clone()) else { continue };
                if index_to_pos.get(&target.index).is_some_and(|t| keep.contains(t)) {
                    out.redirects.push((upath, e.title.clone(), a.user_path(&target)));
                }
            }
            _ => {}
        }
    }
    out.main_page = a.main_page()?.map(|e| a.user_path(&e)).filter(|p| out.docs.iter().any(|d| &d.path == p));
    Ok(out)
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "js" | "mjs" => "application/javascript",
        _ => "application/octet-stream",
    }
}

fn walk(dir: &Path) -> Result<Vec<(std::path::PathBuf, String)>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut files = Vec::new();
    for f in WalkDir::new(dir).sort_by_file_name() {
        let f = f?;
        if f.file_type().is_file() {
            let rel = f.path().strip_prefix(dir)?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            files.push((f.path().to_path_buf(), rel));
        }
    }
    Ok(files)
}

pub fn from_html_dir(dir: &Path) -> Result<Collected> {
    let mut out = Collected::default();
    for (file, rel) in walk(dir)? {
        let mime = mime_for(&file);
        if mime.contains("javascript") {
            continue;
        }
        let title = if is_html(mime) {
            extract_text(&String::from_utf8_lossy(&std::fs::read(&file)?)).title
        } else {
            String::new()
        };
        out.docs.push(Doc { path: rel, title, mime: mime.into(), front: is_html(mime), data: Source::File(file) });
    }
    Ok(out)
}

/// Plain-text documents (field manuals, public-domain books).
pub fn from_text_dir(dir: &Path) -> Result<Collected> {
    let mut out = Collected::default();
    for (file, rel) in walk(dir)? {
        if mime_for(&file) != "text/plain" {
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&file)?).replace("\r\n", "\n");
        let text = strip_gutenberg_boilerplate(&text);
        let fallback = file.file_stem().unwrap_or_default().to_string_lossy().replace(['_', '-'], " ");
        let title: String = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.trim_start_matches("Title:").trim().chars().take(120).collect())
            .unwrap_or(fallback);
        let mut html = format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title></head><body><h1>{t}</h1>\n", t = escape(&title));
        for para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).skip(1) {
            html.push_str(&format!("<p>{}</p>\n", escape(para).replace('\n', "<br>\n")));
        }
        html.push_str("</body></html>");
        let path = format!("{}.html", rel.trim_end_matches(".txt"));
        out.docs.push(Doc { path, title, mime: "text/html".into(), front: true, data: Source::Bytes(html.into_bytes()) });
    }
    Ok(out)
}

/// Project Gutenberg's licence only lets a paid product ship the bare
/// public-domain text: its header, footer and trademark must be removed.
fn strip_gutenberg_boilerplate(text: &str) -> &str {
    let mut body = text;
    if let Some(i) = body.find("*** START OF") {
        body = body[i..].split_once('\n').map_or("", |(_, rest)| rest);
    }
    if let Some(i) = body.find("*** END OF") {
        body = &body[..i];
    }
    body
}

/// Alphabetical contents page for sources that do not bring a main page.
pub fn generated_index(name: &str, docs: &[Doc]) -> Doc {
    let mut items: Vec<(&str, &str)> =
        docs.iter().filter(|d| d.front).map(|d| (if d.title.is_empty() { d.path.as_str() } else { d.title.as_str() }, d.path.as_str())).collect();
    items.sort();
    let mut html = format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title></head><body><h1>{t}</h1><ul>\n", t = escape(name));
    for (title, path) in items {
        html.push_str(&format!("<li><a href=\"/{}\">{}</a></li>\n", sd_core::library::percent_encode(path), escape(title)));
    }
    html.push_str("</ul></body></html>");
    Doc { path: "_contents.html".into(), title: format!("{name}: contents"), mime: "text/html".into(), front: false, data: Source::Bytes(html.into_bytes()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_references_inside_the_archive() {
        assert_eq!(resolve_ref("A/Fire", "../I/flame.png").as_deref(), Some("I/flame.png"));
        assert_eq!(resolve_ref("wiki/a/b.html", "./c%20d.png?x=1#y").as_deref(), Some("wiki/a/c d.png"));
        assert_eq!(resolve_ref("a/b", "/css/s.css").as_deref(), Some("css/s.css"));
        assert_eq!(resolve_ref("a", "../../../etc/passwd").as_deref(), Some("etc/passwd"));
        assert_eq!(resolve_ref("a", "https://example.org/x.png"), None);
        assert_eq!(resolve_ref("a", "//cdn/x.png"), None);
        assert_eq!(resolve_ref("a", "data:image/png;base64,AAAA"), None);
        assert_eq!(resolve_ref("a", "#top"), None);
    }

    #[test]
    fn strips_gutenberg_header_and_footer() {
        let text = "The Project Gutenberg eBook of X\n\n*** START OF THE PROJECT GUTENBERG EBOOK X ***\n\nA Title\n\nBody text.\n\n*** END OF THE PROJECT GUTENBERG EBOOK X ***\nLicense blah";
        let body = strip_gutenberg_boilerplate(text);
        assert!(body.contains("A Title") && body.contains("Body text."));
        assert!(!body.contains("Gutenberg") && !body.contains("License blah"));
        assert_eq!(strip_gutenberg_boilerplate("plain"), "plain");
    }

    #[test]
    fn finds_article_resources() {
        let refs = html_refs("A/Fire", r#"<img src="../I/a.png" srcset="../I/b.png 2x"><link rel="stylesheet" href="../-/s.css"><img src="http://x/y.png">"#);
        assert_eq!(refs, ["I/a.png", "I/b.png", "-/s.css"]);
        assert_eq!(css_refs("-/s.css", "a{background:url('../I/bg.png')} @font-face{src:url(f.woff2)}"), ["I/bg.png", "-/f.woff2"]);
    }
}
