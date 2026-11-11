# Mobile shells

Thin native layers over the Rust core (`crates/ffi`, UniFFI). **Status: source
skeletons, not yet compiled.** They were written against the generated
bindings' signatures, but no Xcode or Android SDK was available where this
was authored. Treat them as the Phase 3 starting point, not as tested code.

## What exists

| File | Role |
|---|---|
| `android/.../DiscWebViewClient.kt` | Answers every WebView request from Rust; blocks everything else |
| `android/.../ReaderWebView.kt` | Compose wrapper; JavaScript, file access and network loads off |
| `android/.../DiscRepository.kt` | Opens the disc, coroutine wrappers, SAF side-loading by file descriptor |
| `android/.../AndroidManifest.xml` | Declares no permissions |
| `ios/SurvivalDisc/DiscSchemeHandler.swift` | `WKURLSchemeHandler` for `survival-disc://` |
| `ios/SurvivalDisc/ReaderView.swift` | `WKWebView` with JavaScript off and a non-persistent data store |
| `ios/SurvivalDisc/DiscStore.swift` | Opens the disc, async wrappers, security-scoped side-loading |
| `ios/SurvivalDisc/PrivacyInfo.xcprivacy` | No tracking, no collected data |

Not started: Gradle/Xcode projects, library grid, search UI, settings,
storage and licences screens, asset-pack download flow.

## URL contract

The core serves a dataset under a hierarchical origin so relative links in
articles resolve without rewriting:

- iOS: `survival-disc://<dataset-id>/<url_path>`
- Android: `https://<dataset-id>.disc.invalid/<url_path>`

Android uses an `https` origin because Chromium only guarantees standard
relative-URL resolution for special schemes. `.invalid` is reserved and can
never resolve, and the app has no `INTERNET` permission.

`url_path` values come from the core already percent-encoded (`search`,
`list_titles`, `main_page`, `random_article`). Pass the request's path and
query back to `get_resource` unchanged.

## Threading

All `Disc` methods are synchronous and thread-safe. Call them from
`Dispatchers.IO` or a detached `Task`. WebView resource callbacks already run
off the main thread on Android; the iOS handler dispatches to a concurrent
queue. There is deliberately no single Rust worker thread: it would serialise
the dozens of image requests a page makes.

## Building

```sh
scripts/build-android.sh   # cargo-ndk -> jniLibs + Kotlin bindings
scripts/build-ios.sh       # xcframework + Swift bindings (macOS only)
```

## Packaging the disc

The disc directory (`disc.json`, `*.zim`, `search-index/`, `LICENSES/`) must
reach the device as real files, because the core opens them by path.

- **Android:** use *fast-follow* asset packs and
  `AssetPackLocation.assetsPath()`. Install-time packs are served through
  `AssetManager` only, so using them would mean copying 1.8 GB out on first
  launch and doubling the footprint. Play also caps the size of a single
  pack, so split by dataset (the disc is already one ZIM per dataset). Check
  the current per-pack and total limits before sizing the packs.
- **iOS:** on-demand resources have per-tag size limits and Apple now steers
  new apps toward Background Assets. Decide between the two before building
  the download flow; either way, tag one pack per dataset.

Both stores download the packs themselves, so the app still declares no
network access. The first launch therefore needs connectivity once, to the
store; "works in airplane mode from first launch" holds only after the packs
have arrived.

## Side-loading

- Android: `ACTION_OPEN_DOCUMENT` → `DiscRepository.sideLoad(uri)`. The archive
  is read in place through its descriptor.
- iOS: document picker → `DiscStore.sideLoad(url)`; keep a security-scoped
  bookmark to reopen it.

Side-loaded archives get title search only. Their built-in Xapian index is
not read, and no Tantivy index exists for them.
