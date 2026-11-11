//! HTML sanitising for the reader WebView and text extraction for indexing.
//!
//! Articles are served under a hierarchical base URL
//! (`<scheme>://<dataset>/<path>`), so relative links, images and stylesheets
//! resolve inside the WebView without rewriting. What is rewritten here is
//! everything that must never survive: scripts, event handlers, remote
//! references, and URL schemes that would leave the archive.

use std::cell::RefCell;
use std::rc::Rc;

use lol_html::html_content::{ContentType, TextType};
use lol_html::{doc_text, element, rewrite_str, text, RewriteStrSettings};

use crate::CoreError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Light,
    Dark,
    Sepia,
}

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub theme: Theme,
    /// Root font size in percent (100 = platform default).
    pub font_scale_percent: u32,
    /// Query terms to wrap in `<mark>`.
    pub highlight: Vec<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { theme: Theme::Light, font_scale_percent: 100, highlight: Vec::new() }
    }
}

const CSP: &str = "default-src 'self' data:; script-src 'none'; object-src 'none'; \
base-uri 'none'; form-action 'none'; style-src 'self' 'unsafe-inline'";

const BASE_CSS: &str = "html{-webkit-text-size-adjust:100%;text-size-adjust:100%}\
body{max-width:46em;margin:0 auto;padding:0 1em 3em;line-height:1.55;overflow-wrap:anywhere;\
font-family:-apple-system,system-ui,'Segoe UI',Roboto,sans-serif}\
img,svg,video{max-width:100%;height:auto}table{display:block;max-width:100%;overflow-x:auto}\
pre{overflow-x:auto;white-space:pre-wrap}mark.sd-hl{background:#ffe066;color:#000;border-radius:2px}\
a[data-sd-external]{text-decoration:line-through dotted;opacity:.7}";

fn head_block(opts: &RenderOptions) -> String {
    let scale = opts.font_scale_percent.clamp(50, 300);
    let palette = match opts.theme {
        Theme::Light => String::new(),
        Theme::Dark => themed("#121212", "#e3e3e3", "#8ab4f8", "#333"),
        Theme::Sepia => themed("#f4ecd8", "#3b2f1e", "#7a4a12", "#cdbf9f"),
    };
    format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<style id=\"sd-reader\">html{{font-size:{scale}%}}{BASE_CSS}{palette}</style>"
    )
}

/// Dark/sepia must override whatever colours the source wiki hard-coded.
fn themed(bg: &str, fg: &str, link: &str, border: &str) -> String {
    format!(
        "html,body{{background:{bg}!important;color:{fg}!important}}\
body *{{background-color:transparent!important;color:inherit!important;border-color:{border}!important}}\
a,a *{{color:{link}!important}}mark.sd-hl{{background:#ffe066!important;color:#000!important}}"
    )
}

const URL_ATTRS: &[&str] =
    &["href", "src", "poster", "data", "action", "formaction", "background", "xlink:href", "manifest", "cite"];
const EXTERNAL_SCHEMES: &[&str] = &[
    "http", "https", "ftp", "ftps", "mailto", "tel", "sms", "geo", "file", "ws", "wss", "blob", "about",
    "intent", "market", "content", "chrome", "irc", "magnet", "news",
];

#[derive(Debug, PartialEq)]
enum UrlClass {
    Local,
    /// `Help:Contents`-style relative path a browser would misread as a scheme.
    LocalColon,
    Zim(String),
    DataImage,
    External,
    Dangerous,
}

