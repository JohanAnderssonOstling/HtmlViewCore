use std::time::Duration;

use html_view_doc::DocQuery;
use html_view_paginate::Paginator;
use html_view_paint::PagePainter;
use crate::{PagePositions, PaintDirection, Painter, RenderState, RendererCore};

impl RendererCore {
    pub(crate) fn painter(&self) -> PagePainter<'_> {
        PagePainter {
            geo: self.text_geometry(),
            images: &self.image_pipeline,
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
    /// Images are resolved against an empty pipeline, so a note's images do
    /// not paint. A scoped note indexes its own image resources, while the
    /// page's pipeline holds the page's -- the same index means different
    /// pictures in each. Handing over the page's pipeline would not leave a
    /// note's image blank, it would draw whichever page image shared its
    /// index. Notes with images want a pipeline of their own.
    ///
    /// [`FootnotePreview`]: crate::FootnotePreview
    /// Height a note occupies at the width it was laid out to, so a host can
    /// size the popup it will be drawn in.
    pub(crate) fn note_height(&self, note: &crate::LaidOutDocument) -> f64 {
        let text = DocQuery::new(note).text();
        (0..text.line_count()).filter_map(|line_idx| text.line(line_idx)).map(|line| line.point().y + line.height()).fold(0.0, f64::max)
    }

    pub(crate) fn paint_note(&self, note: &crate::LaidOutDocument, cx: &mut impl Painter) {
        let frame = crate::VisibleFrame::default();
        let layout = crate::ColumnLayout { col_count: 1.0, ..self.layout };
        let geo = html_view_doc::TextGeometry::new(DocQuery::new(note), &frame, &layout);
        let (selection_state, table_state) = (crate::SelectionState::default(), crate::TableSelectionState::default());
        let images = html::resources::ImagePipeline::new(std::sync::Arc::new(Vec::new()), self.provider.clone());
        PagePainter {
            geo,
            images: &images,
            reader_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            highlight: &Default::default(),
            annotations: &Default::default(),
            media_overlay: &Default::default(),
            selection: html_view_select::SelectionView::new(geo, &selection_state, &table_state),
        }
        .paint_whole_document(cx);
    }

    pub(crate) fn paint_frame(&self, cx: &mut impl Painter) {
        self.painter().paint_frame(cx);
    }

    pub(crate) fn paint_base_before_overlay(&self, cx: &mut impl Painter) {
        self.painter().paint_base_before_overlay(cx);
    }

    pub(crate) fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        self.painter().paint_interaction_overlay(cx);
    }

    pub(crate) fn paint_base_after_overlay(&self, cx: &mut impl Painter) -> RenderState {
        self.painter().paint_base_after_overlay(cx)
    }

    #[cfg(test)]
    pub(crate) fn paint_forward(&mut self, cx: &mut impl Painter) -> RenderState {
        self.prepare_forward();
        self.painter().paint_frame(cx);
        html_view_paginate::initial_render_state()
    }

    pub(crate) fn paginator(&self) -> Paginator<'_> {
        Paginator::new(DocQuery::new(&self.document), &self.layout, self.pipeline_inputs.layout.text_composition_policy.is_book_optimized(), self.vertical_rhythm)
    }

    fn install_positioned_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>, use_cached_inline_positions: bool) {
        let previous_positions = std::mem::replace(&mut self.frame.last_line_positions, page.positions);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
        self.frame.current_page_start_line = Some(page.start_line);
        self.frame.current_page_end_line = paint_end;
        self.frame.paint_start_offset_y = start_offset_y;
        self.frame.use_cached_inline_positions = use_cached_inline_positions;
        self.frame.offset_breaks = page.offset_breaks;
        self.frame.repeated_table_headers = page.repeated_table_headers;
    }

    pub(crate) fn prepare_forward(&mut self) {
        // render forward from current offset
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);
        let paginator = self.paginator();
        let events = paginator.events();
        let page = paginator.page_at(&events, self.viewport.start_offset_y, positions);
        self.viewport.next_start_offset_y = page.next_start_offset_y;
        self.viewport.reached_end = page.reached_end;
        let paint_end = page.last_line;
        self.install_positioned_page(page, self.viewport.start_offset_y, paint_end, false);
    }

    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let root = self.document.render_view();
        let text = root.text();
        let fragments = root.fragments();
        if fragments.images().is_empty() || text.line_count() == 0 {
            return;
        }
        let Some(start_line) = self.frame.current_page_start_line else {
            return;
        };
        let Some(end_line) = self.frame.current_page_end_line else {
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

        self.image_pipeline.ensure_window(&desired);
        self.frame_scratch.desired_images = desired;
        if self.image_pipeline.has_pending() {
            self.host.schedule_repaint(Duration::from_millis(16));
        }
    }

    pub(crate) fn prepare_backward(&mut self) {
        // render backward from end offset
        self.viewport.reached_end = false;

        let lines = self.document.render_view().text().lines();
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

        let available_height = self.layout.col_count * self.layout.size.height;
        let end_line_data = lines.get(end_line).expect("end line must be within line count");
        let end_line_height = self.doc().effective_line_height(end_line, end_line_data.height());
        // Keep a tiny safety margin so the anchor line doesn't land exactly on the
        // bottom edge and get treated as overflow by column fit logic.
        let end_line_target_y = (available_height - end_line_height - 0.1).max(0.0);
        self.viewport.back_start_offset_y = (end_line_data.point().y - end_line_target_y).max(0.0);

        // Use the same forward position resolver as normal rendering so lines that share
        // the same document Y (e.g. table rows across columns) keep consistent screen Y.
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);
        // Both scratch buffers are claimed before the paginator borrows the
        // document and column layout, so the search below needs no further
        // mutable access while it runs.
        let mut candidate_positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);
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

