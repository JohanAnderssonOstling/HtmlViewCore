//! Bounded caches for spine documents the reader may navigate back to.
//!
//! Two LRU maps keyed by spine index share one capacity: fully laid-out
//! documents, and parsed DOMs kept only to answer footnote previews. Neither
//! depends on any other renderer state, so eviction policy lives here rather
//! than being spread across navigation.

use std::collections::{HashMap, VecDeque};

use html::engine::Engine;
use html::pipeline::PipelineInputs;

use crate::LaidOutDocument;

pub(crate) struct CachedDocument {
    pub(crate) session: Engine,
    pub(crate) inputs: PipelineInputs,
    pub(crate) document: LaidOutDocument,
}

pub(crate) struct FootnoteDocumentIndex {
    pub(crate) document: scraper::Html,
}

impl FootnoteDocumentIndex {
    pub(crate) fn parse(source: &str) -> Self {
        Self { document: scraper::Html::parse_document(source) }
    }
}

pub(crate) struct DocumentCache {
    documents: HashMap<usize, CachedDocument>,
    document_order: VecDeque<usize>,
    footnotes: HashMap<usize, FootnoteDocumentIndex>,
    footnote_order: VecDeque<usize>,
    capacity: usize,
}

impl DocumentCache {
    pub(crate) fn new(capacity: usize, current_doc: usize, footnotes: FootnoteDocumentIndex) -> Self {
        Self {
            documents: HashMap::new(),
            document_order: VecDeque::new(),
            footnotes: HashMap::from([(current_doc, footnotes)]),
            footnote_order: VecDeque::from([current_doc]),
            capacity,
        }
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.capacity == 0
    }

    pub(crate) fn insert_document(&mut self, doc_index: usize, cached: CachedDocument) {
        if self.capacity == 0 {
            return;
        }

        if self.documents.contains_key(&doc_index) {
            self.document_order.retain(|cached_index| *cached_index != doc_index);
        }

        self.documents.insert(doc_index, cached);
        self.document_order.push_back(doc_index);

        while self.document_order.len() > self.capacity {
            if let Some(evicted) = self.document_order.pop_front() {
                self.documents.remove(&evicted);
            }
        }
    }

    pub(crate) fn take_document(&mut self, doc_index: usize) -> Option<CachedDocument> {
        self.document_order.retain(|cached_index| *cached_index != doc_index);
        self.documents.remove(&doc_index)
    }

    pub(crate) fn insert_footnotes(&mut self, doc_index: usize, index: FootnoteDocumentIndex) {
        self.footnote_order.retain(|candidate| *candidate != doc_index);
        self.footnotes.insert(doc_index, index);
        self.footnote_order.push_back(doc_index);
        // Footnote previews stay available even when document caching is off,
        // so this bound never collapses to zero.
        while self.footnote_order.len() > self.capacity.max(1) {
            if let Some(evicted) = self.footnote_order.pop_front() {
                self.footnotes.remove(&evicted);
            }
        }
    }

    pub(crate) fn footnotes(&self, doc_index: usize) -> Option<&FootnoteDocumentIndex> {
        self.footnotes.get(&doc_index)
    }

    pub(crate) fn has_footnotes(&self, doc_index: usize) -> bool {
        self.footnotes.contains_key(&doc_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(id: &str) -> FootnoteDocumentIndex {
        FootnoteDocumentIndex::parse(&format!("<p id='{id}'>note</p>"))
    }

    fn cache(capacity: usize) -> DocumentCache {
        DocumentCache::new(capacity, 0, index("doc0"))
    }

    #[test]
    fn the_starting_document_is_indexed_for_footnotes() {
        let cache = cache(3);
        assert!(cache.has_footnotes(0));
        assert!(!cache.has_footnotes(1));
    }

    #[test]
    fn footnote_indexes_evict_oldest_first() {
        let mut cache = cache(2);
        cache.insert_footnotes(1, index("doc1"));
        cache.insert_footnotes(2, index("doc2"));

        assert!(!cache.has_footnotes(0), "the oldest index is evicted at capacity");
        assert!(cache.has_footnotes(1));
        assert!(cache.has_footnotes(2));
    }

    #[test]
    fn reinserting_a_footnote_index_refreshes_its_position() {
        let mut cache = cache(2);
        cache.insert_footnotes(0, index("doc0-again"));
        cache.insert_footnotes(1, index("doc1"));

        assert!(cache.has_footnotes(0), "reinsertion moves an entry to the newest slot rather than duplicating it");
        assert!(cache.has_footnotes(1));
    }

    #[test]
    fn footnote_previews_survive_disabled_document_caching() {
        // Capacity zero disables document caching, but footnote previews must
        // keep working, so their bound floors at one rather than collapsing.
        let mut cache = cache(0);
        assert!(cache.is_disabled());
        assert!(cache.has_footnotes(0));

        cache.insert_footnotes(1, index("doc1"));
        assert!(cache.has_footnotes(1), "the current document's notes stay available");
        assert!(!cache.has_footnotes(0), "but only one at a time");
    }

    #[test]
    fn a_disabled_cache_stores_no_documents() {
        let mut cache = cache(0);
        assert!(cache.take_document(0).is_none());
    }

    #[test]
    fn footnote_lookup_returns_the_parsed_document() {
        let cache = cache(2);
        let indexed = cache.footnotes(0).expect("the starting document is indexed");
        let selector = scraper::Selector::parse("#doc0").expect("static selector must parse");
        assert!(indexed.document.select(&selector).next().is_some(), "the stored index must be the parsed source, not an empty document");
    }
}
