use crate::{PagePositions, Painter, RenderState, RendererCore};

// Temporary profiling switch: keep ordinary fragmentation active while
// bypassing table, forced-break, keep, widow/orphan, and semantic event work.
const SEMANTIC_PAGINATION_ENABLED: bool = cfg!(test);
use html_view_doc::DocQuery;
use html_view_paginate::Paginator;
use html_view_paint::PagePainter;

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

    /// Builds the renderer-owned note scene whole and at its own layout
    /// positions for a host showing it in a popup.
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
    /// The returned scene is sized and clipped to the width the note was laid
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

    pub(crate) fn close_note(&mut self) {
        self.note = None;
        self.note_document = None;
    }

    /// Paints the open note's selection highlight over its content. The note
    /// is framed whole, so the overlay walks every line it has.
    pub(crate) fn paint_note_selection(&self, cx: &mut impl Painter) {
        let Some(note) = self.note.as_ref() else {
            return;
        };
        if let Some(doc) = self.note_document {
            self.host.set_glyph_document(doc);
        }
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
        self.host.set_glyph_document(self.nav.location().document());
    }

    pub(crate) fn select_note_glyph_document(&self) {
        if let Some(doc) = self.note_document {
            self.host.set_glyph_document(doc);
        }
    }

    /// Width a note is laid out and shown at. The host decides; the reading
    /// column is the fallback when it has no opinion.
    pub(crate) fn note_layout_width(&self) -> f64 {
        self.host.note_popup_width().filter(|width| width.is_finite() && *width > 0.0).unwrap_or(self.page.view.layout.col_width)
    }

    pub(crate) fn paint_frame(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.location().document());
        self.painter().paint_base_before_overlay(cx);
        self.painter().paint_interaction_overlay(cx);
    }

    pub(crate) fn paint_base_before_overlay(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.location().document());
        self.painter().paint_base_before_overlay(cx);
    }

    pub(crate) fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        self.host.set_glyph_document(self.nav.location().document());
        self.painter().paint_interaction_overlay(cx);
    }

    pub(crate) fn paint_base_after_overlay(&self, cx: &mut impl Painter) -> RenderState {
        self.host.set_glyph_document(self.nav.location().document());
        self.painter().paint_base_after_overlay(cx)
    }

    #[cfg(test)]
    pub(crate) fn paint_forward(&mut self, cx: &mut impl Painter) -> RenderState {
        self.prepare_forward();
        self.host.set_glyph_document(self.nav.location().document());
        self.painter().paint_base_before_overlay(cx);
        self.painter().paint_interaction_overlay(cx);
        html_view_paginate::initial_render_state()
    }

    pub(crate) fn paginator(&self) -> Paginator<'_> {
        let mode = if self.page.inputs.layout.text_composition_policy.is_book_optimized() {
            if self.vertical_rhythm { html_view_paginate::PaginationMode::BookWithVerticalRhythm } else { html_view_paginate::PaginationMode::Book }
        } else {
            html_view_paginate::PaginationMode::Web
        };
        Paginator::new(DocQuery::new(&self.page.view.document), &self.page.view.layout, mode)
    }

    fn install_flow_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>) {
        let previous_positions = self.page.view.frame.install_flow_page(page, start_offset_y, paint_end);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
    }

    fn install_positioned_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>) {
        let previous_positions = self.page.view.frame.install_positioned_page(page, start_offset_y, paint_end);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
    }

    pub(crate) fn prepare_forward(&mut self) {
        // render forward from current offset
        let start_offset_y = self.viewport.composition().start_offset_y();
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.view.frame);
        let mut pagination_scratch = std::mem::take(&mut self.frame_scratch.pagination);
        let paginator = self.paginator();
        let page =
            if SEMANTIC_PAGINATION_ENABLED { paginator.page_at_with_scratch(start_offset_y, positions, &mut pagination_scratch) } else { paginator.page_at_without_semantics_with_scratch(start_offset_y, positions, &mut pagination_scratch) };
        self.frame_scratch.pagination = pagination_scratch;
        let continuation = if page.reached_end { crate::ViewportContinuation::EndedAt(page.next_start_offset_y) } else { crate::ViewportContinuation::MoreAt(page.next_start_offset_y) };
        self.viewport.update_forward_result(continuation);
        let paint_end = page.last_line;
        self.install_flow_page(page, start_offset_y, paint_end);
    }

    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let root = self.page.view.document.render_view();
        let text = root.text();
        let fragments = root.fragments();
        if fragments.images().is_empty() || text.line_count() == 0 {
            return;
        }
        let Some(start_line) = self.page.view.frame.page().start_line() else {
            return;
        };
        let Some(end_line) = self.page.view.frame.page().end_line() else {
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
    }

    pub(crate) fn prepare_backward(&mut self) {
        // render backward from end offset
        self.viewport.begin_backward_preparation();

        let lines = self.page.view.document.render_view().text().lines();
        let composition = self.viewport.composition();
        let backward = self.viewport.backward_composition();
        let end_line = if let Some(idx) = backward.anchor_end_line() {
            idx.min(lines.len().saturating_sub(1))
        } else if let Some(idx) = self.doc().end_line_at(composition.end_offset_y(), 1e-6) {
            idx
        } else {
            self.viewport.begin_forward_at(0.0);
            self.prepare_forward();
            return;
        };
        if end_line == 0 {
            self.viewport.begin_forward_at(0.0);
            self.prepare_forward();
            return;
        }

        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.view.frame);
        let mut pagination_scratch = std::mem::take(&mut self.frame_scratch.pagination);
        let paginator = self.paginator();
        let page = if SEMANTIC_PAGINATION_ENABLED {
            paginator.page_ending_at_with_scratch(end_line, positions, &mut pagination_scratch)
        } else {
            paginator.page_ending_at_without_semantics_with_scratch(end_line, positions, &mut pagination_scratch)
        };
        self.frame_scratch.pagination = pagination_scratch;
        let first_painted = page.start_line;
        let paint_end = page.last_line.unwrap_or(first_painted).min(end_line);
        let first_painted_y = lines.get(first_painted).map(|line| line.point().y).unwrap_or(0.0);
        self.viewport.set_backward_composition_start(first_painted_y);
        self.install_positioned_page(page, first_painted_y, Some(paint_end));

        self.viewport.finish_backward_composition(first_painted_y);

        if first_painted == 0 && self.viewport.composition().start_offset_y() <= 0.0 {
            self.viewport.begin_forward_at(0.0);
        }
    }
}
