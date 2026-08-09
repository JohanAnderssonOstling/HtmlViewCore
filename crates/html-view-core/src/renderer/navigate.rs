//! Within-document navigation.
//!
//! Everything here answers "where is the reader, and where should they go" for
//! the document already on screen. None of it can load or swap a document, so
//! none of it needs the pipeline, the resource provider or the document cache.
//! That boundary is what [`NavView`] and [`NavContext`] express.
//!
//! Crossing a document boundary stays on `RendererCore`: it asks these methods
//! for a target, then performs the swap.

use crate::navigation::{Location, NavContext, NavView};
use crate::{DocAnchor, NavigationState, PaintDirection, RendererEvent};

/// Characters per reported reading location. Fixed-size spans keep location
/// numbers stable as font size and column width change.
const LOCATION_CHARS: u64 = 150;

impl NavigationState {
    pub(crate) fn find_doc_index_by_uri_or_suffix(&self, candidate: &str) -> Option<usize> {
        self.document_uris.iter().position(|uri| uri == candidate).or_else(|| self.document_uris.iter().position(|uri| uri.ends_with(candidate)))
    }

    pub(crate) fn fragment_part(fragment: &str) -> Option<String> {
        (!fragment.is_empty()).then(|| fragment.to_string())
    }

    // -- position queries ---------------------------------------------------

    pub(crate) fn find_start_line(&self, cx: NavView<'_>) -> usize {
        cx.doc.start_line_at(cx.viewport.start_offset_y, 0.1)
    }

    /// The glyph at the top of the page on screen, or at the scroll offset when
    /// no page has been laid out yet.
    pub(crate) fn current_glyph_position(&self, cx: NavView<'_>) -> Option<u32> {
        if cx.doc.text().lines().is_empty() {
            return None;
        }
        let line_idx = cx.frame.current_page_start_line.unwrap_or_else(|| self.find_start_line(cx));
        cx.doc.text().line(line_idx).map(|line| line.start())
    }

    /// The nearest preceding anchor id, restricted to table-of-contents anchors
    /// when a filter is active. With a filter, no match means no anchor rather
    /// than falling back to an arbitrary id in the markup.
    pub(crate) fn current_anchor_id(&self, cx: NavView<'_>) -> Option<String> {
        let current_y = cx.viewport.start_offset_y;
        let filter = self.toc_anchors.filter();

        if let Some(current_glyph) = self.current_glyph_position(cx)
            && let Some(id_idx) = cx.doc.best_anchor_from_glyphs(current_glyph, filter)
        {
            return Some(cx.doc.view().string(id_idx).to_string());
        }

        if let Some(id_idx) = cx.doc.best_anchor_from_positions(current_y, filter) {
            return Some(cx.doc.view().string(id_idx).to_string());
        }

        if filter.is_some() {
            return None;
        }

        let glyph_idx = self.nav_anchor_glyph.or_else(|| self.current_glyph_position(cx))?;
        cx.doc.best_anchor_from_glyphs(glyph_idx, None).map(|id_idx| cx.doc.view().string(id_idx).to_string())
    }

    pub(crate) fn current_cfi(&self, cx: NavView<'_>) -> Option<String> {
        let glyph_idx = self.nav_anchor_glyph.or_else(|| self.current_glyph_position(cx))?;
        crate::cfi::generate_cfi(cx.doc.document(), self.current_doc_index, glyph_idx)
    }

    pub(crate) fn current_location(&self, cx: NavView<'_>) -> Location {
        (self.current_doc_index, self.current_anchor_id(cx))
    }

    // -- moving within the document -----------------------------------------

    fn jump_to_forward_offset(&mut self, cx: &mut NavContext<'_>, start_offset_y: f64, pending_nav_anchor_update: bool) {
        cx.viewport.direction = PaintDirection::Forward;
        cx.viewport.reached_end = false;
        cx.viewport.back_anchor_end_line = None;
        cx.viewport.start_offset_y = start_offset_y;
        cx.viewport.end_offset_y = cx.viewport.start_offset_y;
        cx.host.request_repaint();
        self.pending_nav_anchor_update = pending_nav_anchor_update;
    }