/// Decode character references the way a browser will before it parses the
/// URL. Attribute values reach us raw, so `java&#9;script&colon;` must be
/// resolved here. Unknown named references are rejected rather than guessed.
fn decode_url_attr(v: &str) -> Option<String> {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(num) = rest.strip_prefix('#') {
            let (digits, radix) = match num.strip_prefix(['x', 'X']) {
                Some(h) => (h, 16),
                None => (num, 10),
            };
            let len = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
            if len == 0 {
                out.push('&');
                continue;
            }
            out.push(u32::from_str_radix(&digits[..len], radix).ok().and_then(char::from_u32)?);
            rest = digits[len..].strip_prefix(';').unwrap_or(&digits[len..]);
            continue;
        }
        let len = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
        if len == 0 || !rest[len..].starts_with(';') {
            out.push('&');
            continue;
        }
        out.push(match &rest[..len] {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => return None,
        });
        rest = &rest[len + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn classify(value: &str) -> UrlClass {
    let Some(value) = decode_url_attr(value) else { return UrlClass::Dangerous };
    // Browsers strip tabs/newlines and leading control chars before parsing.
    let t: String = value.chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("//") || lower.starts_with("\\\\") || lower.starts_with("/\\") || lower.starts_with("\\/") {
        return UrlClass::External;
    }
    let end = lower.find([':', '/', '?', '#']).unwrap_or(lower.len());
    if !lower[end..].starts_with(':') || end == 0 {
        return UrlClass::Local;
    }
    let scheme = &lower[..end];
    let valid = scheme.starts_with(|c: char| c.is_ascii_lowercase())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    if !valid {
        return UrlClass::Local;
    }
    match scheme {
        "javascript" | "vbscript" | "livescript" | "mhtml" => UrlClass::Dangerous,
        "data" if lower.starts_with("data:image/") => UrlClass::DataImage,
        "data" => UrlClass::Dangerous,
        "zim" => UrlClass::Zim(format!("/{}", t[end + 1..].trim_start_matches('/'))),
        s if EXTERNAL_SCHEMES.contains(&s) => UrlClass::External,
        _ => UrlClass::LocalColon,
    }
}

fn srcset_is_local(v: &str) -> bool {
    v.split(',').all(|c| matches!(classify(c.split_whitespace().next().unwrap_or("")), UrlClass::Local))
}

/// Produce the HTML handed to the WebView.
pub fn sanitize(html: &str, opts: &RenderOptions) -> Result<String, CoreError> {
    let head = head_block(opts);
    let seen_head = Rc::new(RefCell::new(false));
    let terms: Vec<String> =
        opts.highlight.iter().map(|t| t.to_lowercase()).filter(|t| t.chars().count() >= 2).collect();

    let handlers = vec![
        element!(
            "script, iframe, frame, frameset, object, embed, applet, base, meta[http-equiv], portal, template",
            |el| {
                el.remove();
                Ok(())
            }
        ),
        element!("noscript", |el| {
            // Scripting is off, so the fallback content is the content.
            el.remove_and_keep_content();
            Ok(())
        }),
        element!("set, animate, animateTransform, animateMotion", |el| {
            // SVG animation can retarget href to a script URL.
            el.remove();
            Ok(())
        }),
        element!("link", |el| {
            let rel = el.get_attribute("rel").unwrap_or_default().to_ascii_lowercase();
            if !rel.split_whitespace().any(|r| r == "stylesheet" || r == "icon") {
                el.remove();
            }
            Ok(())
        }),
        element!("*", |el| {
            let is_anchor = el.tag_name() == "a" || el.tag_name() == "area";
            let is_image = matches!(el.tag_name().as_str(), "img" | "source" | "image");
            let attrs: Vec<(String, String)> = el.attributes().iter().map(|a| (a.name(), a.value())).collect();
            for (name, value) in attrs {
                if name.starts_with("on") || matches!(name.as_str(), "target" | "ping" | "integrity" | "srcdoc") {
                    el.remove_attribute(&name);
                } else if name == "style" {
                    let v = decode_url_attr(&value).map(|v| v.to_ascii_lowercase());
                    if v.is_none_or(|v| v.contains("javascript:") || v.contains("expression(") || v.contains("@import")) {
                        el.remove_attribute(&name);
                    }
                } else if name == "srcset" || name == "imagesrcset" {
                    if !srcset_is_local(&value) {
                        el.remove_attribute(&name);
                    }
                } else if URL_ATTRS.contains(&name.as_str()) {
                    match classify(&value) {
                        UrlClass::Local => {}
                        UrlClass::LocalColon => el.set_attribute(&name, &format!("./{}", value.trim()))?,
                        UrlClass::Zim(path) => el.set_attribute(&name, &path)?,
                        UrlClass::DataImage if is_image && name == "src" => {}
                        UrlClass::External if is_anchor && name == "href" => {
                            el.remove_attribute(&name);
                            el.set_attribute("data-sd-external", &value)?;
                        }
                        _ => el.remove_attribute(&name),
                    }
                }
            }
            Ok(())
        }),
        element!("head", {
            let (seen, head) = (seen_head.clone(), head.clone());
            move |el| {
                if !seen.replace(true) {
                    el.prepend(&head, ContentType::Html);
                }
                Ok(())
            }
        }),
        element!("body", {
            let (seen, head) = (seen_head.clone(), head.clone());
            move |el| {
                if !seen.replace(true) {
                    el.before(&head, ContentType::Html);
                }
                Ok(())
            }
        }),
    ];
    let doc_handlers = if terms.is_empty() {
        vec![]
    } else {
        vec![doc_text!(move |t| {
            if t.text_type() == TextType::Data {
                if let Some(marked) = mark_terms(t.as_str(), &terms) {
                    t.replace(&marked, ContentType::Html);
                }
            }
            Ok(())
        })]
    };

    let out = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: handlers,
            document_content_handlers: doc_handlers,
            ..RewriteStrSettings::new()
        },
    )
    .map_err(|e| CoreError::Render(e.to_string()))?;

    Ok(if *seen_head.borrow() { out } else { format!("{head}{out}") })
}

