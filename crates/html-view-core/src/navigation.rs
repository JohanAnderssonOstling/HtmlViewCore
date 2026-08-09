//! Navigation state: where the reader is in the publication, how it got there,
//! and what the host has already been told about it.
//!
//! The pieces here are deliberately separate. Change-detection markers, the
//! location history stack and the table-of-contents anchor filter share nothing
//! but the spine index they are keyed by, and each is exercised by a different
//! part of navigation.

use std::collections::HashSet;

use crate::doc_query::DocQuery;
use crate::{RendererEvent, RendererHost};

/// A value the host has already been told about.
///
/// Every outbound navigation signal is deduplicated: the host is notified only
/// when the computed value differs from the last one sent. Owning the compare,
/// the store and the emit together means a caller cannot emit without updating
/// the marker, which is the failure mode this type exists to prevent.
pub struct Debounced<T> {
    last: Option<T>,
}

impl<T> Default for Debounced<T> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<T: Clone + PartialEq> Debounced<T> {
    /// Emits `event(value)` if `value` differs from the last emitted one.
    /// Returns whether anything was sent.
    pub fn emit_if_changed(&mut self, value: T, host: &dyn RendererHost, event: impl FnOnce(T) -> RendererEvent) -> bool {
        if self.last.as_ref() == Some(&value) {
            return false;
        }
        self.last = Some(value.clone());
        host.emit(event(value));
        true
    }

    /// Forgets what was last sent, so the next value is emitted even if it is
    /// unchanged.
    pub fn invalidate(&mut self) {
        self.last = None;
    }
}

/// The four signals emitted together whenever the reading position moves.
#[derive(Default)]
pub struct NavSignals {
    pub position: Debounced<(usize, Option<u32>)>,
    pub cfi: Debounced<Option<String>>,
    pub progress: Debounced<(u64, u64, usize)>,
    pub anchor: Debounced<(usize, Option<String>)>,
}

/// A place the reader has been: a spine index and an optional anchor within it.
pub type Location = (usize, Option<String>);

/// Back/forward stack for reader-initiated jumps.
///
/// The cursor addresses the current entry rather than the next free slot, so
/// pushing truncates everything after it, exactly like a browser history.
#[derive(Default)]
pub struct LocationHistory {
    entries: Vec<Location>,
    index: usize,
    availability: Debounced<(bool, bool)>,
}

impl LocationHistory {
    fn availability(&self) -> (bool, bool) {
        let len = self.entries.len();
        (len > 0 && self.index > 0, len > 0 && self.index + 1 < len)
    }

    pub fn emit_availability(&mut self, host: &dyn RendererHost) {
        let (back, forward) = self.availability();
        self.availability.emit_if_changed((back, forward), host, |(back, forward)| RendererEvent::HistoryAvailability { back, forward });
    }

    pub fn current(&self) -> Option<&Location> {
        self.entries.get(self.index)
    }

    /// Records `location` unless it is already the current entry. Anything
    /// after the cursor is discarded.
    pub fn push(&mut self, location: Location, host: &dyn RendererHost) {
        if self.current() != Some(&location) {
            self.entries.truncate(self.index + 1);
            self.entries.push(location);
            self.index = self.entries.len().saturating_sub(1);
        }
        self.emit_availability(host);
    }

    /// Whether the cursor sits on the newest entry, i.e. nothing to go forward
    /// to. Vacuously true for an empty stack.
    #[cfg(test)]
    pub fn is_at_newest(&self) -> bool {
        !self.availability().1
    }

    /// Moves the cursor by `step` and returns the location to navigate to, or
    /// `None` if that would leave the stack.
    pub fn step(&mut self, step: isize, host: &dyn RendererHost) -> Option<Location> {
        let next = self.index as isize + step;
        if next < 0 || next >= self.entries.len() as isize {
            self.emit_availability(host);
            return None;
        }
        self.index = next as usize;
        Some(self.entries[self.index].clone())
    }
}

/// Anchor ids the table of contents points at, per spine document.
///
/// Reader position reporting names the nearest preceding anchor. Restricting
/// that to anchors the table of contents actually references keeps the reported
/// location meaningful instead of snapping to arbitrary ids in the markup.
#[derive(Default)]
pub struct TocAnchors {
    strings_by_doc: Vec<Vec<String>>,
    filter: Option<HashSet<u16>>,
}

impl TocAnchors {
    pub fn filter(&self) -> Option<&HashSet<u16>> {
        self.filter.as_ref()
    }

    pub fn set_strings_by_doc(&mut self, mut by_doc: Vec<Vec<String>>, document_count: usize) {
        if by_doc.len() < document_count {
            by_doc.resize_with(document_count, Vec::new);
        }
        self.strings_by_doc = by_doc;
    }

    /// Resolves this document's anchor strings to interned string ids. Anchors
    /// that do not occur in the document drop out; an empty result disables the
    /// filter rather than matching nothing.
    pub(crate) fn rebuild(&mut self, doc: DocQuery<'_>, current_doc: usize) {
        let Some(anchor_ids) = self.strings_by_doc.get(current_doc).filter(|ids| !ids.is_empty()) else {
            self.filter = None;
            return;
        };
        let filter: HashSet<u16> = anchor_ids.iter().filter_map(|id| doc.view().lookup_string(id)).collect();
        self.filter = (!filter.is_empty()).then_some(filter);
    }
}
