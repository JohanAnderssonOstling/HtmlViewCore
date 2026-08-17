//! Previously visited pages held for navigation.
//!
//! The renderer never populates this cache speculatively. It stores a bounded
//! ring of pages that were actually visible so backward navigation can reuse
//! their geometry.

use std::collections::VecDeque;

use crate::document_view::DocumentView;
use crate::{FrameGeometryCacheKey, ViewportContinuation, ViewportState, VisibleFrame};
#[cfg(test)]
use crate::{PagePositions, VisibleLinePositions};

const CAPACITY: usize = 3;

pub(crate) struct PreparedPage {
    pub(crate) key: FrameGeometryCacheKey,
    pub(crate) frame: VisibleFrame,
    pub(crate) continuation: ViewportContinuation,
}

impl PreparedPage {
    pub(crate) fn install(self, view: &mut DocumentView, viewport: &mut ViewportState) {
        viewport.install_forward_page(self.frame.page().paint_start_offset_y(), self.continuation);
        view.frame = self.frame;
    }
}

#[derive(Default)]
pub(crate) struct PreparedPageCache {
    pages: VecDeque<PreparedPage>,
}

impl PreparedPageCache {
    pub(crate) fn new() -> Self {
        Self {
            pages: VecDeque::with_capacity(CAPACITY),
        }
    }

    pub(crate) fn insert(&mut self, mut prepared: PreparedPage) {
        // Native text-run IDs are handles into backend-owned transient storage.
        // Prepared pages outlive that storage, so only retain portable geometry.
        prepared.frame.clear_backend_shaping();
        if let Some(index) = self
            .pages
            .iter()
            .position(|candidate| candidate.key == prepared.key)
        {
            self.pages.remove(index);
        }
        self.pages.push_back(prepared);
        while self.pages.len() > CAPACITY {
            self.pages.pop_front();
        }
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: FrameGeometryCacheKey) -> bool {
        self.pages.iter().any(|candidate| candidate.key == key)
    }

    pub(crate) fn take(&mut self, key: FrameGeometryCacheKey) -> Option<PreparedPage> {
        let index = self
            .pages
            .iter()
            .position(|candidate| candidate.key == key)?;
        self.pages.remove(index)
    }

    /// Takes a page that ends on `end_line` under geometry matching `current`
    /// in everything but page offset and paint direction. Backward navigation
    /// knows which line it wants to land on, not the offset that produces it.
    pub(crate) fn take_ending_at(
        &mut self,
        end_line: usize,
        current: FrameGeometryCacheKey,
    ) -> Option<PreparedPage> {
        let index = self.pages.iter().position(|candidate| {
            candidate.frame.page().end_line() == Some(end_line)
                && candidate.key.layout_revision == current.layout_revision
                && candidate.key.viewport_width_bits == current.viewport_width_bits
                && candidate.key.viewport_height_bits == current.viewport_height_bits
                && candidate.key.column_width_bits == current.column_width_bits
                && candidate.key.column_gap_bits == current.column_gap_bits
                && candidate.key.column_count == current.column_count
                && candidate.key.scale_bits == current.scale_bits
        })?;
        self.pages.remove(index)
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.pages.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page_offset: u64) -> FrameGeometryCacheKey {
        FrameGeometryCacheKey {
            layout_revision: 1,
            viewport_width_bits: 800f64.to_bits(),
            viewport_height_bits: 600f64.to_bits(),
            column_width_bits: 400f64.to_bits(),
            column_gap_bits: 20f64.to_bits(),
            column_count: 2,
            scale_bits: 1f64.to_bits(),
            page_offset_bits: page_offset,
            direction: 0,
        }
    }

    fn page(key: FrameGeometryCacheKey, end_line: Option<usize>) -> PreparedPage {
        let mut frame = VisibleFrame::default();
        frame.install_flow_page(
            PagePositions {
                start_line: 0,
                last_line: end_line,
                positions: VisibleLinePositions::default(),
                offset_breaks: Vec::new(),
                repeated_table_headers: Vec::new(),
                block_decoration_indices: Vec::new(),
                reached_end: false,
                next_start_offset_y: 0.0,
            },
            0.0,
            end_line,
        );
        PreparedPage {
            key,
            frame,
            continuation: ViewportContinuation::MoreAt(0.0),
        }
    }

    #[test]
    fn taking_a_page_removes_it() {
        let mut cache = PreparedPageCache::new();
        cache.insert(page(key(1), None));

        assert!(cache.contains(key(1)));
        assert!(cache.take(key(1)).is_some());
        assert!(!cache.contains(key(1)), "a page is consumed by being taken");
        assert!(cache.take(key(1)).is_none());
    }

    #[test]
    fn a_page_for_different_geometry_is_not_reused() {
        let mut cache = PreparedPageCache::new();
        cache.insert(page(key(1), None));

        let mut other = key(1);
        other.scale_bits = 2f64.to_bits();
        assert!(
            !cache.contains(other),
            "geometry that would paginate differently must not hit"
        );
        assert!(cache.take(other).is_none());
    }

    #[test]
    fn the_oldest_page_is_evicted_past_capacity() {
        let mut cache = PreparedPageCache::new();
        for offset in 0..CAPACITY as u64 + 1 {
            cache.insert(page(key(offset), None));
        }

        assert!(!cache.contains(key(0)), "the oldest page is evicted");
        for offset in 1..CAPACITY as u64 + 1 {
            assert!(
                cache.contains(key(offset)),
                "page {offset} should still be cached"
            );
        }
    }

    #[test]
    fn reinserting_a_key_replaces_rather_than_duplicates() {
        let mut cache = PreparedPageCache::new();
        cache.insert(page(key(1), Some(10)));
        cache.insert(page(key(1), Some(20)));

        let restored = cache.take(key(1)).expect("the page is cached");
        assert_eq!(
            restored.frame.page().end_line(),
            Some(20),
            "the newer page wins"
        );
        assert!(cache.take(key(1)).is_none(), "no duplicate is left behind");
    }

    #[test]
    fn insertion_discards_backend_shaping_handles() {
        let mut cache = PreparedPageCache::new();
        let mut prepared = page(key(1), None);
        prepared.frame.record_native_shape_failure(3);

        cache.insert(prepared);

        let restored = cache.take(key(1)).expect("the page is cached");
        assert!(
            restored.frame.shaping().shaped_lines().is_empty(),
            "shaped lines index backend-owned storage that a cached page outlives"
        );
        assert!(restored.frame.shaping().native_failures().is_empty());
        assert!(restored.frame.shaping().key().is_none());
    }

    #[test]
    fn a_page_can_be_found_by_the_line_it_ends_on() {
        let mut cache = PreparedPageCache::new();
        cache.insert(page(key(1), Some(42)));

        // Backward navigation knows the line it wants to land on, not the
        // offset that produces it, so page_offset_bits must not participate.
        let mut current = key(1);
        current.page_offset_bits = 999;
        assert!(cache.take_ending_at(42, current).is_some());
    }

    #[test]
    fn ending_line_lookup_still_respects_geometry() {
        let mut cache = PreparedPageCache::new();
        cache.insert(page(key(1), Some(42)));

        let mut different = key(1);
        different.column_count = 3;
        assert!(
            cache.take_ending_at(42, different).is_none(),
            "a page laid out for another column count cannot be reused"
        );
        assert!(
            cache.take_ending_at(41, key(1)).is_none(),
            "the end line must match exactly"
        );
    }
}
