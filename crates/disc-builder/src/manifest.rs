//! The versioned build manifest: the disc's "completeness contract".

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub disc: Disc,
    #[serde(rename = "dataset")]
    pub datasets: Vec<Dataset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disc {
    pub id: String,
    pub title: String,
    pub version: String,
    /// Hard cap for ZIMs + search index + licences.
    pub budget_mb: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    /// SPDX identifier (or `Public-Domain`).
    pub license: String,
    pub attribution: String,
    #[serde(default)]
    pub source_url: String,
    /// Full licence text shipped in `LICENSES/`.
    pub license_file: PathBuf,
    pub budget_mb: Option<u64>,
    /// Set only when a written commercial licence overrides an NC term.
    #[serde(default)]
    pub commercial_license_ack: bool,
    pub source: Source,
    #[serde(default)]
    pub images: Images,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Source {
    /// Re-pack an existing ZIM (mwoffliner, sotoki, gutenberg, zimit output).
    Zim {
        path: PathBuf,
        /// Optional file listing article titles or paths to keep, one per line.
        include: Option<PathBuf>,
    },
    /// A directory of HTML and assets.
    HtmlDir { path: PathBuf },
    /// A directory of UTF-8 `.txt` files, one document each.
    TextDir { path: PathBuf },
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ImageMode {
    Keep,
    Webp,
    Drop,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct Images {
    #[serde(default = "default_mode")]
    pub mode: ImageMode,
    #[serde(default = "default_width")]
    pub max_width: u32,
    #[serde(default = "default_quality")]
    pub quality: f32,
}

fn default_mode() -> ImageMode {
    ImageMode::Webp
}
fn default_width() -> u32 {
    800
}
fn default_quality() -> f32 {
    75.0
}

impl Default for Images {
    fn default() -> Self {
        Self { mode: default_mode(), max_width: default_width(), quality: default_quality() }
    }
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut m: Manifest = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let abs = |p: &mut PathBuf| *p = base.join(&*p);
        let mut ids = std::collections::HashSet::new();
        for d in &mut m.datasets {
            abs(&mut d.license_file);
            match &mut d.source {
                Source::Zim { path, include } => {
                    abs(path);
                    include.as_mut().map(abs);
                }
                Source::HtmlDir { path } | Source::TextDir { path } => abs(path),
            }
            let id_ok = !d.id.is_empty() && d.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !id_ok || !ids.insert(d.id.clone()) {
                bail!("dataset id {:?} must be unique and match [a-z0-9-]+", d.id);
            }
            d.check_license()?;
        }
        if m.datasets.is_empty() {
            bail!("manifest lists no datasets");
        }
        Ok(m)
    }
}

impl Dataset {
    /// The disc is sold, so every dataset needs a licence that allows it.
    fn check_license(&self) -> Result<()> {
        let l = self.license.to_ascii_uppercase();
        if l.trim().is_empty() || self.attribution.trim().is_empty() {
            bail!("dataset {}: license and attribution are mandatory", self.id);
        }
        if !self.license_file.is_file() {
            bail!("dataset {}: licence text {} not found", self.id, self.license_file.display());
        }
        let non_commercial = l.contains("-NC") || l.contains("NONCOMMERCIAL") || l.contains("NON-COMMERCIAL");
        if non_commercial && !self.commercial_license_ack {
            bail!(
                "dataset {}: {} forbids commercial redistribution; remove it or set \
                 commercial_license_ack = true once a written commercial licence exists",
                self.id,
                self.license
            );
        }
        if l.contains("-ND") {
            bail!("dataset {}: {} forbids derivatives; curating or recompressing it is not allowed", self.id, self.license);
        }
        Ok(())
    }
}