    pub(crate) fn restore_glyph_position(&mut self, cx: &mut NavContext<'_>, glyph_idx: u32) {
        let Some(line_idx) = cx.doc.find_line_for_glyph(glyph_idx) else {
            return;
        };
        let Some(line) = cx.doc.text().line(line_idx) else {
            return;
        };
        let y = line.point().y;
        self.jump_to_forward_offset(cx, y, false);
        self.nav_anchor_glyph = Some(glyph_idx);
    }

    /// Anchors on the line covering `target_y`, for anchors recorded by
    /// position rather than by glyph.
    fn restore_anchor_position(&mut self, cx: &mut NavContext<'_>, target_y: f64) -> bool {
        if cx.doc.text().lines().is_empty() {
            return false;
        }
        let line_idx = cx.doc.start_line_at(target_y, 0.1);
        let Some(line) = cx.doc.text().line(line_idx) else {
            return false;
        };
        let line_y = line.point().y;
        let line_start = line.start();
        let line_end = line.end();
        self.jump_to_forward_offset(cx, line_y, false);
        self.nav_anchor_glyph = if line_start < line_end { Some(line_start) } else { None };
        true
    }

    pub(crate) fn restore_position_from_cfi(&mut self, cx: &mut NavContext<'_>, cfi: &str) -> bool {
        let Some((doc_idx, glyph_idx)) = crate::cfi::parse_cfi(cx.doc.document(), cfi) else {
            return false;
        };
        if doc_idx != self.current_doc_index {
            return false;
        }
        self.restore_glyph_position(cx, glyph_idx);
        true
    }

    pub(crate) fn jump_to_id(&mut self, cx: &mut NavContext<'_>, id: &str) -> bool {
        let Some(id_idx) = cx.doc.view().lookup_string(id) else {
            println!("jump_to_id: missing id '{}'", id);
            return false;
        };
        let addressing = cx.doc.view().addressing();
        let Some(glyph_idx) = addressing.anchor_glyph(id_idx) else {
            if let Some(pos) = addressing.anchor_position(id_idx) {
                let y = pos.y();
                return self.restore_anchor_position(cx, y);
            }
            println!("jump_to_id: no glyph for id '{}'", id);
            return false;
        };
        self.restore_glyph_position(cx, glyph_idx);
        self.pending_nav_anchor_update = false;
        true
    }

    pub(crate) fn next_line(&mut self, cx: &mut NavContext<'_>) {
        let lines = cx.doc.text().lines();
        if lines.is_empty() {
            return;
        }
        let current_idx = cx.frame.current_page_start_line.unwrap_or_else(|| self.find_start_line(cx.view()));
        let Some(current_line) = lines.get(current_idx) else {
            return;
        };
        let search_y = current_line.point().y + current_line.height() + 0.1;
        let target_idx = cx.doc.start_line_at(search_y, 0.0);
        if target_idx >= lines.len() {
            return;
        }
        if let Some(target_line) = lines.get(target_idx) {
            let y = target_line.point().y;
            self.jump_to_forward_offset(cx, y, true);
        }
    }

    pub(crate) fn prev_line(&mut self, cx: &mut NavContext<'_>) {
        let lines = cx.doc.text().lines();
        if lines.is_empty() {
            return;
        }
        let current_idx = cx.frame.current_page_start_line.unwrap_or_else(|| self.find_start_line(cx.view()));
        let Some(current_line) = lines.get(current_idx) else {
            return;
        };
        if current_line.point().y <= 0.0 && current_idx == 0 {
            return;
        }
        let search_y = current_line.point().y.max(0.0);
        let Some(target_idx) = cx.doc.end_line_at(search_y, 0.1) else {
            return;
        };
        if let Some(target_line) = lines.get(target_idx) {
            let y = target_line.point().y;
            self.jump_to_forward_offset(cx, y, true);
        }
    }

