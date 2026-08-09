//! Speculatively paginated pages held for adjacent-page navigation.
//!
//! Pagination is the expensive part of turning a page, so the renderer lays out
//! the next page during idle time and keeps a bounded ring of results keyed by
//! frame geometry. This module owns the ring and the two markers that keep the
//! idle-time scheduling from re-running for a page already prefetched; deciding
//! *when* to prefetch stays with the renderer, which knows about the viewport.

use std::collections::VecDeque;

use crate::{FrameGeometryCacheKey, VisibleFrame};

const CAPACITY: usize = 3;

pub(crate) struct PreparedPage {
    pub(crate) key: FrameGeometryCacheKey,
    pub(crate) frame: VisibleFrame,
    pub(crate) reached_end: bool,
    pub(crate) next_start_offset_y: f64,
}

#[derive(Default)]
pub(crate) struct PreparedPageCache {
    pages: VecDeque<PreparedPage>,
    scheduled_for: Option<FrameGeometryCacheKey>,
    prefetched_for: Option<FrameGeometryCacheKey>,
}

impl PreparedPageCache {
    pub(crate) fn new() -> Self {
        Self { pages: VecDeque::with_capacity(CAPACITY), scheduled_for: None, prefetched_for: None }
    }

    pub(crate) fn insert(&mut self, mut prepared: PreparedPage) {
        // Native text-run IDs are handles into backend-owned transient storage.
        // Prepared pages outlive that storage, so only retain portable geometry.
        prepared.frame.clear_backend_shaping();
        if let Some(index) = self.pages.iter().position(|candidate| candidate.key == prepared.key) {
            self.pages.remove(index);
        }
        self.pages.push_back(prepared);
        while self.pages.len() > CAPACITY {
            self.pages.pop_front();
        }
    }

    pub(crate) fn contains(&self, key: FrameGeometryCacheKey) -> bool {
        self.pages.iter().any(|candidate| candidate.key == key)
    }

    pub(crate) fn take(&mut self, key: FrameGeometryCacheKey) -> Option<PreparedPage> {
        let index = self.pages.iter().position(|candidate| candidate.key == key)?;
        self.pages.remove(index)
    }

    /// Takes a page that ends on `end_line` under geometry matching `current`
    /// in everything but page offset and paint direction. Backward navigation
    /// knows which line it wants to land on, not the offset that produces it.
    pub(crate) fn take_ending_at(&mut self, end_line: usize, current: FrameGeometryCacheKey) -> Option<PreparedPage> {
        let index = self.pages.iter().position(|candidate| {
            candidate.frame.current_page_end_line == Some(end_line)
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

    /// Consumes a pending schedule for `key`, marking it prefetched. Returns
    /// whether the caller should now run the prefetch.
    pub(crate) fn claim_scheduled(&mut self, key: FrameGeometryCacheKey) -> bool {
        if self.scheduled_for != Some(key) {
            return false;
        }
        self.scheduled_for = None;
        self.prefetched_for = Some(key);
        true
    }

    pub(crate) fn is_prefetched(&self, key: FrameGeometryCacheKey) -> bool {
        self.prefetched_for == Some(key)
    }

    pub(crate) fn mark_scheduled(&mut self, key: FrameGeometryCacheKey) {
        self.scheduled_for = Some(key);
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.pages.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &PreparedPage> {
        self.pages.iter()
    }
}
