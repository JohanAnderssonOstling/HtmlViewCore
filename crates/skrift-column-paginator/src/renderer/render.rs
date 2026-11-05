use crate::{Painter, RendererCore};
use html::Document;
use html::render::RenderScene;
use html::scoped::ScopedContent;
use std::mem::take;

use html::pagination::{PageRequest, PaginationMode, Paginator};
use html_view_paint::PagePainter;

impl RendererCore {
    pub(crate) fn note_parts(&self) -> Option<(&ScopedContent, &Document)> {
        let note = self.note.as_ref()?;
        let owner = note.owner.as_ref().unwrap_or(&self.page.session);
        note.view.render_view(owner)?;
        Some((&note.view, owner))
    }

    pub(crate) fn note_parts_mut(&mut self) -> Option<(&mut ScopedContent, &Document)> {
        let note = self.note.as_mut()?;
        let owner = note.owner.as_ref().unwrap_or(&self.page.session);
        note.view.render_view(owner)?;
        Some((&mut note.view, owner))
    }

    pub(crate) fn painter(&self) -> PagePainter<'_> {
        PagePainter {
            geo: self.page.geometry(),
            scale: self.page.layout.scale,
            images: self.page.session.images(),
            reader_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            highlight: &self.highlight,
            annotations: &self.annotations,
            media_overlay: &self.media_overlay,
            selection: &self.page.selection,
        }
    }

    /// Record the popup through its owning document's resources. The engine
    /// paints the whole scope at its layout width; selection is painted separately.
    pub(crate) fn note_scene(&self) -> Option<RenderScene> {
        let (note, owner) = self.note_parts()?;
        note.scene(owner)
    }

    pub(crate) fn close_note(&mut self) {
        self.note = None;
        self.prefetch_images_for_current_page();
    }

    /// Paints the open note's selection highlight over its content. The note
    /// is framed whole, so the overlay walks every line it has.
    pub(crate) fn paint_note_selection(&mut self, cx: &mut impl Painter) {
        let mut scratch = take(&mut self.frame_scratch.note_selection);
        if let Some((note, owner)) = self.note_parts() {
            note.paint_selection(owner, self.interaction_palette.selection, cx, &mut scratch);
        }
        self.frame_scratch.note_selection = scratch;
    }

    /// Width a note is laid out and shown at. The host decides; the reading
    /// column is the fallback when it has no opinion.
    pub(crate) fn note_layout_width(&self) -> f64 {
        self.note_popup_width
            .filter(|width| width.is_finite() && *width > 0.0)
            .unwrap_or(self.page.layout.col_width)
    }

    pub(crate) fn paint_frame(&mut self, cx: &mut impl Painter) {
        let mut scratch = take(&mut self.frame_scratch.overlay);
        self.painter().paint_frame(cx, &mut scratch);
        self.frame_scratch.overlay = scratch;
    }

    pub(crate) fn paint_base(&self, cx: &mut impl Painter) {
        self.painter().paint_base(cx);
    }

    pub(crate) fn paint_interaction_overlay(&mut self, cx: &mut impl Painter) {
        let mut scratch = take(&mut self.frame_scratch.overlay);
        self.painter().paint_interaction_overlay(cx, &mut scratch);
        self.frame_scratch.overlay = scratch;
    }

    #[cfg(test)]
    pub(crate) fn paint_forward(&mut self, cx: &mut impl Painter) {
        self.prepare_page(self.viewport.start_line().map_or(
            PageRequest::StartingAtOffset(self.viewport.start_offset_y()),
            PageRequest::StartingAt,
        ));

        self.painter().paint_base(cx);
        self.paint_interaction_overlay(cx);
    }

    pub(crate) fn paginator(&self) -> Paginator<'_> {
        let mode = if self
            .page
            .configuration()
            .text_composition_policy()
            .is_book_optimized()
        {
            if self.vertical_rhythm {
                PaginationMode::BookWithVerticalRhythm
            } else {
                PaginationMode::Book
            }
        } else {
            PaginationMode::Web
        };
        Paginator::new(
            self.page.render_view(),
            self.page.layout.page_geometry(),
            mode,
        )
    }

    pub(crate) fn prepare_page(&mut self, request: PageRequest) {
        let positions = self.page.frame.take_line_positions().into_buffer();
        let mut scratch = std::mem::take(&mut self.frame_scratch.pagination);
        let page = self.paginator().compose(request, positions, &mut scratch);
        self.frame_scratch.pagination = scratch;
        self.viewport.install_page(&page);
        self.page.frame.install_page(page);
    }


    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let scopes = self
            .note
            .iter()
            .filter(|note| note.owner.is_none())
            .map(|note| &note.view);
        let page = self.page.frame.page();
        self.page
            .session
            .request_images(page.start_line().map(|_| page.content()), 1, scopes);
    }

}