    /// Settles the viewport after arriving in a document.
    pub(crate) fn finish_document_navigation(&mut self, cx: &mut NavContext<'_>, anchor: DocAnchor) {
        match anchor {
            DocAnchor::Start => {
                cx.viewport.direction = PaintDirection::Forward;
                cx.viewport.start_offset_y = 0.0;
                cx.viewport.end_offset_y = 0.0;
            }
            DocAnchor::End => {
                cx.viewport.direction = PaintDirection::Backward;
                if let Some(last) = cx.doc.text().lines().last() {
                    cx.viewport.end_offset_y = last.point().y + last.height();
                    cx.viewport.back_anchor_end_line = Some(cx.doc.text().line_count().saturating_sub(1));
                }
            }
            DocAnchor::Glyph(glyph_idx) => {
                self.restore_glyph_position(cx, glyph_idx);
            }
        }

        cx.host.request_repaint();
        if matches!(anchor, DocAnchor::Glyph(_)) {
            self.update_nav_signal(cx);
        } else {
            // The anchor is only knowable once the page has been laid out.
            self.pending_nav_anchor_update = true;
        }
    }

    // -- bookkeeping --------------------------------------------------------

    pub(crate) fn update_nav_anchor_from_layout(&mut self, cx: NavView<'_>) {
        self.nav_anchor_glyph = self.current_glyph_position(cx);
    }

    pub(crate) fn update_toc_anchor_filter(&mut self, cx: NavView<'_>) {
        let current_doc = self.current_doc_index;
        self.toc_anchors.rebuild(cx.doc, current_doc);
    }

    pub(crate) fn update_history_availability_signals(&mut self, host: &dyn crate::RendererHost) {
        self.history.emit_availability(host);
    }

    pub(crate) fn push_history_location(&mut self, location: Location, host: &dyn crate::RendererHost) {
        self.history.push(location, host);
    }

    pub(crate) fn push_current_location_if_missing(&mut self, cx: &mut NavContext<'_>) {
        let current = self.current_location(cx.view());
        self.push_history_location(current, cx.host);
    }

    /// Emits every signal derived from the reading position. Each is
    /// deduplicated independently, so an unchanged one stays silent.
    pub(crate) fn update_nav_signal(&mut self, cx: &mut NavContext<'_>) {
        let host = cx.host;
        let view = cx.view();
        let glyph = self.nav_anchor_glyph.or_else(|| self.current_glyph_position(view));
        let current_doc = self.current_doc_index;

        self.signals.position.emit_if_changed((current_doc, glyph), host, |(doc, glyph)| RendererEvent::PositionChanged { doc, glyph });

        let cfi_string = glyph.and_then(|glyph_idx| crate::cfi::generate_cfi(view.doc.document(), current_doc, glyph_idx));
        self.signals.cfi.emit_if_changed(cfi_string, host, RendererEvent::CfiChanged);

        let (fraction, location, total_locations) = self.reading_progress(view, glyph);
        let doc_count = self.document_uris.len();
        self.signals.progress.emit_if_changed((location, total_locations, current_doc), host, |(location, total_locations, doc)| RendererEvent::ReadingProgress {
            fraction,
            location,
            total_locations,
            doc,
            doc_count,
        });

        let anchor = self.current_anchor_id(view);
        self.signals.anchor.emit_if_changed((current_doc, anchor), host, |(doc, anchor)| RendererEvent::NavAnchorChanged { doc, anchor });
    }

    /// Maps the reading position to a publication-wide fraction and location
    /// number. Locations are fixed-size character spans, so they stay stable as
    /// font size and column width change.
    fn reading_progress(&self, cx: NavView<'_>, glyph: Option<u32>) -> (f32, u64, u64) {
        let glyph_count = cx.doc.text().glyph_count().max(1) as f64;
        self.progress_at(glyph.unwrap_or(0) as f64 / glyph_count)
    }

