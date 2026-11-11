//! End-to-end: manifest -> disc directory -> reader/search through sd-core.

use std::path::Path;
use std::process::Command;

use sd_core::{Library, RenderOptions};

fn run(manifest: &Path, out: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_disc-builder"))
        .args(["build", manifest.to_str().unwrap(), "--out", out.to_str().unwrap(), "--zstd-level", "19"])
        .output()
        .unwrap()
}

fn write(path: &Path, data: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn fixture(root: &Path) {
    let page = |title: &str, body: &str| {
        format!("<html><head><title>{title}</title><link rel=\"stylesheet\" href=\"css/s.css\"><script src=\"app.js\"></script></head><body><h1>{title}</h1>{body}</body></html>")
    };
    write(&root.join("src/water.html"), page("Water purification", "<p>Boil water for one minute.</p><img src=\"img/photo.jpg\"><a href=\"burns.html\">burns</a>"));
    write(&root.join("src/burns.html"), page("Burn treatment", "<p>Cool the burn under running water.</p><img src=\"img/diagram.png\">"));
    write(&root.join("src/css/s.css"), "body{background:url(../img/bg.png)}");
    write(&root.join("src/app.js"), "alert(1)");
    // Noisy photo: lossy WebP must beat the JPEG. Flat diagram: lossless WebP.
    let photo = image::RgbImage::from_fn(1600, 400, |x, y| image::Rgb([((x * 7) ^ (y * 13)) as u8, (x + y) as u8, (x * y) as u8]));
    std::fs::create_dir_all(root.join("src/img")).unwrap();
    image::DynamicImage::ImageRgb8(photo).save(root.join("src/img/photo.jpg")).unwrap();
    let diagram = image::RgbImage::from_fn(300, 300, |x, _| if x < 150 { image::Rgb([255, 255, 255]) } else { image::Rgb([200, 30, 30]) });
    diagram.save(root.join("src/img/diagram.png")).unwrap();
    diagram.save(root.join("src/img/bg.png")).unwrap();
    write(&root.join("LICENSE.txt"), "Test licence text.");
}

fn manifest(root: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    write(&p, format!("[disc]\nid = \"t\"\ntitle = \"T\"\nversion = \"1\"\n{body}"));
    p
}

const DATASET: &str = r#"
[[dataset]]
id = "notes"
name = "Notes"
attribution = "Test authors"
license_file = "LICENSE.txt"
"#;

#[test]
fn builds_and_serves_a_disc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fixture(root);
    let m = manifest(root, "ok.toml", &format!("budget_mb = 5\n{DATASET}license = \"CC-BY-SA-4.0\"\nsource = {{ type = \"html-dir\", path = \"src\" }}\n"));
    let out = root.join("disc");
    let res = run(&m, &out);
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    assert!(out.join("LICENSES/notes.txt").is_file() && out.join("LICENSES/ATTRIBUTION.md").is_file());

    let lib = Library::open_disc(&out).unwrap();
    let opts = RenderOptions::default();
    let ds = &lib.datasets()[0];
    assert!(ds.bundled && ds.full_text);
    assert_eq!((ds.article_count, ds.license.as_str()), (2, "CC-BY-SA-4.0"));

    let hits = lib.search("boil water", 10).unwrap();
    assert_eq!(hits[0].title, "Water purification");
    assert!(hits[0].snippet.contains("<b>"), "{}", hits[0].snippet);
    let page = lib.resource_for_url("notes", &hits[0].url_path, &opts).unwrap();
    let html = String::from_utf8(page.data).unwrap();
    assert!(html.contains("<mark class=\"sd-hl\">Boil</mark>") && !html.contains("<script"));
    assert!(lib.search("\"unbalanced (query", 5).is_ok());

    // Raster images were re-encoded; paths are unchanged so links still work.
    let photo = lib.resource("notes", "img/photo.jpg", &opts).unwrap();
    assert_eq!(photo.mime, "image/webp");
    assert!(photo.data.len() < std::fs::metadata(root.join("src/img/photo.jpg")).unwrap().len() as usize);
    assert_eq!(&photo.data[..4], b"RIFF");
    assert_eq!(lib.resource("notes", "img/diagram.png", &opts).unwrap().mime, "image/webp");
    assert!(matches!(lib.resource("notes", "app.js", &opts), Err(sd_core::CoreError::NotFound(_))));
    assert!(lib.remove_archive("notes").is_err(), "bundled datasets are not removable");

    // Side-load a second archive: title search only, removable.
    let extra = lib.add_archive(out.join("notes.zim")).unwrap();
    assert_eq!(extra.id, "notes-2");
    assert!(lib.search("burn", 10).unwrap().iter().any(|r| r.dataset_id == "notes-2" && r.snippet.is_empty()));
    lib.remove_archive("notes-2").unwrap();

    // Re-pack the built ZIM narrowed to one article: its stylesheet, the
    // image it shows and the image the stylesheet references come along.
    write(&root.join("keep.txt"), "# essentials\nWater purification\n");
    let m2 = manifest(root, "repack.toml", &format!("budget_mb = 5\n{DATASET}license = \"CC-BY-SA-4.0\"\nimages = {{ mode = \"keep\" }}\nsource = {{ type = \"zim\", path = \"disc/notes.zim\", include = \"keep.txt\" }}\n"));
    let out2 = root.join("disc2");
    let res = run(&m2, &out2);
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    let lib2 = Library::open_disc(&out2).unwrap();
    for present in ["water.html", "css/s.css", "img/photo.jpg", "img/bg.png"] {
        assert!(lib2.resource("notes", present, &opts).is_ok(), "{present} missing after repack");
    }
    for absent in ["burns.html", "img/diagram.png"] {
        assert!(lib2.resource("notes", absent, &opts).is_err(), "{absent} should have been dropped");
    }
    assert_eq!(lib2.search("cool running", 10).unwrap().len(), 0);

    write(&root.join("keep.txt"), "Water purification\nNo such article\n");
    let res = run(&m2, &root.join("disc3"));
    assert!(!res.status.success() && String::from_utf8_lossy(&res.stderr).contains("not found"));
}

#[test]
fn enforces_license_and_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fixture(root);
    let src = "source = { type = \"html-dir\", path = \"src\" }\n";

    let nc = manifest(root, "nc.toml", &format!("budget_mb = 5\n{DATASET}license = \"CC-BY-NC-SA-3.0\"\n{src}"));
    let res = run(&nc, &root.join("o1"));
    assert!(!res.status.success());
    assert!(String::from_utf8_lossy(&res.stderr).contains("forbids commercial redistribution"));

    let tight = manifest(root, "tight.toml", &format!("budget_mb = 5\n{DATASET}license = \"CC0-1.0\"\nbudget_mb = 0\n{src}"));
    let res = run(&tight, &root.join("o2"));
    assert!(!res.status.success());
    assert!(String::from_utf8_lossy(&res.stderr).contains("size budget exceeded"));
}
