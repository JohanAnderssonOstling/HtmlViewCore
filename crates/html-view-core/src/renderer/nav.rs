use std::mem;
use std::rc::Rc;
use std::sync::Arc;

use crate::document_view::DocumentView;
use crate::spine_item::SpineItem;
use crate::{DocAnchor, GlyphShaper, LaidOutDocument, NavigationState, RendererCore, RendererEvent, RendererHost, RendererInitialConfig, ResourceProvider, TocEntry, ViewportState, load};
use html_view_doc::DocQuery;
use html_view_navigate::{NavContext, NavView};

#[path = "nav/href.rs"]
mod href;
#[path = "nav/notes.rs"]
mod notes;
#[path = "nav/spine.rs"]
mod spine;

impl RendererCore {
    pub(crate) fn nav_view(&self) -> NavView<'_> {
        NavView { doc: DocQuery::new(&self.page.view.document), viewport: &self.viewport, frame: &self.page.view.frame }
    }

    fn nav_cx(&mut self) -> (&mut NavigationState, NavContext<'_>) {
        // Disjoint field borrows: navigation state is mutated through the
        // first, everything it reads or writes through the second.
        (&mut self.nav, NavContext { doc: DocQuery::new(&self.page.view.document), viewport: &mut self.viewport, frame: &self.page.view.frame, host: self.host.as_ref() })
    }

    pub fn restore_glyph_position(&mut self, glyph_idx: u32) {
        let (nav, mut cx) = self.nav_cx();
        nav.restore_glyph_position(&mut cx, glyph_idx);
    }

    pub fn restore_position_from_cfi(&mut self, cfi: &str) -> bool {
        let (nav, mut cx) = self.nav_cx();
        nav.restore_position_from_cfi(&mut cx, cfi)
    }

    pub fn next_line(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let (nav, mut cx) = self.nav_cx();
        if nav.next_line(&mut cx) {
            return;
        }
        self.continue_past_page(glyph_shaper);
    }

    pub fn prev_line(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.prev_line(&mut cx);
    }

    pub(crate) fn update_nav_signal(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.update_nav_signal(&mut cx);
    }

    pub(crate) fn set_progress_fraction(&mut self, glyph_shaper: &mut impl GlyphShaper, fraction: f32) {
        let (doc, in_doc_fraction) = self.nav.document_at_fraction(fraction);
        self.set_position(glyph_shaper, doc, None);
        let glyph_count = self.page.view.document.render_view().text().glyph_count();
        let glyph = (in_doc_fraction * glyph_count as f64) as u32;
        self.restore_glyph_position(glyph.min(glyph_count.saturating_sub(1) as u32));
        self.update_nav_signal();
        self.host.request_repaint();
    }

    pub fn from_provider_with_nav(
        host: Rc<dyn RendererHost>, glyph_shaper: &mut impl GlyphShaper, provider: Arc<dyn ResourceProvider>, document_uris: Vec<String>, start_index: usize, nav_state: Option<&str>, config: RendererInitialConfig,
    ) -> Self {
        assert!(!document_uris.is_empty(), "No HTML documents found for source");
        // Reading positions are publication-scoped state owned by the host.
        // Never fall back to a process-global file: `None` means this
        // publication has no saved position and must open at `start_index`.
        let nav_state = nav_state.map(str::to_owned);
        let restoring_position = nav_state.is_some();
        let (doc_index, glyph_idx) = nav_state.as_deref().and_then(Self::parse_nav_state).unwrap_or((start_index, 0));
        let doc_index = doc_index.min(document_uris.len().saturating_sub(1));
        let font_size = config.font_size.clamp(8.0, 64.0).round();
        let column_width = config.column_width.clamp(200.0, 1200.0).round();
        let loaded = load::load_document_with_settings(
            provider.clone(),
            &document_uris[doc_index],
            load::DocumentLoadSettings {
                root_font_size: font_size,
                column_width,
                reader_overrides: config.style_overrides.clone(),
                image_sizing_policy: config.image_sizing_policy,
                text_composition_policy: config.text_composition_policy,
                note_display: config.note_display,
            },
            glyph_shaper,
        );
        let mut renderer = Self::new(host, loaded, provider, document_uris, doc_index, config);

        if nav_state.as_deref().is_some_and(|state| state.trim().starts_with("epubcfi(") && renderer.restore_position_from_cfi(state.trim())) {
            renderer.update_nav_signal();
            return renderer;
        }
        if glyph_idx > 0 {
            renderer.restore_glyph_position(glyph_idx);
        }
        if restoring_position && renderer.nav.anchor_glyph_here().is_none() {
            let glyph = renderer.nav.current_glyph_position(renderer.nav_view());
            renderer.nav.anchor_at(glyph);
        }
        renderer.update_nav_signal();
        renderer
    }

    pub fn build_document_toc_entries_for(document: &LaidOutDocument) -> Vec<TocEntry> {
        fn convert_nodes(view: html::layout::RenderView<'_>, nodes: &[crate::DocumentTocNode]) -> Vec<TocEntry> {
            nodes.iter().map(|node| TocEntry { title: view.string(node.title).to_string(), link: view.string(node.href).to_string(), children: convert_nodes(view, node.children.as_slice()) }).collect()
        }

        let view = document.render_view();
        convert_nodes(view, view.document_toc_entries())
    }

    /// Builds a spine item out of a freshly loaded document, at the column
    /// layout the reader is using.
    pub fn parse_nav_state(raw: &str) -> Option<(usize, u32)> {
        use crate::cfi;

        let trimmed = raw.trim();

        if trimmed.starts_with("epubcfi(") {
            let doc_idx = cfi::parse_cfi_spine_only(trimmed)?;
            return Some((doc_idx, 0));
        }

        if let Some((doc_part, glyph_part)) = trimmed.split_once(':') {
            let doc_idx = doc_part.parse::<usize>().ok()?;
            let glyph_idx = glyph_part.parse::<u32>().ok()?;
            return Some((doc_idx, glyph_idx));
        }

        trimmed.parse::<u32>().ok().map(|glyph| (0, glyph))
    }

    pub fn next_page(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let composition = self.viewport.composition();
        let continuation = self.viewport.continuation();
        if continuation.reached_end() {
            self.continue_past_page(glyph_shaper);
            return;
        }

        let mut next_start_offset_y = composition.start_offset_y();
        if let Some(end_idx) = self.page.view.frame.page().end_line() {
            let next_idx = end_idx + 1;
            if let Some(line) = self.page.view.document.render_view().text().line(next_idx) {
                next_start_offset_y = line.point().y;
            }
        } else if continuation.next_start_offset_y() > composition.start_offset_y() {
            next_start_offset_y = continuation.next_start_offset_y();
        }
        self.cache_current_prepared_page();
        let (nav, mut cx) = self.nav_cx();
        nav.advance_to_forward_offset(&mut cx, next_start_offset_y);
    }

    /// Moves on when the page has nothing further to show.
    ///
    fn continue_past_page(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let next = self.nav.location().document() + 1;
        if next < self.nav.documents().uris().len() {
            self.load_document_at(glyph_shaper, next, DocAnchor::Start);
        }
    }

    pub fn prev_page(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let start_offset_y = self.viewport.composition().start_offset_y();
        if self.page.view.frame.page().start_line() == Some(0) || start_offset_y <= 0.0 {
            if self.nav.location().document() > 0 {
                self.load_document_at(glyph_shaper, self.nav.location().document() - 1, DocAnchor::End);
                return;
            }
            let (nav, mut cx) = self.nav_cx();
            nav.restore_forward_offset(&mut cx, 0.0);
            return;
        }

        let back_anchor_end_line = self.page.view.frame.page().start_line().and_then(|idx| idx.checked_sub(1));
        let end_offset_y = back_anchor_end_line.and_then(|anchor_idx| self.page.view.document.render_view().text().line(anchor_idx)).map(|line| line.point().y + line.height()).unwrap_or(start_offset_y);
        self.cache_current_prepared_page();
        if let Some(anchor_idx) = back_anchor_end_line
            && self.restore_prepared_page_ending_at(anchor_idx)
        {
            self.host.request_repaint();
            self.nav.mark_anchor_dirty();
            return;
        }
        self.viewport.begin_backward_at(end_offset_y, back_anchor_end_line);
        self.host.request_repaint();
        self.nav.mark_anchor_dirty();
    }

    pub fn next_document(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.nav.location().document() + 1 < self.nav.documents().uris().len() {
            self.load_document_at(glyph_shaper, self.nav.location().document() + 1, DocAnchor::Start);
        }
    }

    pub fn prev_document(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.nav.location().document() > 0 {
            self.load_document_at(glyph_shaper, self.nav.location().document() - 1, DocAnchor::End);
        }
    }

    pub fn relayout_for_column_width(&mut self, glyph_shaper: &mut impl GlyphShaper, column_width: f64) -> Result<(), html::pipeline::PipelineError> {
        let next = self.next_pipeline_inputs_for_layout(column_width);
        self.apply_pipeline_inputs(next, glyph_shaper)
    }

    pub fn set_root_font_size(&mut self, glyph_shaper: &mut impl GlyphShaper, new_size: f32) {
        let clamped = new_size.clamp(8.0, 64.0);
        let rounded = clamped.round();
        if (rounded - self.root_font_size).abs() < f32::EPSILON {
            return;
        }
        let next = self.next_pipeline_inputs_for_style(rounded);
        match self.apply_pipeline_inputs(next, glyph_shaper) {
            Ok(()) => self.host.emit(RendererEvent::FontSizeChanged(self.root_font_size)),
            Err(error) => self.emit_operation_failed(crate::RendererOperation::UpdateDocument, error.to_string()),
        }
    }

    pub fn set_col_width(&mut self, glyph_shaper: &mut impl GlyphShaper, new_width: f64) {
        let clamped = new_width.clamp(200.0, 1200.0);
        let rounded = clamped.round();
        let base_changed = (rounded - self.page.view.layout.base_col_width).abs() >= f64::EPSILON;
        let effective = if self.page.view.layout.size.width > 0.0 { rounded.min(self.page.view.layout.size.width) } else { rounded };
        let effective_changed = (effective - self.page.view.layout.col_width).abs() >= 0.1;
        if !base_changed && !effective_changed {
            return;
        }
        if effective_changed && let Err(error) = self.relayout_for_column_width(glyph_shaper, effective) {
            self.emit_operation_failed(crate::RendererOperation::UpdateDocument, error.to_string());
            return;
        }
        let base_width = base_changed.then_some(rounded);
        let effective_width = effective_changed.then_some(effective);
        self.page.view.layout.commit_column_width(base_width, effective_width);
        if base_changed {
            self.host.emit(RendererEvent::ColumnWidthChanged(self.page.view.layout.base_col_width));
        }
    }

    pub fn set_scale(&mut self, new_scale: f64) {
        let Some(scale) = self.page.view.layout.update_scale(new_scale) else {
            return;
        };
        self.host.emit(RendererEvent::ScaleChanged(scale));
        self.host.request_repaint();
    }

    pub fn reset_view_state(&mut self) {
        self.viewport = ViewportState::default();
        self.page.view.reset_view_state();
        self.frame_scratch.clear_transient();
    }
}

#[cfg(test)]
#[path = "nav/tests.rs"]
mod tests;
