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
    pub(crate) fn note_scene(&self, note: &crate::LaidOutDocument) -> Option<html::render::RenderScene> {
        let width = self.note_layout_width();
        let options = html::render::FragmentRenderOptions::new(width, None).ok()?.with_typography(self.root_font_size.max(1.0) as u32, self.root_font_size as f64).ok()?;
        Some(html::render::RenderScene::for_document(note, &options))
    }

    /// Width a note is laid out and shown at. The host decides; the reading
    /// column is the fallback when it has no opinion.
    pub(crate) fn note_layout_width(&self) -> f64 {
        self.host.note_popup_width().filter(|width| width.is_finite() && *width > 0.0).unwrap_or(self.page.layout.col_width)
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
        Paginator::new(DocQuery::new(&self.page.document), &self.page.layout, self.pipeline_inputs.layout.text_composition_policy.is_book_optimized(), self.vertical_rhythm)
    }

    fn install_positioned_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>, use_cached_inline_positions: bool) {
        let previous_positions = std::mem::replace(&mut self.page.frame.last_line_positions, page.positions);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
        self.page.frame.current_page_start_line = Some(page.start_line);
        self.page.frame.current_page_end_line = paint_end;
        self.page.frame.paint_start_offset_y = start_offset_y;
        self.page.frame.use_cached_inline_positions = use_cached_inline_positions;
        self.page.frame.offset_breaks = page.offset_breaks;
        self.page.frame.repeated_table_headers = page.repeated_table_headers;
    }

    pub(crate) fn prepare_forward(&mut self) {
        // render forward from current offset
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.frame.last_line_positions);
        let paginator = self.paginator();
        let events = paginator.events();
        let page = paginator.page_at(&events, self.viewport.start_offset_y, positions);
        self.viewport.next_start_offset_y = page.next_start_offset_y;
        self.viewport.reached_end = page.reached_end;
        let paint_end = page.last_line;
        self.install_positioned_page(page, self.viewport.start_offset_y, paint_end, false);
    }

    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let root = self.page.document.render_view();
        let text = root.text();
        let fragments = root.fragments();
        if fragments.images().is_empty() || text.line_count() == 0 {
            return;
        }
        let Some(start_line) = self.page.frame.current_page_start_line else {
            return;
        };
        let Some(end_line) = self.page.frame.current_page_end_line else {
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

        let lines = self.page.document.render_view().text().lines();
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

        let available_height = self.page.layout.col_count * self.page.layout.size.height;
        let end_line_data = lines.get(end_line).expect("end line must be within line count");
        let end_line_height = self.doc().effective_line_height(end_line, end_line_data.height());
        // Keep a tiny safety margin so the anchor line doesn't land exactly on the
        // bottom edge and get treated as overflow by column fit logic.
        let end_line_target_y = (available_height - end_line_height - 0.1).max(0.0);
        self.viewport.back_start_offset_y = (end_line_data.point().y - end_line_target_y).max(0.0);

        // Use the same forward position resolver as normal rendering so lines that share
        // the same document Y (e.g. table rows across columns) keep consistent screen Y.
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.page.frame.last_line_positions);
        // Both scratch buffers are claimed before the paginator borrows the
        // document and column layout, so the search below needs no further
        // mutable access while it runs.
        let mut candidate_positions = self.frame_scratch.take_line_position_buffer(&mut self.page.frame.last_line_positions);
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

