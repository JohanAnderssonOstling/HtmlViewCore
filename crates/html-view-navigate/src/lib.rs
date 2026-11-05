//! Navigation state: where the reader is in the publication, how it got there,
//! and what the host has already been told about it.
//!
//! Settled source positions are reported as one snapshot. The host owns
//! navigation history and maps source positions to its table of contents.

use html::layout::RenderView;

mod navigate;
use html_view_types::{ReadingPosition, RendererEffects, RendererEvent, ViewportState, VisibleFrame};

/// Everything within-document navigation reads.
///
/// Where the reader is depends on the document, the scroll offset and the page
/// currently laid out -- and on nothing else in the renderer.
#[derive(Clone, Copy)]
pub struct NavView<'a> {
    pub doc: RenderView<'a>,
    pub viewport: &'a ViewportState,
    pub frame: &'a VisibleFrame,
}

/// [`NavView`] plus the ability to move the viewport and notify the host.
pub struct NavContext<'a> {
    pub doc: RenderView<'a>,
    pub viewport: &'a mut ViewportState,
    pub frame: &'a VisibleFrame,
    pub effects: &'a mut RendererEffects,
}

impl<'a> NavContext<'a> {
    pub fn view(&self) -> NavView<'_> {
        NavView {
            doc: self.doc,
            viewport: self.viewport,
            frame: self.frame,
        }
    }
}

/// A value the host has already been told about.
///
/// Every outbound navigation signal is deduplicated: the host is notified only
/// when the computed value differs from the last one sent. Owning the compare,
/// the store and the emit together means a caller cannot emit without updating
/// the marker, which is the failure mode this type exists to prevent.
struct Debounced<T> {
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
    fn emit_if_changed(
        &mut self,
        value: T,
        effects: &mut RendererEffects,
        event: impl FnOnce(T) -> RendererEvent,
    ) -> bool {
        if self.last.as_ref() == Some(&value) {
            return false;
        }
        self.last = Some(value.clone());
        effects.emit(event(value));
        true
    }

    /// Forgets what was last sent, so the next value is emitted even if it is
    /// unchanged.
    fn invalidate(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any event carrying the value is fine; these tests are about the
    /// compare-and-store, not the payload.
    fn event(value: i32) -> RendererEvent {
        RendererEvent::SpeakableText(value.to_string())
    }

    #[test]
    fn debounced_emits_only_when_the_value_changes() {
        let mut effects = RendererEffects::default();
        let mut signal = Debounced::default();

        assert!(
            signal.emit_if_changed(7, &mut effects, event),
            "the first value has nothing to match"
        );
        assert!(
            !signal.emit_if_changed(7, &mut effects, event),
            "an unchanged value stays silent"
        );
        assert!(signal.emit_if_changed(8, &mut effects, event));
        assert_eq!(effects.events.len(), 2);
    }

    #[test]
    fn debounced_re_emits_an_unchanged_value_after_invalidate() {
        let mut effects = RendererEffects::default();
        let mut signal = Debounced::default();
        signal.emit_if_changed(7, &mut effects, event);
        effects.events.clear();

        signal.emit_if_changed(7, &mut effects, event);
        assert_eq!(effects.events.len(), 0, "still deduplicated");

        signal.invalidate();
        signal.emit_if_changed(7, &mut effects, event);
        assert_eq!(
            effects.events.len(),
            1,
            "invalidate forces the next value through"
        );
    }
}

/// A place in the book: a glyph, and the spine item it belongs to.
///
/// This is the durable form. It survives a relayout, because a glyph keeps
/// its identity when the columns change shape and an offset does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpineAnchor {
    pub doc: usize,
    pub glyph: u32,
}

pub struct NavigationState {
    pub cfi: html_view_cfi::PublicationCfi,
    document_uris: Vec<String>,
    document_text_lengths: Vec<u64>,
    current_doc_index: usize,
    /// Where the reader is, durably: a glyph, and the document it indexes.
    ///
    /// A glyph index means nothing without its document -- the same number
    /// names a different place in the next spine item. Carrying the document
    /// with it is what makes an anchor from a document the reader has left
    /// simply not apply, rather than apply somewhere wrong.
    nav_anchor: Option<SpineAnchor>,
    pending_nav_anchor_update: bool,
    reading_position: Debounced<ReadingPosition>,
    pending_jump_origin: Option<String>,
}

impl NavigationState {
    pub fn new(document_uris: Vec<String>, current_document: usize) -> Self {
        let document_text_lengths = vec![1; document_uris.len()];
        Self {
            cfi: Default::default(),
            document_uris,
            document_text_lengths,
            current_doc_index: current_document,
            nav_anchor: None,
            pending_nav_anchor_update: false,
            reading_position: Debounced::default(),
            pending_jump_origin: None,
        }
    }

    pub fn document_uris(&self) -> &[String] {
        &self.document_uris
    }

    pub fn current_document(&self) -> usize {
        self.current_doc_index
    }

    pub fn anchor_update_pending(&self) -> bool {
        self.pending_nav_anchor_update
    }

    pub fn activate_document(&mut self, document: usize) {
        self.current_doc_index = document;
        self.reading_position.invalidate();
    }

    pub fn mark_anchor_dirty(&mut self) {
        self.pending_nav_anchor_update = true;
    }

    pub fn mark_anchor_clean(&mut self) {
        self.pending_nav_anchor_update = false;
    }

    pub fn install_document_text_lengths(&mut self, lengths: Vec<u64>) -> bool {
        if lengths.len() != self.document_uris.len() || lengths.contains(&0) {
            return false;
        }
        self.document_text_lengths = lengths;
        self.reading_position.invalidate();
        true
    }
}
