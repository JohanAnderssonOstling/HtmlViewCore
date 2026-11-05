//! Within-document navigation.
//!
//! Everything here answers "where is the reader, and where should they go" for
//! the document already on screen. None of it can load or swap a document, so
//! none of it needs the pipeline, the resource provider or the document cache.
//! That boundary is what [`NavView`] and [`NavContext`] express.
//!
//! Crossing a document boundary stays on `RendererCore`: it asks these methods
//! for a target, then performs the swap.

use html::pagination::{DocumentPosition, PageAnchor, ReflowAnchor, document_position};
use crate::NavigationState;
use crate::{NavContext, NavView, SpineAnchor};
use html::layout::SourceLocation;
use html::targets::{DocumentTarget, ResolvedDestination};
use html_view_cfi::parse_cfi;
use html_view_types::{DocAnchor, ReadingPosition, RendererEffects, RendererEvent, SourcePosition};

/// Characters per reported reading location. Fixed-size spans keep location
/// numbers stable as font size and column width change.
const LOCATION_CHARS: u64 = 150;

impl NavigationState {
    pub fn fragment_part(fragment: &str) -> Option<String> {
        (!fragment.is_empty()).then(|| fragment.to_string())
    }

    // -- position queries ---------------------------------------------------

    fn document_position<'a>(&self, cx: NavView<'a>) -> DocumentPosition<'a> {
        document_position(
            cx.doc,
            PageAnchor {
                cursor: cx.frame.page().start_line().or(cx.viewport.start_line()),
                offset_y: cx.viewport.start_offset_y(),
            },
            self.anchor_glyph_here(),
        )
    }

    pub fn reflow_anchor(&self, cx: NavView<'_>) -> ReflowAnchor {
        let page = cx.frame.page();
        self.document_position(cx).reflow_anchor(
            page.start_line().map(|_| page.content()),
            cx.viewport.request(),
        )
    }

    /// The glyph at the top of the page on screen, or at the scroll offset when
    /// no page has been laid out yet.
    pub fn current_glyph_position(&self, cx: NavView<'_>) -> Option<u32> {
        self.document_position(cx).glyph
    }

    pub fn current_cfi(&self, cx: NavView<'_>) -> Option<String> {
        self.cfi
            .source_position_cfi(&self.current_source_position(cx)?)
    }

    pub fn current_source_position(&self, cx: NavView<'_>) -> Option<SourcePosition> {
        let source = self.document_position(cx).source()?;
        Some(SourcePosition {
            document: self.current_doc_index,
            path: source.path,
            utf16_offset: source.utf16_offset,
        })
    }

    /// Replay the last settled position to a newly attached host. Pending
    /// layout changes are published only after composition settles again.
    pub fn emit_reading_position_snapshot(&self, effects: &mut RendererEffects) {
        if let Some(position) = &self.reading_position.last {
            effects.emit(RendererEvent::ReadingPositionChanged(position.clone()));
        }
    }

    /// Preserve the displayed origin until the requested jump has paginated.
    pub fn begin_jump(&mut self, from: String) {
        self.pending_jump_origin.get_or_insert(from);
    }

    pub fn cancel_jump(&mut self) {
        self.pending_jump_origin = None;
    }

    /// Publish only after the viewport's page has settled. All host projections
    /// consume this value together.
    pub fn emit_reading_position(&mut self, cx: &mut NavContext<'_>) {
        let local = self.document_position(cx.view());
        let Some(source) = local.source() else {
            return;
        };
        let source = SourcePosition {
            document: self.current_doc_index,
            path: source.path,
            utf16_offset: source.utf16_offset,
        };
        let document_fraction = local.document_fraction;
        let (fraction, location, total_locations) = self.progress_at(document_fraction);
        let position = ReadingPosition {
            source,
            fraction,
            document_fraction: document_fraction as f32,
            location,
            total_locations,
            document_count: self.document_uris.len(),
        };
        if let Some(from) = self.pending_jump_origin.take()
            && Some(&from) != self.cfi.source_position_cfi(&position.source).as_ref()
        {
            cx.effects.emit(RendererEvent::NavigationJump { from });
            self.reading_position.invalidate();
        }
        self.reading_position.emit_if_changed(
            position,
            cx.effects,
            RendererEvent::ReadingPositionChanged,
        );
    }

    // -- moving within the document -----------------------------------------

    fn begin_forward_at(&mut self, cx: &mut NavContext<'_>, start_offset_y: f64) {
        cx.viewport.begin_forward_at(start_offset_y);
        cx.effects.request_repaint();
    }

    pub fn restore_forward_offset(&mut self, cx: &mut NavContext<'_>, start_offset_y: f64) {
        self.begin_forward_at(cx, start_offset_y);
        self.pending_nav_anchor_update = false;
    }

    pub fn advance_to_forward_offset(&mut self, cx: &mut NavContext<'_>, start_offset_y: f64) {
        self.begin_forward_at(cx, start_offset_y);
        self.pending_nav_anchor_update = true;
    }

    /// Advances to a known pagination group without reducing its identity to y.
    pub fn advance_to_forward_line(&mut self, cx: &mut NavContext<'_>, cursor: usize) {
        let Some(ResolvedDestination::Line { cursor, point, .. }) = cx
            .doc
            .addressing()
            .destination(DocumentTarget::Cursor(cursor))
        else {
            return;
        };
        cx.viewport.begin_forward_at_anchor(PageAnchor {
            cursor: Some(cursor),
            offset_y: point.y,
        });
        cx.effects.request_repaint();
        self.pending_nav_anchor_update = true;
    }

    pub fn restore_glyph_position(&mut self, cx: &mut NavContext<'_>, glyph_idx: u32) {
        let Some(ResolvedDestination::Line { cursor, .. }) = cx
            .doc
            .addressing()
            .destination(DocumentTarget::Glyph(glyph_idx))
        else {
            return;
        };
        self.advance_to_forward_line(cx, cursor);
        self.pending_nav_anchor_update = false;
        self.anchor_at(Some(glyph_idx));
    }

    /// Anchors on the line covering `target_y`, for anchors recorded by
    /// position rather than by glyph.
    fn restore_anchor_position(&mut self, cx: &mut NavContext<'_>, target_y: f64) -> bool {
        let Some(ResolvedDestination::Line { point, glyph, .. }) = cx
            .doc
            .addressing()
            .destination(DocumentTarget::Offset(target_y))
        else {
            return false;
        };
        self.restore_forward_offset(cx, point.y);
        self.anchor_at(glyph);
        true
    }

    pub fn restore_position_from_cfi(&mut self, cx: &mut NavContext<'_>, cfi: &str) -> bool {
        let Some(internal) = self.cfi.to_internal(cfi) else {
            return false;
        };
        if !internal.contains('!') && self.cfi.document_index(cfi) == Some(self.current_doc_index) {
            self.restore_forward_offset(cx, 0.0);
            self.anchor_at(None);
            return true;
        }
        if let Some(position) = html_view_cfi::parse_source_position(&internal)
            && position.document == self.current_doc_index
        {
            let source = SourceLocation {
                path: position.path,
                utf16_offset: position.utf16_offset,
            };
            if let Some(destination) = cx
                .doc
                .addressing()
                .destination(DocumentTarget::Source(&source))
            {
                match destination {
                    ResolvedDestination::Line { cursor, .. } => {
                        self.advance_to_forward_line(cx, cursor)
                    }
                    ResolvedDestination::Position { y } => self.restore_forward_offset(cx, y),
                }
                self.pending_nav_anchor_update = false;
                self.anchor_at(None);
                return true;
            }
        }
        let Some((doc_idx, glyph_idx)) = parse_cfi(cx.doc, &internal) else {
            return false;
        };
        if doc_idx != self.current_doc_index {
            return false;
        }
        self.restore_glyph_position(cx, glyph_idx);
        true
    }

    pub fn jump_to_id(&mut self, cx: &mut NavContext<'_>, id: &str) -> bool {
        let Some(destination) = cx.doc.addressing().destination(DocumentTarget::Element(id)) else {
            return false;
        };
        match destination {
            ResolvedDestination::Line { cursor, glyph, .. } => {
                self.advance_to_forward_line(cx, cursor);
                self.anchor_at(glyph);
            }
            ResolvedDestination::Position { y } => return self.restore_anchor_position(cx, y),
        }
        self.pending_nav_anchor_update = false;
        true
    }

    /// Scrolls one line further into this document, reporting whether there
    /// was one. A caller that reads more than one document at a time takes
    /// over when there was not.
    pub fn next_line(&mut self, cx: &mut NavContext<'_>) -> bool {
        let cursor = self.document_position(cx.view()).cursor;
        let Some(ResolvedDestination::Line { cursor, point, .. }) = cx
            .doc
            .addressing()
            .destination(DocumentTarget::NextLine(cursor))
        else {
            return false;
        };
        cx.viewport.begin_forward_at_anchor(PageAnchor {
            cursor: Some(cursor),
            offset_y: point.y,
        });
        cx.effects.request_repaint();
        self.pending_nav_anchor_update = true;
        true
    }

    /// Scrolls one line toward the beginning of this document, reporting
    /// whether there was one.
    pub fn prev_line(&mut self, cx: &mut NavContext<'_>) -> bool {
        let cursor = self.document_position(cx.view()).cursor;
        let Some(ResolvedDestination::Line { cursor, point, .. }) = cx
            .doc
            .addressing()
            .destination(DocumentTarget::PreviousLine(cursor))
        else {
            return false;
        };
        cx.viewport.begin_forward_at_anchor(PageAnchor {
            cursor: Some(cursor),
            offset_y: point.y,
        });
        cx.effects.request_repaint();
        self.pending_nav_anchor_update = true;
        true
    }

    /// Settles the viewport after arriving in a document.
    pub fn finish_document_navigation(&mut self, cx: &mut NavContext<'_>, anchor: DocAnchor) {
        match anchor {
            DocAnchor::Start => {
                cx.viewport.begin_forward_at(0.0);
            }
            DocAnchor::End => {
                if let Some(destination) = cx.doc.addressing().destination(DocumentTarget::End) {
                    let (offset, cursor) = match destination {
                        ResolvedDestination::Line { cursor, point, .. } => (point.y, Some(cursor)),
                        ResolvedDestination::Position { y } => (y, None),
                    };
                    cx.viewport.begin_backward_at(offset, cursor);
                }
            }
            DocAnchor::Glyph(glyph_idx) => {
                self.restore_glyph_position(cx, glyph_idx);
            }
        }

        cx.effects.request_repaint();
        if !matches!(anchor, DocAnchor::Glyph(_)) {
            // The anchor is only knowable once the page has been laid out.
            self.pending_nav_anchor_update = true;
        }
    }

    // -- bookkeeping --------------------------------------------------------

    pub fn update_nav_anchor_from_layout(&mut self, cx: NavView<'_>) {
        let glyph = self.current_glyph_position(cx);
        self.anchor_at(glyph);
    }

    /// The anchor, if it belongs to the document being read. One from a
    /// document the reader has left indexes glyphs that are not on screen.
    pub fn anchor_glyph_here(&self) -> Option<u32> {
        self.nav_anchor
            .filter(|anchor| anchor.doc == self.current_doc_index)
            .map(|anchor| anchor.glyph)
    }

    /// Records where the reader is in the document being read.
    pub fn anchor_at(&mut self, glyph: Option<u32>) {
        self.nav_anchor = glyph.map(|glyph| SpineAnchor {
            doc: self.current_doc_index,
            glyph,
        });
    }

    /// The publication-wide position of `in_document_fraction` through the
    /// current document, using the publication's retained text lengths.
    pub fn progress_at(&self, in_document_fraction: f64) -> (f32, u64, u64) {
        let current_doc = self.current_doc_index;
        let total_chars = self.document_text_lengths.iter().sum::<u64>().max(1);
        let preceding_chars = self
            .document_text_lengths
            .iter()
            .take(current_doc)
            .sum::<u64>();
        let current_length = self
            .document_text_lengths
            .get(current_doc)
            .copied()
            .unwrap_or(1);
        let in_document = (in_document_fraction.clamp(0.0, 1.0) * current_length as f64) as u64;
        let current_chars = (preceding_chars + in_document).min(total_chars);
        let location = (current_chars / LOCATION_CHARS + 1).max(1);
        let total_locations = total_chars.div_ceil(LOCATION_CHARS).max(1);
        (
            current_chars as f32 / total_chars as f32,
            location,
            total_locations,
        )
    }

    /// Inverse of [`Self::reading_progress`]: maps a publication-wide fraction
    /// to a spine index and the fraction within that document.
    pub fn document_at_fraction(&self, fraction: f32) -> (usize, f64) {
        let total = self.document_text_lengths.iter().sum::<u64>().max(1);
        let target = (fraction.clamp(0.0, 1.0) as f64 * total as f64) as u64;
        // The last document owns the end of the publication: a target equal to
        // the total character count belongs at its end, not past it.
        let mut running = 0_u64;
        let doc = self
            .document_text_lengths
            .iter()
            .copied()
            .position(|length| {
                running = running.saturating_add(length);
                target < running
            })
            .unwrap_or_else(|| self.document_text_lengths.len().saturating_sub(1));
        let preceding: u64 = self.document_text_lengths.iter().take(doc).sum();
        let doc_length = self
            .document_text_lengths
            .get(doc)
            .copied()
            .unwrap_or(1)
            .max(1);
        (
            doc,
            (target.saturating_sub(preceding) as f64 / doc_length as f64).min(1.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(lengths: Vec<u64>, current_doc: usize) -> NavigationState {
        NavigationState {
            cfi: Default::default(),
            document_uris: lengths
                .iter()
                .enumerate()
                .map(|(index, _)| format!("doc{index}.html"))
                .collect(),
            document_text_lengths: lengths,
            current_doc_index: current_doc,
            nav_anchor: None,
            pending_nav_anchor_update: false,
            reading_position: Default::default(),
            pending_jump_origin: None,
        }
    }

    #[test]
    fn progress_accumulates_the_lengths_of_preceding_documents() {
        let state = nav(vec![300, 300, 300], 1);

        let (start, _, _) = state.progress_at(0.0);
        let (end, _, _) = state.progress_at(1.0);
        assert!(
            (start - 1.0 / 3.0).abs() < 1e-6,
            "the second of three equal documents starts a third of the way in"
        );
        assert!((end - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn location_numbers_are_one_based_and_span_fixed_character_counts() {
        let state = nav(vec![LOCATION_CHARS * 4], 0);

        assert_eq!(
            state.progress_at(0.0).1,
            1,
            "locations are numbered from one"
        );
        assert_eq!(
            state.progress_at(1.0).2,
            4,
            "four locations of LOCATION_CHARS each"
        );
        assert_eq!(
            state.progress_at(0.5).1,
            3,
            "halfway through is the third location"
        );
    }

    #[test]
    fn an_empty_publication_reports_one_location_rather_than_dividing_by_zero() {
        let state = nav(vec![], 0);
        let (fraction, location, total) = state.progress_at(0.0);
        assert_eq!((location, total), (1, 1));
        assert!(fraction.is_finite());
    }

    #[test]
    fn document_at_fraction_inverts_progress_at() {
        let lengths = vec![500, 1500, 250, 3000];
        for (doc, _) in lengths.iter().enumerate() {
            let state = nav(lengths.clone(), doc);
            for step in 0..=10 {
                let within = step as f64 / 10.0;
                let (fraction, _, _) = state.progress_at(within);
                let (round_tripped_doc, round_tripped_within) =
                    state.document_at_fraction(fraction);

                // The forward mapping truncates to whole characters, so a
                // position at the very end of a document can land on the start
                // of the next one. Both readings are the same place.
                let same_place = round_tripped_doc == doc
                    || (round_tripped_doc == doc + 1
                        && within > 0.99
                        && round_tripped_within < 0.01);
                assert!(
                    same_place,
                    "doc {doc} at {within} round-tripped to doc {round_tripped_doc} at {round_tripped_within}"
                );
                if round_tripped_doc == doc {
                    assert!(
                        (round_tripped_within - within).abs() < 0.01,
                        "doc {doc} at {within} round-tripped to {round_tripped_within}"
                    );
                }
            }
        }
    }

    #[test]
    fn document_at_fraction_clamps_out_of_range_input() {
        let state = nav(vec![100, 100], 0);
        assert_eq!(state.document_at_fraction(-5.0).0, 0);
        assert_eq!(
            state.document_at_fraction(5.0).0,
            1,
            "past the end lands in the last document"
        );
    }

    #[test]
    fn document_at_fraction_survives_an_empty_publication() {
        let state = nav(vec![], 0);
        let (doc, within) = state.document_at_fraction(0.5);
        assert_eq!(doc, 0);
        assert!(within.is_finite());
    }

    #[test]
    fn fragment_part_treats_an_empty_fragment_as_absent() {
        assert_eq!(NavigationState::fragment_part(""), None);
        assert_eq!(
            NavigationState::fragment_part("section-1"),
            Some("section-1".to_owned())
        );
    }
}