/// Wrap whole words starting with a query term in `<mark>`. Operates on raw
/// (still entity-encoded) text, so matches never start inside an entity.
fn mark_terms(text: &str, terms: &[String]) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut prev: Option<char> = None;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let at_word_start = c.is_alphanumeric() && !prev.is_some_and(|p| p.is_alphanumeric() || p == '&');
        prev = Some(c);
        if !at_word_start {
            continue;
        }
        let rest = &text[i..];
        let hit = terms.iter().any(|t| {
            rest.len() >= t.len() && rest.is_char_boundary(t.len()) && rest[..t.len()].to_lowercase() == *t
        });
        if !hit {
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, n)) = iter.peek() {
            if !n.is_alphanumeric() {
                break;
            }
            end = j + n.len_utf8();
            prev = Some(n);
            iter.next();
        }
        out.push_str(&text[copied..i]);
        out.push_str("<mark class=\"sd-hl\">");
        out.push_str(&text[i..end]);
        out.push_str("</mark>");
        copied = end;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

/// Plain text pulled from an article for indexing and snippets.
#[derive(Debug, Default, Clone)]
pub struct Extracted {
    pub title: String,
    pub headings: String,
    pub body: String,
}

#[derive(Default)]
struct ExtractState {
    out: Extracted,
    skip: u32,
    heading: u32,
}

/// Elements whose text is navigation, citation or caption noise.
const SKIP: &str = "script, style, nav, header, footer, figcaption, sup.reference, .references, .reflist, \
.navbox, .mw-editsection, .thumbcaption, .noprint, .hatnote, .metadata, .catlinks";
const BLOCKS: &str = "p, div, br, li, td, th, tr, h1, h2, h3, h4, h5, h6, section, dt, dd, blockquote, pre, table";

pub fn extract_text(html: &str) -> Extracted {
    let st = Rc::new(RefCell::new(ExtractState::default()));
    let scoped = |field: fn(&mut ExtractState) -> &mut u32| {
        let st = st.clone();
        move |el: &mut lol_html::html_content::Element| {
            *field(&mut st.borrow_mut()) += 1;
            let st = st.clone();
            if let Some(h) = el.end_tag_handlers() {
                h.push(Box::new(move |_| {
                    let mut s = st.borrow_mut();
                    let f = field(&mut s);
                    *f = f.saturating_sub(1);
                    Ok(())
                }));
            } else {
                let mut s = st.borrow_mut();
                let f = field(&mut s);
                *f = f.saturating_sub(1);
            }
            Ok(())
        }
    };
    let space = {
        let st = st.clone();
        move |_: &mut lol_html::html_content::Element| {
            let mut s = st.borrow_mut();
            if s.skip == 0 {
                if s.heading > 0 {
                    s.out.headings.push(' ');
                }
                s.out.body.push(' ');
            }
            Ok(())
        }
    };
    let title = {
        let st = st.clone();
        move |t: &mut lol_html::html_content::TextChunk| {
            decode_entities(t.as_str(), &mut st.borrow_mut().out.title);
            Ok(())
        }
    };
    let body = {
        let st = st.clone();
        move |t: &mut lol_html::html_content::TextChunk| {
            let mut s = st.borrow_mut();
            if s.skip == 0 && t.text_type() == TextType::Data {
                if s.heading > 0 {
                    decode_entities(t.as_str(), &mut s.out.headings);
                }
                decode_entities(t.as_str(), &mut s.out.body);
            }
            Ok(())
        }
    };
    let res = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![
                element!(SKIP, scoped(|s| &mut s.skip)),
                element!("h1, h2, h3, h4, h5, h6", scoped(|s| &mut s.heading)),
                element!(BLOCKS, space),
                text!("head > title", title),
            ],
            document_content_handlers: vec![doc_text!(body)],
            ..RewriteStrSettings::new()
        },
    );
    drop(res);
    let mut out = std::mem::take(&mut st.borrow_mut().out);
    for f in [&mut out.title, &mut out.headings, &mut out.body] {
        *f = f.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    out
}

