use sd_zim::{Archive, EntryKind, Item, Source, WriterOptions, ZimWriter};

fn build(dir: &std::path::Path, level: i32) -> std::path::PathBuf {
    let mut w = ZimWriter::new(WriterOptions { zstd_level: level, cluster_size: 4096 });
    for i in 0..200 {
        let body = format!("<html><head><title>Article {i}</title></head><body><p>{}</p></body></html>", "water purification ".repeat(40 + i));
        w.add(Item::content(format!("wiki/Article_{i:03}"), format!("Article {i:03}"), "text/html", Source::Bytes(body.into_bytes()), true));
    }
    w.add(Item::content("img/pixel.png", "", "image/png", Source::Bytes(vec![0x89, b'P', b'N', b'G', 1, 2, 3]), false));
    w.add(Item::redirect("wiki/Start", "Start", "wiki/Article_007"));
    w.add(Item::metadata("Title", "Test disc"));
    w.set_main_page("wiki/Article_000");
    let path = dir.join("t.zim");
    let stats = w.finish(&path).unwrap();
    assert_eq!(stats.articles, 200);
    assert!(stats.clusters > 5, "small cluster size must split into many clusters");
    path
}

#[test]
fn write_then_read() {
    let dir = tempfile::tempdir().unwrap();
    for level in [3, 22] {
        let a = Archive::open(build(dir.path(), level)).unwrap();
        assert!(a.has_new_namespace_scheme());
        assert!(a.verify_checksum().unwrap());
        assert_eq!(a.article_count(), 200);
        assert_eq!(a.metadata_str("Title").as_deref(), Some("Test disc"));

        let e = a.get("wiki/Article_150").unwrap().unwrap();
        assert_eq!(a.mime(&e), "text/html");
        assert_eq!(e.title, "Article 150");
        let html = String::from_utf8(a.read(&e).unwrap()).unwrap();
        assert!(html.contains("<title>Article 150</title>"));

        let img = a.get("img/pixel.png").unwrap().unwrap();
        assert_eq!(a.read(&img).unwrap(), [0x89, b'P', b'N', b'G', 1, 2, 3]);

        let r = a.get("wiki/Start").unwrap().unwrap();
        assert!(r.is_redirect());
        assert_eq!(a.resolve(r).unwrap().path, "wiki/Article_007");
        assert_eq!(a.main_page().unwrap().unwrap().path, "wiki/Article_000");
        assert!(a.get("wiki/Missing").unwrap().is_none());

        let hits = a.titles_with_prefix("Article 01", 100).unwrap();
        assert_eq!(hits.len(), 10);
        assert_eq!(hits[0].title, "Article 010");
        assert_eq!(a.article_at(199).unwrap().title, "Article 199");
        assert_eq!(a.titles_with_prefix("Start", 5).unwrap().len(), 0, "redirects are not front articles");
    }
}

#[test]
fn cache_respects_budget() {
    let dir = tempfile::tempdir().unwrap();
    let a = Archive::open(build(dir.path(), 3)).unwrap();
    a.set_cache_budget(16 * 1024);
    for i in 0..200 {
        let e = a.get(&format!("wiki/Article_{i:03}")).unwrap().unwrap();
        a.read(&e).unwrap();
        assert!(a.cache_used() <= 16 * 1024 + 8192, "cache grew to {}", a.cache_used());
    }
}

/// Hand-assembled v5 archive: legacy namespaces, XZ cluster, header title list.
#[test]
fn legacy_xz_archive() {
    let blobs: [&[u8]; 2] = [b"<html><body>Fire</body></html>", b"<html><body>Water</body></html>"];
    let mut raw = Vec::new();
    let mut off = 12u32;
    for len in [blobs[0].len() as u32, blobs[1].len() as u32, 0] {
        raw.extend(off.to_le_bytes());
        off += len;
    }
    raw.extend(blobs[0]);
    raw.extend(blobs[1]);
    let mut cluster = vec![4u8];
    let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
    std::io::Write::write_all(&mut xz, &raw).unwrap();
    cluster.extend(xz.finish().unwrap());

    let dirent = |blob: u32, path: &str| {
        let mut d = vec![0, 0, 0, b'A', 0, 0, 0, 0, 0, 0, 0, 0];
        d.extend(blob.to_le_bytes());
        d.extend(path.as_bytes());
        d.extend([0, 0]);
        d
    };
    let (d0, d1) = (dirent(0, "Fire"), dirent(1, "Water"));
    let mime = b"text/html\0\0";
    let path_ptr = 80 + mime.len() as u64;
    let title_ptr = path_ptr + 16;
    let dir_pos = title_ptr + 8;
    let cluster_ptr = dir_pos + (d0.len() + d1.len()) as u64;
    let cluster_pos = cluster_ptr + 8;
    let checksum = cluster_pos + cluster.len() as u64;

    let mut f = Vec::new();
    f.extend(0x044D_495Au32.to_le_bytes());
    f.extend(5u16.to_le_bytes());
    f.extend(0u16.to_le_bytes());
    f.extend([7u8; 16]);
    f.extend(2u32.to_le_bytes());
    f.extend(1u32.to_le_bytes());
    for v in [path_ptr, title_ptr, cluster_ptr, 80] {
        f.extend(v.to_le_bytes());
    }
    f.extend(1u32.to_le_bytes());
    f.extend(u32::MAX.to_le_bytes());
    f.extend(checksum.to_le_bytes());
    f.extend(mime);
    f.extend(dir_pos.to_le_bytes());
    f.extend((dir_pos + d0.len() as u64).to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(1u32.to_le_bytes());
    f.extend(&d0);
    f.extend(&d1);
    f.extend(cluster_pos.to_le_bytes());
    f.extend(&cluster);
    f.extend([0u8; 16]);

    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("legacy.zim");
    std::fs::write(&p, f).unwrap();
    let a = Archive::open(&p).unwrap();
    assert!(!a.has_new_namespace_scheme());
    let e = a.get("A/Water").unwrap().unwrap();
    assert!(matches!(e.kind, EntryKind::Content { blob: 1, .. }));
    assert_eq!(a.read(&e).unwrap(), blobs[1]);
    assert_eq!(a.user_path(&e), "A/Water");
    assert_eq!(a.main_page().unwrap().unwrap().path, "Water");
    assert_eq!(a.article_count(), 2);
    assert_eq!(a.titles_with_prefix("Fi", 5).unwrap()[0].path, "Fire");
    assert!(a.get("Water").unwrap().is_none());
}

#[test]
fn rejects_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bad.zim");
    std::fs::write(&p, vec![0u8; 200]).unwrap();
    assert!(Archive::open(&p).is_err());
    let good = build(dir.path(), 3);
    let mut bytes = std::fs::read(good).unwrap();
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&p, bytes).unwrap();
    // Truncated archive: either refuses to open or errors on read, never panics.
    if let Ok(a) = Archive::open(&p) {
        for i in 0..a.entry_count() {
            if let Ok(e) = a.entry_at(i) {
                let _ = a.read(&e);
            }
        }
    }
}
