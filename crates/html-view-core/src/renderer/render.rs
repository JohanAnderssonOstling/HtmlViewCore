use std::time::Duration;

use html_view_doc::DocQuery;
use html_view_paginate::Paginator;
use html_view_paint::PagePainter;
use crate::{GlyphShaper, PagePositions, PaintDirection, Painter, RenderState, RendererCore};

impl RendererCore {
    pub(crate) fn painter(&self) -> PagePainter<'_> {
        PagePainter {
            geo: self.text_geometry(),
            images: &self.page.images,
            reader_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            highlight: &self.highlight,
            annotations: &self.annotations,
            media_overlay: &self.media_overlay,
            selection: self.selection_view(),
        }
    }

    /// Paints a note handed out with a [`FootnotePreview`], whole and at its
    /// own layout positions, for a host showing it in a popup.
    ///
    /// The note carries no selection, search or annotation state, so those
    /// overlays are empty.
    ///
    /// Images resolve through the page's pipeline, which is this document's.
    /// Image resources belong to the DOM and `image_idx` is an index into the
    /// document's list, so a scoped note -- sharing the document -- shares
    /// that index space. A note's images are the same resources as the page's,
    /// already loaded and decoded.
    ///
    /// [`FootnotePreview`]: crate::FootnotePreview
    /// A note as a paintable scene, sized and clipped to the width it was laid
    /// out to.
    ///
    /// The engine's fragment renderer owns this: natural height over lines,
    /// decorations and images alike, content in paint order, and per-line
    /// overflow clipping. A note is an ordinary laid-out document, so it is
    /// drawn by the same machinery rather than by a second painter here.
    pub(crate) fn note_scene(&self) -> Option<html::render::RenderScene> {
        let note = &self.note.as_ref()?.document;
        let width = self.note_layout_width();
        let options = html::render::FragmentRenderOptions::new(width, None).ok()?.with_typography(self.root_font_size.max(1.0) as u32, self.root_font_size as f64).ok()?;
        Some(html::render::RenderScene::for_document(note, &options))
    }

    pub(crate) fn note_view(&self) -> Option<&crate::document_view::DocumentView> {
        self.note.as_ref()
    }

    pub(crate) fn note_view_mut(&mut self) -> Option<&mut crate::document_view::DocumentView> {
        self.note.as_mut()
    }

    pub(crate) fn close_note(&mut self) {
        self.note = None;
    }

    /// Paints the open note's selection highlight over its content. The note
    /// is framed whole, so the overlay walks every line it has.
    pub(crate) fn paint_note_selection(&self, cx: &mut impl Painter) {
        let Some(note) = self.note.as_ref() else { return };
        PagePainter {
            geo: note.geometry(),
            images: &self.page.images,
            reader_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            highlight: &Default::default(),
            annotations: &Default::default(),
            media_overlay: &Default::default(),
            selection: note.selection_view(),
        }
        .paint_interaction_overlay(cx);
    }

    /// Width a note is laid out and shown at. The host decides; the reading
    /// column is the fallback when it has no opinion.
    pub(crate) fn note_layout_width(&self) -> f64 {
        self.host.note_popup_width().filter(|width| width.is_finite() && *width > 0.0).unwrap_or(self.page.view.layout.col_width)
    }

    pub(crate) fn paint_frame(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.current_doc_index);
        self.painter().paint_frame(cx);
        self.paint_following(cx);
    }

    pub(crate) fn paint_base_before_overlay(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.current_doc_index);
        self.painter().paint_base_before_overlay(cx);
    }

    pub(crate) fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.current_doc_index);
        self.painter().paint_interaction_overlay(cx);
    }

    pub(crate) fn paint_base_after_overlay(&self, cx: &mut impl Painter) -> RenderState {
        self.host.set_glyph_document(self.nav.current_doc_index);
        let state = self.painter().paint_base_after_overlay(cx);
        // The following items belong to the base layer. They occupy columns
        // the page does not, so painting them after its overlay hides nothing.
        self.paint_following(cx);
        state
    }

    #[cfg(test)]
    pub(crate) fn paint_forward(&mut self, cx: &mut impl Painter) -> RenderState {
        self.prepare_forward();
        self.host.set_glyph_document(self.nav.current_doc_index);
        self.painter().paint_frame(cx);
        self.paint_following(cx);
        html_view_paginate::initial_render_state()
    }

    /// Paints the spine items sharing the screen with the page. They are drawn
    /// after it and without the canvas fill, which the page has already laid
    /// down for the whole viewport.
    pub(crate) fn paint_following(&self, cx: &mut impl Painter) {
        for item in &self.following {
            self.paint_following_item(item, cx);
        }
        // Whatever comes next belongs to the page again.
        self.host.set_glyph_document(self.nav.current_doc_index);
    }

    fn paint_following_item(&self, item: &crate::document_view::FollowingItem, cx: &mut impl Painter) {
        self.host.set_glyph_document(item.doc_index);
        let view = &item.spine.view;
        PagePainter {
            // An image is an index into the document that refers to it, so a
            // following item's pictures come from its own pipeline.
            geo: view.geometry(),
            images: &item.spine.images,
            reader_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            highlight: &Default::default(),
            annotations: &Default::default(),
            media_overlay: &Default::default(),
            selection: view.selection_view(),
        }
        .paint_remaining_base(cx);
    }

    pub(crate) fn paginator(&self) -> Paginator<'_> {
        Paginator::new(DocQuery::new(&self.page.view.document), &self.page.view.layout, self.page.inputs.layout.text_composition_policy.is_book_optimized(), self.vertical_rhythm)
    }

    fn install_positioned_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>, use_cached_inline_positions: bool) {
        let previous_positions = std::mem::replace(&mut self.page.view.frame.last_line_positions, page.positions);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
        self.page.view.frame.current_page_start_line = Some(page.start_line);
        self.page.view.frame.current_page_end_line = paint_end;
        self.page.view.frame.paint_start_offset_y = start_offset_y;
        self.page.view.frame.use_cached_inline_positions = use_cached_inline_positions;
        self.page.view.frame.offset_breaks = page.offset_breaks;
        self.page.view.frame.repeated_table_headers = page.repeated_table_headers;
    }

    pub(crate) fn prepare_forward(&mut self) {
        // render forward from current offset
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.view.frame.last_line_positions);
        let paginator = self.paginator();
        let events = paginator.events();
        let page = paginator.page_at(&events, self.viewport.start_offset_y, positions);
        self.viewport.next_start_offset_y = page.next_start_offset_y;
        self.viewport.reached_end = page.reached_end;
        let paint_end = page.last_line;
        self.install_positioned_page(page, self.viewport.start_offset_y, paint_end, false);
    }

    /// Fills the columns the page leaves empty with the spine items that
    /// follow it.
    ///
    /// A spine item always begins at the top of a column, so a document
    /// boundary is a column boundary and each item can be paginated on its own
    /// and then shifted into the columns still free. Nothing has to flow
    /// across a boundary.
    ///
    /// Each item is paginated into the columns it will actually get, not into
    /// a full screen's worth, so what it reports about running out -- where it
    /// resumes, and whether there is anything left of it -- describes the
    /// screen it is on. Continuous scrolling reads that back.
    pub(crate) fn fill_following_columns(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if !self.continuous_spine {
            return;
        }
        let screen = (self.frame_geometry_cache_key(), self.nav.current_doc_index);
        if self.following_key == Some(screen) {
            return;
        }
        self.following_key = Some(screen);
        self.following.clear();

        let columns = self.page.view.layout.col_count as i32;
        let mut used = self.columns_used(&self.page.view);
        let mut doc_index = self.nav.current_doc_index;

        while used < columns && doc_index + 1 < self.nav.document_uris.len() {
            doc_index += 1;
            let loaded = self.load_document_for_index(glyph_shaper, doc_index);
            // The load above shaped this item's glyphs under its own id space,
            // and the item keeps the pipeline and images addressed by it.
            let mut spine = crate::spine_item::SpineItem::new(loaded, self.page.view.layout.clone(), self.provider.clone());
            let view = &mut spine.view;

            // Paginated against the free columns alone, then shifted into
            // them: the item is laid out for the room it has, and the columns
            // it would have spilled into are the next screen's business.
            let mut free_columns = view.layout.clone();
            free_columns.col_count = f64::from(columns - used);
            let paginator = Paginator::new(DocQuery::new(&view.document), &free_columns, self.page.inputs.layout.text_composition_policy.is_book_optimized(), self.vertical_rhythm);
            let events = paginator.events();
            let mut page = paginator.page_at(&events, 0.0, Vec::new().into());
            page.positions.shift_columns(used, &view.layout);
            for header in &mut page.repeated_table_headers {
                header.col_index += used;
            }

            view.frame.current_page_start_line = Some(page.start_line);
            view.frame.current_page_end_line = page.last_line;
            // Backgrounds and borders are placed from the paint offset rather
            // than from recorded line positions, and this item's document
            // starts at the top of column `used`. Saying so puts them in the
            // same columns as the text they belong to.
            view.frame.paint_start_offset_y = -f64::from(used) * view.layout.size.height;
            view.frame.use_cached_inline_positions = true;
            view.frame.offset_breaks = page.offset_breaks;
            view.frame.repeated_table_headers = page.repeated_table_headers;
            view.frame.last_line_positions = page.positions;

            used = self.columns_used(&spine.view).max(used + 1);
            self.following.push(crate::document_view::FollowingItem { spine, doc_index, next_start_offset_y: page.next_start_offset_y, reached_end: page.reached_end });
        }
    }

    /// Where reading continues past everything the screen is showing, for a
    /// screen showing more than the page.
    ///
    /// Those items are already on screen, so starting one of them over would
    /// show the reader what they just read. Reading resumes inside the last
    /// one, below the part that fit -- or after it, when all of it fit.
    ///
    /// `None` when the page is the whole screen, which is every screen
    /// outside continuous mode, and when the spine has nothing left.
    pub(crate) fn spine_continuation(&self) -> Option<(usize, crate::DocAnchor)> {
        let last = self.following.last()?;
        if !last.reached_end {
            return Some((last.doc_index, crate::DocAnchor::Offset(last.next_start_offset_y)));
        }
        let after = last.doc_index + 1;
        (after < self.nav.document_uris.len()).then_some((after, crate::DocAnchor::Start))
    }

    #[cfg(test)]
    pub(crate) fn following_views(&self) -> &[crate::document_view::FollowingItem] {
        &self.following
    }

    /// Columns a view's painted lines occupy, which is where the next spine
    /// item may start.
    fn columns_used(&self, view: &crate::document_view::DocumentView) -> i32 {
        let (Some(start), Some(end)) = (view.frame.current_page_start_line, view.frame.current_page_end_line) else { return 0 };
        (start..=end).filter_map(|line| view.frame.last_line_positions.get(line)).map(|screen| screen.col_index + 1).max().unwrap_or(0)
    }

    /// Decodes what the items beside the page are showing.
    ///
    /// An image is an index into the document that refers to it, so each item
    /// fetches through its own pipeline. Unlike the page there is no next page
    /// to read ahead for: an item is re-paginated whenever the screen changes.
    pub(crate) fn prefetch_images_for_following(&mut self) {
        let mut pending = false;
        for item in &mut self.following {
            let (Some(start), Some(end)) = (item.spine.view.frame.current_page_start_line, item.spine.view.frame.current_page_end_line) else { continue };
            let desired = {
                let root = item.spine.view.document.render_view();
                let fragments = root.fragments();
                if fragments.images().is_empty() {
                    continue;
                }
                let mut desired = std::collections::HashSet::new();
                for line_idx in start..=end {
                    for frag in fragments.images_for_line(line_idx).iter() {
                        desired.insert(frag.image_idx());
                    }
                }
                desired
            };
            item.spine.images.ensure_window(&desired);
            pending |= item.spine.images.has_pending();
        }
        if pending {
            self.host.schedule_repaint(Duration::from_millis(16));
        }
    }

    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let root = self.page.view.document.render_view();
        let text = root.text();
        let fragments = root.fragments();
        if fragments.images().is_empty() || text.line_count() == 0 {
            return;
        }
        let Some(start_line) = self.page.view.frame.current_page_start_line else {
            return;
        };
        let Some(end_line) = self.page.view.frame.current_page_end_line else {
            return;
        };
        if start_line >= text.line_count() || end_line >= text.line_count() {
            return;
        }

        let page_len = end_line.saturating_sub(start_line) + 1;
        let next_start = end_line + 1;
        let next_end = if next_start >= text.line_count() { end_line } else { (next_start + page_len - 1).min(text.line_count() - 1) };

        let mut desired = std::mem::take(&mut self.frame_scratch.desired_images);
        desired.clear();
        for line_idx in start_line..=next_end {
            for frag in fragments.images_for_line(line_idx).iter() {
                desired.insert(frag.image_idx());
            }
        }

        self.page.images.ensure_window(&desired);
        self.frame_scratch.desired_images = desired;
        if self.page.images.has_pending() {
            self.host.schedule_repaint(Duration::from_millis(16));
        }
    }

    pub(crate) fn prepare_backward(&mut self) {
        // render backward from end offset
        self.viewport.reached_end = false;

        let lines = self.page.view.document.render_view().text().lines();
        let end_line = if let Some(idx) = self.viewport.back_anchor_end_line {
            idx.min(lines.len().saturating_sub(1))
        } else if let Some(idx) = self.doc().end_line_at(self.viewport.end_offset_y, 1e-6) {
            idx
        } else {
            self.viewport.start_offset_y = 0.0;
            self.viewport.direction = PaintDirection::Forward;
            self.prepare_forward();
            return;
        };
        if end_line == 0 {
            self.viewport.start_offset_y = 0.0;
            self.viewport.direction = PaintDirection::Forward;
            self.viewport.back_anchor_end_line = None;
            self.prepare_forward();
            return;
        }

        let available_height = self.page.view.layout.col_count * self.page.view.layout.size.height;
        let end_line_data = lines.get(end_line).expect("end line must be within line count");
        let end_line_height = self.doc().effective_line_height(end_line, end_line_data.height());
        // Keep a tiny safety margin so the anchor line doesn't land exactly on the
        // bottom edge and get treated as overflow by column fit logic.
        let end_line_target_y = (available_height - end_line_height - 0.1).max(0.0);
        self.viewport.back_start_offset_y = (end_line_data.point().y - end_line_target_y).max(0.0);

        // Use the same forward position resolver as normal rendering so lines that share
        // the same document Y (e.g. table rows across columns) keep consistent screen Y.
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.view.frame.last_line_positions);
        // Both scratch buffers are claimed before the paginator borrows the
        // document and column layout, so the search below needs no further
        // mutable access while it runs.
        let mut candidate_positions = self.frame_scratch.take_line_position_buffer(&mut self.page.view.frame.last_line_positions);
        let back_start_offset_y = self.viewport.back_start_offset_y;
        let paginator = self.paginator();
        let events = paginator.events();
        let mut page = paginator.page_at(&events, back_start_offset_y, positions);

        // If anchor line is not visible, find the smallest start offset that includes it.
        // This keeps the anchor as low as possible (near page bottom) instead of pinning top.
        if !page.positions.contains(end_line) {
            let mut lo = back_start_offset_y;
            let mut hi = end_line_data.point().y.max(lo);
            let mut best_start = None;

            for _ in 0..20 {
                if (hi - lo) <= 0.01 {
                    break;
                }
                let mid = (lo + hi) * 0.5;
                let candidate = paginator.page_at(&events, mid, candidate_positions);
                if candidate.positions.contains(end_line) {
                    let previous = std::mem::replace(&mut page, candidate);
                    candidate_positions = previous.positions.into_buffer();
                    best_start = Some(mid);
                    hi = mid;
                } else {
                    candidate_positions = candidate.positions.into_buffer();
                    lo = mid;
                }
            }
            self.frame_scratch.recycle_line_position_buffer(candidate_positions);

            if let Some(start) = best_start {
                self.viewport.back_start_offset_y = start;
            }
        }
        let first_painted = page.start_line;
        let paint_end = page.last_line.unwrap_or(first_painted).min(end_line);
        let first_painted_y = lines.get(first_painted).map(|line| line.point().y).unwrap_or(0.0);
        self.install_positioned_page(page, self.viewport.back_start_offset_y, Some(paint_end), true);

        self.viewport.start_offset_y = first_painted_y;
        self.viewport.next_start_offset_y = self.viewport.end_offset_y;

        if first_painted == 0 && self.viewport.start_offset_y <= 0.0 {
            self.viewport.direction = PaintDirection::Forward;
            self.viewport.start_offset_y = 0.0;
            self.viewport.end_offset_y = 0.0;
            self.viewport.back_anchor_end_line = None;
        }
    }

}

