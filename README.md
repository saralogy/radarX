# Survival Disc

An offline knowledge reader sold as a single purchase: a curated 1–2 GB
library of ZIM archives, instant full-text search, no network access at all.
The core is Rust; iOS and Android get thin native shells over it.

**Status: Phases 0–2 implemented and tested on desktop. Phase 3 (the apps)
is skeleton source only.** See [What is and is not done](#what-is-and-is-not-done).

## The content decision

A full Wikipedia does not fit in 2 GB under any compression, so the disc is a
set of *complete curated datasets*, not a slice of everything:

| Dataset | Source | Licence | Budget |
|---|---|---|---|
| WikiMed medical encyclopedia | Kiwix ZIM, whole | CC BY-SA 4.0 | 950 MB |
| Wikipedia Essentials (~5,000 listed articles) | Kiwix ZIM, include list | CC BY-SA 4.0 | 350 MB |
| Stack Exchange: Outdoors, Home Improvement | Kiwix ZIMs, whole | CC BY-SA 4.0 | 180 MB |
| Classic books | cleared public-domain texts | Public domain | 120 MB |
| US field manuals and preparedness guides | US-government works | Public domain | 60 MB |
| Search index | built by the pipeline | n/a | ~140 MB |
| **Total** | | | **1,800 MB** |

"Complete" is a checkable claim: [`discs/survival-essentials-v1.toml`](discs/survival-essentials-v1.toml)
lists every dataset and article, and the build fails if a listed article is
missing or a budget is exceeded. The figures above are allocations; the
first full build against the real sources yields the measurements. The app
stays a general ZIM reader, so users can side-load archives of any size.

WikiHow is excluded. Its CC BY-NC-SA licence forbids a paid product, and
the builder refuses NC and ND licences outright.

## Layout

```
crates/zim           sd-zim: pure-Rust ZIM reader and writer (no C++ libzim)
crates/core          sd-core: dataset library, HTML sanitiser, Tantivy search
crates/ffi           sd-ffi: UniFFI bindings (Swift, Kotlin)
crates/disc-builder  content pipeline and inspection CLI
discs/               production manifest, include lists, licence texts
examples/sample-disc small in-repo disc that exercises the whole pipeline
mobile/              Android and iOS shell skeletons
scripts/             cross-compile and binding generation
```

## Try it

```sh
cargo test --workspace

cargo run --release -p disc-builder -- build examples/sample-disc/manifest.toml --out out/sample
cargo run --release -p disc-builder -- search out/sample "boiling water"
cargo run --release -p disc-builder -- inspect out/sample/field-notes.zim --verify
cargo run --release -p disc-builder -- cat out/sample/field-notes.zim water.html --sanitized
cargo run --release -p disc-builder -- bench out/sample
```

`inspect`, `ls`, `cat` and `bench` accept any ZIM file, including ones
downloaded from Kiwix.

## How it works

**Reading.** `sd-zim` parses the header, MIME list, pointer lists and
directory entries with positioned reads, so one archive handle serves many
threads without locking a file cursor. Clusters are decompressed on demand
(Zstandard; XZ for pre-2021 archives) into a byte-budgeted LRU cache, 64 MiB
by default. Uncompressed clusters, which hold images, are never loaded:
blobs are read straight from the file.

**Rendering.** Articles are served under a hierarchical origin
(`survival-disc://<dataset>/<path>`), so relative links and images resolve in
the WebView unchanged. The sanitiser removes scripts, event handlers, frames,
`<base>`, meta refresh and every remote reference, defuses `javascript:` and
`data:` URLs including entity-encoded forms, and injects a
Content-Security-Policy plus the reader stylesheet (light, dark, sepia, font
scale). The shells also disable JavaScript in the WebView, and the app holds
no network permission, so three independent layers each block exfiltration.

**Search.** The pipeline builds one Tantivy index for the whole disc: titles,
headings and body text, stemmed, stop-word filtered and ASCII-folded. Body
text is indexed but not stored and has no positions; snippets are generated
at query time from the article in the ZIM. That keeps the index small at the
cost of decompressing up to ten clusters per query. Side-loaded archives
fall back to title-prefix search.

**Building.** `disc-builder build` reads the manifest and, per dataset:
ingests the source (an existing ZIM, a directory of HTML, or a directory of
plain text), narrows it to the include list plus the images and stylesheets
those articles reference, re-encodes raster images to WebP, and writes a ZIM
with Zstandard level 22 clusters compressed in parallel. It then indexes the
written archives through the reader, copies licence texts, generates
`LICENSES/ATTRIBUTION.md`, writes `disc.json` and checks every budget.

## Measurements

Kiwix downloads were blocked where this was built, so the scale test used a
stand-in corpus: Rust's HTML documentation, 61,806 pages and 598 MB. Machine:
4-core x86-64 server, release build. **None of this is a phone measurement.**

| Zstandard level | ZIM size | ZIM write time | Cluster decompress p50 / p95 |
|---|---|---|---|
| 3 | 48.8 MB | 15 s | 0.72 / 1.97 ms |
| 19 | 41.0 MB | 77 s | 0.77 / 2.67 ms |
| 22 | 40.8 MB | 243 s | 0.71 / 2.31 ms |

| Metric | Result |
|---|---|
| Search index | 10.3 MB for 99.1 MB of extracted text (10.4%), built in 7 s |
| Search, top 10 with snippets, 62k documents | p50 10.5 ms, p95 13–18 ms |
| Random article read + sanitise | p50 0.9 ms, p95 2.5 ms |
| Builder memory at level 22 | about 310 MB resident, sampled mid-build |
| Core library (`libsurvival_disc.so`, stripped) | 4.5 MB |

What carries over and what does not:

- Decompression time does not depend on the compression level. Level 22 is
  free at read time.
- Level 22 saved 0.5% over level 19 here for three times the build time.
  Use `--zstd-level 19` while iterating and 22 for release builds; re-check
  the gain on the real corpus before paying for it.
- The index-to-text ratio landed on the 10% target.
- The 7% compression ratio will not carry over. API documentation is far
  more repetitive than encyclopedia prose.
- Reproduce with `disc-builder bench <disc-dir>`.

## Where this departs from the brief

- **ZIM parser written from scratch** rather than forking a third-party
  crate. The format is small, and owning it gave positioned reads, the cache
  design and a matching writer. It is cross-checked against the reference
  libzim in both directions.
- **Not `no_std`.** The core is file I/O, mmap and threads throughout.
- **Zstandard level 22 does not slow reading.** Decompression speed is
  nearly independent of the level used to compress; see the measurements.
  The real cost of level 22 is build time. The "fall back to 19 if slow"
  rule is therefore unnecessary for latency.
- **No remote URLs are rewritten to a custom scheme**; they are removed.
  Internal links need no rewriting because of the hierarchical origin.
- **Synchronous FFI.** No dedicated Rust worker thread: a single worker
  would serialise a page's image requests. The shells call from background
  threads.
- **MediaWiki XML and Stack Exchange XML dumps are not ingested directly.**
  Rendering wikitext to HTML needs MediaWiki itself. The pipeline takes the
  ZIMs that Kiwix's mwoffliner and sotoki already produce and re-packs them.
- **Android uses fast-follow asset packs**, not install-time, because the
  core needs real file paths. Details in [`mobile/README.md`](mobile/README.md).

## What is and is not done

Done and tested here:

- ZIM reader and writer, including legacy namespaces and XZ clusters
- Cross-validation with libzim 9: it opens and checksums our archives, and
  our reader opens archives it writes
- Sanitiser, text extraction, search, snippets, highlighting
- Pipeline end to end, licence and budget gates, WebP transcoding
- UniFFI bindings generate for Swift and Kotlin

Not done:

- **The apps.** `mobile/` is uncompiled skeleton code. No UI beyond the
  reader view, no asset-pack flow, no store listings.
- **No cross-compilation** to iOS or Android targets was attempted.
- **No device benchmarks.** Every number above is from an x86-64 server.
  Acceptance criteria 1, 2 and 8 need a mid-range phone.
- **No real content build.** The source ZIMs could not be downloaded here,
  so the size budget is unproven against WikiMed and the other sources.
- **The Essentials list is a 42-title seed**, not the ~5,000-title v1 list.
- Split ZIM files (`.zimaa`, `.zimab`) are not supported.
- Full-text search of side-loaded archives (reading their Xapian index, or
  indexing on device) is not implemented.

## Licensing notes for the bundle

These are engineering guardrails, not legal advice; have counsel review
before sale.

- CC BY-SA content needs attribution, a licence copy and a statement of
  changes. The builder emits all three per dataset. Share-alike covers the
  content, which stays freely copyable; it does not reach the app's code.
- Project Gutenberg's licence restricts commercial use of files carrying its
  name. The text importer strips the Gutenberg header and footer, leaving
  the public-domain text. Each title still needs a per-jurisdiction check
  (US, EU, UK).
- Stack Exchange requires each post to credit its author and link back.
  Confirm the source ZIM's pages carry both before shipping.