    /// The publication-wide position of `in_document_fraction` through the
    /// current document. Split from [`Self::reading_progress`] so the mapping
    /// can be exercised without a laid-out document.
    pub(crate) fn progress_at(&self, in_document_fraction: f64) -> (f32, u64, u64) {
        let current_doc = self.current_doc_index;
        let total_chars = self.document_text_lengths.iter().sum::<u64>().max(1);
        let preceding_chars = self.document_text_lengths.iter().take(current_doc).sum::<u64>();
        let current_length = self.document_text_lengths.get(current_doc).copied().unwrap_or(1);
        let in_document = (in_document_fraction.clamp(0.0, 1.0) * current_length as f64) as u64;
        let current_chars = (preceding_chars + in_document).min(total_chars);
        let location = (current_chars / LOCATION_CHARS + 1).max(1);
        let total_locations = total_chars.div_ceil(LOCATION_CHARS).max(1);
        (current_chars as f32 / total_chars as f32, location, total_locations)
    }

    /// Inverse of [`Self::reading_progress`]: maps a publication-wide fraction
    /// to a spine index and the fraction within that document.
    pub(crate) fn document_at_fraction(&self, fraction: f32) -> (usize, f64) {
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
        let doc_length = self.document_text_lengths.get(doc).copied().unwrap_or(1).max(1);
        (doc, (target.saturating_sub(preceding) as f64 / doc_length as f64).min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(lengths: Vec<u64>, current_doc: usize) -> NavigationState {
        NavigationState {
            document_uris: lengths.iter().enumerate().map(|(index, _)| format!("doc{index}.html")).collect(),
            document_text_lengths: lengths,
            current_doc_index: current_doc,
            nav_anchor_glyph: None,
            pending_nav_anchor_update: false,
            signals: Default::default(),
            history: Default::default(),
            toc_anchors: Default::default(),
        }
    }

    #[test]
    fn progress_accumulates_the_lengths_of_preceding_documents() {
        let state = nav(vec![300, 300, 300], 1);

        let (start, _, _) = state.progress_at(0.0);
        let (end, _, _) = state.progress_at(1.0);
        assert!((start - 1.0 / 3.0).abs() < 1e-6, "the second of three equal documents starts a third of the way in");
        assert!((end - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn location_numbers_are_one_based_and_span_fixed_character_counts() {
        let state = nav(vec![LOCATION_CHARS * 4], 0);

        assert_eq!(state.progress_at(0.0).1, 1, "locations are numbered from one");
        assert_eq!(state.progress_at(1.0).2, 4, "four locations of LOCATION_CHARS each");
        assert_eq!(state.progress_at(0.5).1, 3, "halfway through is the third location");
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
                let (round_tripped_doc, round_tripped_within) = state.document_at_fraction(fraction);

                // The forward mapping truncates to whole characters, so a
                // position at the very end of a document can land on the start
                // of the next one. Both readings are the same place.
                let same_place = round_tripped_doc == doc || (round_tripped_doc == doc + 1 && within > 0.99 && round_tripped_within < 0.01);
                assert!(same_place, "doc {doc} at {within} round-tripped to doc {round_tripped_doc} at {round_tripped_within}");
                if round_tripped_doc == doc {
                    assert!((round_tripped_within - within).abs() < 0.01, "doc {doc} at {within} round-tripped to {round_tripped_within}");
                }
            }
        }
    }

    #[test]
    fn document_at_fraction_clamps_out_of_range_input() {
        let state = nav(vec![100, 100], 0);
        assert_eq!(state.document_at_fraction(-5.0).0, 0);
        assert_eq!(state.document_at_fraction(5.0).0, 1, "past the end lands in the last document");
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
        assert_eq!(NavigationState::fragment_part("section-1"), Some("section-1".to_owned()));
    }

    #[test]
    fn spine_lookup_prefers_an_exact_uri_over_a_suffix_match() {
        let state = nav(vec![1, 1, 1], 0);
        let mut state = state;
        state.document_uris = vec!["a/chapter.html".to_owned(), "chapter.html".to_owned(), "b/chapter.html".to_owned()];

        assert_eq!(state.find_doc_index_by_uri_or_suffix("chapter.html"), Some(1), "an exact match wins over the earlier suffix match");
        assert_eq!(state.find_doc_index_by_uri_or_suffix("b/chapter.html"), Some(2));
        assert_eq!(state.find_doc_index_by_uri_or_suffix("missing.html"), None);
    }
}
