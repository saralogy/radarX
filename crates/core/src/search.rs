//! Tantivy full-text index: built by the content pipeline, queried on device.
//!
//! Article bodies are indexed but not stored. Snippets are generated at query
//! time from the ZIM itself, which keeps the index near 10% of the text size.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer,
};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};

use crate::{CoreError, Result};

const TOKENIZER: &str = "sd_en";

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Search(e.to_string())
}

#[derive(Clone, Copy)]
struct Fields {
    dataset: Field,
    path: Field,
    title: Field,
    headings: Field,
    body: Field,
}

fn schema() -> (Schema, Fields) {
    let text = |record| {
        TextOptions::default()
            .set_indexing_options(TextFieldIndexing::default().set_tokenizer(TOKENIZER).set_index_option(record))
    };
    let mut b = Schema::builder();
    let fields = Fields {
        dataset: b.add_text_field("dataset", STRING | STORED),
        path: b.add_text_field("path", STORED),
        title: b.add_text_field("title", text(IndexRecordOption::WithFreqs).set_stored()),
        headings: b.add_text_field("headings", text(IndexRecordOption::WithFreqs)),
        // No positions: phrase queries are not offered, and positions would
        // roughly double the index.
        body: b.add_text_field("body", text(IndexRecordOption::WithFreqs)),
    };
    (b.build(), fields)
}

fn register_tokenizer(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(StopWordFilter::new(Language::English).expect("English stop words are built in"))
        .filter(Stemmer::new(Language::English))
        .build();
    index.tokenizers().register(TOKENIZER, analyzer);
}

/// Pipeline-side index writer.
pub struct IndexBuilder {
    index: Index,
    writer: IndexWriter,
    fields: Fields,
}

impl IndexBuilder {
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = schema();
        let index = Index::create_in_dir(dir, schema).map_err(err)?;
        register_tokenizer(&index);
        let writer = index.writer(256 << 20).map_err(err)?;
        Ok(Self { index, writer, fields })
    }

    /// Thread-safe; call from parallel extractors.
    pub fn add(&self, dataset: &str, path: &str, title: &str, headings: &str, body: &str) -> Result<()> {
        let f = self.fields;
        self.writer
            .add_document(doc!(f.dataset => dataset, f.path => path, f.title => title, f.headings => headings, f.body => body))
            .map_err(err)?;
        Ok(())
    }

    /// Commit and merge down to one segment for the fastest cold queries.
    pub fn finish(mut self) -> Result<u64> {
        self.writer.commit().map_err(err)?;
        let segments = self.index.searchable_segment_ids().map_err(err)?;
        if segments.len() > 1 {
            self.writer.merge(&segments).wait().map_err(err)?;
        }
        self.writer.garbage_collect_files().wait().map_err(err)?;
        self.writer.wait_merging_threads().map_err(err)?;
        let reader = self.index.reader().map_err(err)?;
        Ok(reader.searcher().num_docs())
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub dataset: String,
    pub path: String,
    pub title: String,
    pub score: f32,
}

pub struct Hits {
    pub hits: Vec<Hit>,
    /// Normalised query words, for in-article highlighting.
    pub terms: Vec<String>,
    snippets: Option<SnippetGenerator>,
}

impl Hits {
    /// HTML snippet (`<b>` around matches) from plain article text.
    pub fn snippet(&self, text: &str) -> Option<String> {
        let s = self.snippets.as_ref()?.snippet(text);
        (!s.is_empty()).then(|| s.to_html())
    }
}

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

/// Reduce user input to bare words so query syntax can never error.
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(16)
        .map(|w| w.to_lowercase())
        .collect()
}

impl SearchIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir).map_err(err)?;
        register_tokenizer(&index);
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().map_err(err)?;
        let (_, expected) = schema();
        let s = index.schema();
        let field = |n| s.get_field(n).map_err(|_| CoreError::Search(format!("index is missing field {n}")));
        let fields = Fields {
            dataset: field("dataset")?,
            path: field("path")?,
            title: field("title")?,
            headings: field("headings")?,
            body: field("body")?,
        };
        debug_assert_eq!(fields.body, expected.body);
        Ok(Self { index, reader, fields })
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    fn parse(&self, text: &str, conjunction: bool) -> Result<Box<dyn Query>> {
        let f = self.fields;
        let mut p = QueryParser::for_index(&self.index, vec![f.title, f.headings, f.body]);
        p.set_field_boost(f.title, 5.0);
        p.set_field_boost(f.headings, 2.0);
        if conjunction {
            p.set_conjunction_by_default();
        }
        p.parse_query(text).map_err(err)
    }

    /// All-terms match first; falls back to any-term when that finds nothing.
    pub fn search(&self, query: &str, limit: usize) -> Result<Hits> {
        let terms = query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Hits { hits: Vec::new(), terms, snippets: None });
        }
        let text = terms.join(" ");
        let searcher = self.reader.searcher();
        let mut query = self.parse(&text, true)?;
        let mut top = searcher.search(&query, &TopDocs::with_limit(limit)).map_err(err)?;
        if top.is_empty() && terms.len() > 1 {
            query = self.parse(&text, false)?;
            top = searcher.search(&query, &TopDocs::with_limit(limit)).map_err(err)?;
        }
        let f = self.fields;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr).map_err(err)?;
            let get = |field| d.get_first(field).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            hits.push(Hit { dataset: get(f.dataset), path: get(f.path), title: get(f.title), score });
        }
        let snippets = SnippetGenerator::create(&searcher, &*query, f.body).ok().map(|mut g| {
            g.set_max_num_chars(160);
            g
        });
        Ok(Hits { hits, terms, snippets })
    }
}
