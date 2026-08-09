//! Navigation state: where the reader is in the publication, how it got there,
//! and what the host has already been told about it.
//!
//! The pieces here are deliberately separate. Change-detection markers, the
//! location history stack and the table-of-contents anchor filter share nothing
//! but the spine index they are keyed by, and each is exercised by a different
//! part of navigation.

use std::collections::HashSet;

use html_view_doc::DocQuery;

mod navigate;
#[cfg(test)]
mod test_support;
use html_view_types::{RendererEvent, RendererHost, ViewportState, VisibleFrame};

/// Everything within-document navigation reads.
///
/// Where the reader is depends on the document, the scroll offset and the page
/// currently laid out -- and on nothing else in the renderer.
#[derive(Clone, Copy)]
pub struct NavView<'a> {
    pub doc: DocQuery<'a>,
    pub viewport: &'a ViewportState,
    pub frame: &'a VisibleFrame,
}

/// [`NavView`] plus the ability to move the viewport and notify the host.
pub struct NavContext<'a> {
    pub doc: DocQuery<'a>,
    pub viewport: &'a mut ViewportState,
    pub frame: &'a VisibleFrame,
    pub host: &'a dyn RendererHost,
}

impl<'a> NavContext<'a> {
    pub fn view(&self) -> NavView<'_> {
        NavView { doc: self.doc, viewport: self.viewport, frame: self.frame }
    }
}

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
    /// to. Vacuously true for an empty stack. Asserted by tests in the renderer
    /// crate, which cannot reach a `#[cfg(test)]` item here.
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
    pub fn rebuild(&mut self, doc: DocQuery<'_>, current_doc: usize) {
        let Some(anchor_ids) = self.strings_by_doc.get(current_doc).filter(|ids| !ids.is_empty()) else {
            self.filter = None;
            return;
        };
        let filter: HashSet<u16> = anchor_ids.iter().filter_map(|id| doc.view().lookup_string(id)).collect();
        self.filter = (!filter.is_empty()).then_some(filter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RecordingHost, availability};

    /// Any event carrying the value is fine; these tests are about the
    /// compare-and-store, not the payload.
    fn event(value: i32) -> RendererEvent {
        RendererEvent::SpeakableText(value.to_string())
    }

    #[test]
    fn debounced_emits_only_when_the_value_changes() {
        let host = RecordingHost::default();
        let mut signal = Debounced::default();

        assert!(signal.emit_if_changed(7, &host, event), "the first value has nothing to match");
        assert!(!signal.emit_if_changed(7, &host, event), "an unchanged value stays silent");
        assert!(signal.emit_if_changed(8, &host, event));
        assert_eq!(host.count(), 2);
    }

    #[test]
    fn debounced_re_emits_an_unchanged_value_after_invalidate() {
        let host = RecordingHost::default();
        let mut signal = Debounced::default();
        signal.emit_if_changed(7, &host, event);
        host.clear();

        signal.emit_if_changed(7, &host, event);
        assert_eq!(host.count(), 0, "still deduplicated");

        signal.invalidate();
        signal.emit_if_changed(7, &host, event);
        assert_eq!(host.count(), 1, "invalidate forces the next value through");
    }

    fn location(doc: usize) -> Location {
        (doc, None)
    }

    #[test]
    fn history_reports_availability_from_the_cursor() {
        let host = RecordingHost::default();
        let mut history = LocationHistory::default();

        history.emit_availability(&host);
        assert_eq!(availability(&host), vec![(false, false)], "an empty stack goes nowhere");

        history.push(location(0), &host);
        history.push(location(1), &host);
        assert_eq!(availability(&host).last().copied(), Some((true, false)), "at the newest entry, only back is available");

        history.step(-1, &host);
        history.emit_availability(&host);
        assert_eq!(availability(&host).last().copied(), Some((false, true)), "at the oldest entry, only forward is available");
    }

    #[test]
    fn history_ignores_a_push_of_the_current_entry() {
        let host = RecordingHost::default();
        let mut history = LocationHistory::default();
        history.push(location(0), &host);
        history.push(location(0), &host);
        history.push(location(0), &host);

        assert!(history.is_at_newest());
        assert!(history.step(-1, &host).is_none(), "repeated pushes of one location must not build a stack to walk back through");
    }

    #[test]
    fn stepping_back_then_pushing_discards_the_forward_entries() {
        let host = RecordingHost::default();
        let mut history = LocationHistory::default();
        history.push(location(0), &host);
        history.push(location(1), &host);
        history.push(location(2), &host);

        assert_eq!(history.step(-1, &host), Some(location(1)));
        history.push(location(9), &host);

        assert!(history.is_at_newest(), "the new branch is the newest entry");
        assert_eq!(history.step(-1, &host), Some(location(1)), "the discarded branch is unreachable");
        assert_eq!(history.step(-1, &host), Some(location(0)));
    }

    #[test]
    fn stepping_past_either_end_leaves_the_cursor_alone() {
        let host = RecordingHost::default();
        let mut history = LocationHistory::default();
        history.push(location(0), &host);
        history.push(location(1), &host);

        assert!(history.step(1, &host).is_none(), "already at the newest entry");
        assert_eq!(history.current(), Some(&location(1)), "a refused step must not move the cursor");

        history.step(-1, &host);
        assert!(history.step(-1, &host).is_none(), "already at the oldest entry");
        assert_eq!(history.current(), Some(&location(0)));
    }

    #[test]
    fn history_availability_is_deduplicated_across_repeated_queries() {
        let host = RecordingHost::default();
        let mut history = LocationHistory::default();
        history.push(location(0), &host);
        host.clear();

        history.emit_availability(&host);
        history.emit_availability(&host);
        history.emit_availability(&host);
        assert!(availability(&host).is_empty(), "an unchanged availability pair must not be re-emitted");
    }

    #[test]
    fn toc_anchor_strings_are_padded_to_the_spine_length() {
        let mut anchors = TocAnchors::default();
        anchors.set_strings_by_doc(vec![vec!["a".to_owned()]], 3);
        assert_eq!(anchors.strings_by_doc.len(), 3, "documents without listed anchors still need a slot to index");
        assert!(anchors.filter().is_none(), "no filter exists until it is rebuilt against a document");
    }

    #[test]
    fn toc_anchor_strings_longer_than_the_spine_are_left_alone() {
        let mut anchors = TocAnchors::default();
        anchors.set_strings_by_doc(vec![vec![], vec![], vec![], vec![]], 2);
        assert_eq!(anchors.strings_by_doc.len(), 4, "padding must not truncate");
    }
}

pub struct NavigationState {
    pub document_uris: Vec<String>,
    pub document_text_lengths: Vec<u64>,
    pub current_doc_index: usize,
    pub nav_anchor_glyph: Option<u32>,
    pub pending_nav_anchor_update: bool,
    pub signals: NavSignals,
    pub history: LocationHistory,
    pub toc_anchors: TocAnchors,
}