fn decode_entities(raw: &str, out: &mut String) {
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|e| e + 1);
        let (name, after) = match end {
            Some(e) if rest[e..].starts_with(';') => (&rest[1..e], &rest[e + 1..]),
            _ => {
                out.push('&');
                rest = &rest[1..];
                continue;
            }
        };
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            n => n
                .strip_prefix("#x")
                .or_else(|| n.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| n.strip_prefix('#').map(|d| d.parse()))
                .and_then(|r| r.ok())
                .and_then(char::from_u32),
        };
        // Unknown named entities index as a word break.
        out.push(decoded.unwrap_or(' '));
        rest = after;
    }
    out.push_str(rest);
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(html: &str) -> String {
        sanitize(html, &RenderOptions::default()).unwrap()
    }

    #[test]
    fn strips_active_content() {
        let out = clean(
            r#"<html><head><script src="a.js"></script><meta http-equiv="refresh" content="0;url=http://x">
            <base href="http://evil/"><link rel="preload" href="x"><link rel="stylesheet" href="../-/s.css"></head>
            <body onload="x()"><p onclick="y()">hi</p><iframe src="http://x"></iframe>
            <svg><script>alert(1)</script><a xlink:href="JaVa&#9;script:alert(1)">x</a></svg>
            <a href="java&Tab;script&colon;alert(1)">y</a><a href="&#106avascript&#x3A;alert(1)">z</a>
            <a href=" java
script:alert(1)">bad</a><noscript><img src="lazy.png"></noscript></body></html>"#,
        );
        for bad in ["<script", "onload", "onclick", "<iframe", "refresh", "<base", "preload", "alert(1)", "<noscript"] {
            assert!(!out.to_lowercase().contains(bad), "{bad} survived: {out}");
        }
        assert!(out.contains(r#"href="../-/s.css""#));
        assert!(out.contains(r#"<img src="lazy.png">"#));
        assert!(out.contains("Content-Security-Policy"));
    }

    #[test]
    fn url_policy() {
        let out = clean(
            r#"<body><a href="https://example.org/x" target="_blank">ext</a><img src="http://tracker/p.gif">
            <img src="//cdn/x.png"><img src="data:image/png;base64,AAAA"><a href="data:text/html,x">d</a>
            <a href="Help:Contents">h</a><a href="zim://A/Fire">z</a><a href="../A/Water#Boiling">w</a>
            <img srcset="a.png 1x, https://x/b.png 2x" src="a.png"></body>"#,
        );
        assert!(out.contains(r#"<a data-sd-external="https://example.org/x">"#), "{out}");
        assert!(!out.contains("tracker") && !out.contains("cdn/x.png") && !out.contains("data:text"));
        assert!(out.contains(r#"src="data:image/png;base64,AAAA""#));
        assert!(out.contains(r#"href="./Help:Contents""#));
        assert!(out.contains(r#"href="/A/Fire""#));
        assert!(out.contains(r#"href="../A/Water#Boiling""#));
        assert!(!out.contains("srcset"));
    }

    #[test]
    fn injects_head_into_fragments() {
        let out = clean("<p>bare fragment</p>");
        assert!(out.starts_with("<meta http-equiv"));
        let out = clean("<!doctype html><html><body><p>x</p></body></html>");
        assert!(out.starts_with("<!doctype html><html><meta"));
    }

    #[test]
    fn themes_and_highlight() {
        let opts = RenderOptions { theme: Theme::Dark, font_scale_percent: 130, highlight: vec!["burn".into(), "amp".into()] };
        let out = sanitize("<html><head><title>Burns</title></head><body><p>Treat BURNS &amp; sunburn; burning.</p></body></html>", &opts).unwrap();
        assert!(out.contains("font-size:130%") && out.contains("#121212"));
        assert!(out.contains(r#"<mark class="sd-hl">BURNS</mark> &amp; sunburn; <mark class="sd-hl">burning</mark>"#), "{out}");
        assert!(out.contains("<title>Burns</title>"));
    }

    #[test]
    fn extracts_indexable_text() {
        let e = extract_text(
            r#"<html><head><title>Water &amp; Fire</title><style>p{}</style></head><body><nav>Menu</nav>
            <h1>Purification</h1><p>Boil for 1&nbsp;minute.<sup class="reference">[1]</sup></p>
            <h2>Filters</h2><ul><li>Ceramic</li><li>Sand</li></ul><div class="reflist">Smith 2001</div>
            <script>var x = "nope";</script></body></html>"#,
        );
        assert_eq!(e.title, "Water & Fire");
        assert_eq!(e.headings, "Purification Filters");
        assert_eq!(e.body, "Purification Boil for 1 minute. Filters Ceramic Sand");
    }
}
