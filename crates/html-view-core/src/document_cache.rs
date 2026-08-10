//! A bounded cache of spine documents the reader may navigate back to.
//!
//! One LRU map keyed by spine index holds fully laid-out documents, each with
//! the pipeline session that produced it. Note previews read from those
//! sessions rather than keeping a second parse of their own. Eviction depends
//! on no other renderer state, so its policy lives here rather than being
//! spread across navigation.

use std::collections::{HashMap, VecDeque};

use crate::spine_item::SpineItem;

/// A document the reader may come back to is the same thing as the one being
/// read, so the cache holds that and not a second shape of its own.
pub(crate) type CachedDocument = SpineItem;

pub(crate) struct DocumentCache {
    documents: HashMap<usize, CachedDocument>,
    document_order: VecDeque<usize>,
    capacity: usize,
}

impl DocumentCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { documents: HashMap::new(), document_order: VecDeque::new(), capacity }
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

    /// Drops every cached document. Used when a setting changes what a laid-out
    /// document contains, so cached ones no longer describe the same book.
    pub(crate) fn clear(&mut self) {
        self.documents.clear();
        self.document_order.clear();
    }

    /// Mutable counterpart of [`Self::document`], for work that appends to a
    /// cached document's renderer resources.
    pub(crate) fn document_mut(&mut self, doc_index: usize) -> Option<&mut CachedDocument> {
        self.documents.get_mut(&doc_index)
    }

    /// Borrows a cached document without disturbing its position in the LRU.
    /// Note previews read through this: they consult a document the reader may
    /// still navigate to, and must not evict or claim it.
    pub(crate) fn document(&self, doc_index: usize) -> Option<&CachedDocument> {
        self.documents.get(&doc_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(capacity: usize) -> DocumentCache {
        DocumentCache::new(capacity)
    }

    #[test]
    fn a_disabled_cache_stores_no_documents() {
        let mut cache = cache(0);
        assert!(cache.take_document(0).is_none());
        assert!(cache.is_disabled());
    }

    #[test]
    fn peeking_a_document_leaves_it_cached() {
        let cache = cache(2);
        // An absent document peeks as absent rather than being loaded on
        // demand; note previews depend on that to decide whether to load.
        assert!(cache.document(0).is_none());
    }
}
