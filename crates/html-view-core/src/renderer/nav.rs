use std::mem;
use std::rc::Rc;
use std::sync::Arc;

use html_view_doc::DocQuery;
use html_view_navigate::{NavContext, NavView};
use crate::document_cache::CachedDocument;
use crate::{NavigationState,  DocAnchor, GlyphShaper, ImagePipeline, LaidOutDocument, PaintDirection, RendererCore, RendererEvent, RendererHost, RendererInitialConfig, ResourceProvider, SelectionState, TocEntry, ViewportState, load};

impl RendererCore {
    pub(crate) fn nav_view(&self) -> NavView<'_> {
        NavView { doc: DocQuery::new(&self.document), viewport: &self.viewport, frame: &self.frame }
    }

    fn nav_cx(&mut self) -> (&mut NavigationState, NavContext<'_>) {
        // Disjoint field borrows: navigation state is mutated through the
        // first, everything it reads or writes through the second.
        (&mut self.nav, NavContext { doc: DocQuery::new(&self.document), viewport: &mut self.viewport, frame: &self.frame, host: self.host.as_ref() })
    }

    pub fn current_glyph_position(&self) -> Option<u32> {
        self.nav.current_glyph_position(self.nav_view())
    }

    pub fn current_cfi(&self) -> Option<String> {
        self.nav.current_cfi(self.nav_view())
    }

    fn current_location(&self) -> html_view_navigate::Location {
        self.nav.current_location(self.nav_view())
    }

    pub fn restore_glyph_position(&mut self, glyph_idx: u32) {
        let (nav, mut cx) = self.nav_cx();
        nav.restore_glyph_position(&mut cx, glyph_idx);
    }

    pub fn restore_position_from_cfi(&mut self, cfi: &str) -> bool {
        let (nav, mut cx) = self.nav_cx();
        nav.restore_position_from_cfi(&mut cx, cfi)
    }

    fn jump_to_id(&mut self, id: &str) -> bool {
        let (nav, mut cx) = self.nav_cx();
        nav.jump_to_id(&mut cx, id)
    }

    pub fn next_line(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.next_line(&mut cx);
    }

    pub fn prev_line(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.prev_line(&mut cx);
    }

    fn finish_document_navigation(&mut self, anchor: DocAnchor) {
        let (nav, mut cx) = self.nav_cx();
        nav.finish_document_navigation(&mut cx, anchor);
    }

    pub(crate) fn update_nav_signal(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.update_nav_signal(&mut cx);
    }

    pub(crate) fn update_nav_anchor_from_layout(&mut self) {
        let view = NavView { doc: DocQuery::new(&self.document), viewport: &self.viewport, frame: &self.frame };
        self.nav.update_nav_anchor_from_layout(view);
    }

    pub fn update_toc_anchor_filter(&mut self) {
        let view = NavView { doc: DocQuery::new(&self.document), viewport: &self.viewport, frame: &self.frame };
        self.nav.update_toc_anchor_filter(view);
    }

    fn update_history_availability_signals(&mut self) {
        self.nav.update_history_availability_signals(self.host.as_ref());
    }

    fn push_history_location(&mut self, location: html_view_navigate::Location) {
        self.nav.push_history_location(location, self.host.as_ref());
    }

    fn push_current_location_if_missing(&mut self) {
        let (nav, mut cx) = self.nav_cx();
        nav.push_current_location_if_missing(&mut cx);
    }

    fn jump_to_forward_offset(&mut self, start_offset_y: f64, pending_nav_anchor_update: bool) {
        self.viewport.direction = PaintDirection::Forward;
        self.viewport.reached_end = false;
        self.viewport.back_anchor_end_line = None;
        self.viewport.start_offset_y = start_offset_y;
        self.viewport.end_offset_y = self.viewport.start_offset_y;
        self.host.request_repaint();
        self.nav.pending_nav_anchor_update = pending_nav_anchor_update;
    }

    fn find_doc_index_by_uri_or_suffix(&self, candidate: &str) -> Option<usize> {
        self.nav.find_doc_index_by_uri_or_suffix(candidate)
    }

    fn fragment_part(fragment: &str) -> Option<String> {
        NavigationState::fragment_part(fragment)
    }

    pub(crate) fn set_progress_fraction(&mut self, glyph_shaper: &mut impl GlyphShaper, fraction: f32) {
        let (doc, in_doc_fraction) = self.nav.document_at_fraction(fraction);
        self.set_position(glyph_shaper, doc, None);
        let glyph_count = self.document.render_view().text().glyph_count();
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
        let loaded = load::load_document_with_settings(provider.clone(), &document_uris[doc_index], font_size, column_width, config.style_overrides.clone(), config.image_sizing_policy, config.text_composition_policy, config.note_display, glyph_shaper);
        let mut renderer = Self::new(host, loaded, provider, document_uris, doc_index, config);

        if nav_state.as_deref().is_some_and(|state| state.trim().starts_with("epubcfi(") && renderer.restore_position_from_cfi(state.trim())) {
            renderer.update_nav_signal();
            return renderer;
        }
        if glyph_idx > 0 {
            renderer.restore_glyph_position(glyph_idx);
        }
        if restoring_position && renderer.nav.nav_anchor_glyph.is_none() {
            renderer.nav.nav_anchor_glyph = renderer.current_glyph_position();
        }
        renderer.update_nav_signal();
        renderer
    }

    pub fn toc(&self) -> std::io::Result<Option<Vec<TocEntry>>> {
        self.provider.toc()
    }

    pub fn build_document_toc_entries_for(document: &LaidOutDocument) -> Vec<TocEntry> {
        fn convert_nodes(view: html::layout::RenderView<'_>, nodes: &[crate::DocumentTocNode]) -> Vec<TocEntry> {
            nodes.iter().map(|node| TocEntry { title: view.string(node.title).to_string(), link: view.string(node.href).to_string(), children: convert_nodes(view, node.children.as_slice()) }).collect()
        }

        let view = document.render_view();
        convert_nodes(view, view.document_toc_entries())
    }

    fn replace_document_state(&mut self, loaded: load::LoadedRenderDocument, sync_doc_title: bool) {
        self.document = loaded.document;
        self.pipeline_session = loaded.session;
        self.pipeline_inputs = loaded.inputs;
        self.root_font_size = self.pipeline_inputs.style_environment.root_font_size as f32;
        self.revisions.invalidate_pipeline_from(html::pipeline::EarliestStage::Parse);
        self.revisions.invalidate_resources();
        if sync_doc_title {
            self.host.emit(RendererEvent::TitleChanged(self.document.render_view().title().map(str::to_owned)));
        }
        self.host.emit(RendererEvent::TocChanged(Self::build_document_toc_entries_for(&self.document)));
        self.update_toc_anchor_filter();
        let image_resources = self.document.render_view().images().to_vec();
        self.image_pipeline = ImagePipeline::new(Arc::new(image_resources), self.provider.clone());
        self.resolve_visible_annotations();
        self.resolve_media_overlay_highlight();
    }

    fn cache_current_document(&mut self) {
        if self.document_cache.is_disabled() {
            return;
        }

        let current_index = self.nav.current_doc_index;
        let cached = CachedDocument { session: mem::replace(&mut self.pipeline_session, html::engine::Engine::new(self.provider.clone())), inputs: self.pipeline_inputs.clone(), document: self.document.clone() };
        self.document_cache.insert_document(current_index, cached);
    }

    fn rebased_inputs_for_doc_index(&self, doc_index: usize, source_inputs: &html::pipeline::PipelineInputs) -> html::pipeline::PipelineInputs {
        let mut next_inputs = source_inputs.clone();
        if let Some(uri) = self.nav.document_uris.get(doc_index) {
            next_inputs.base_uri = uri.clone();
            next_inputs.markup_syntax = html::pipeline::MarkupSyntax::from_uri(uri);
        }
        next_inputs.style_environment = self.pipeline_inputs.style_environment;
        next_inputs.user_styles = self.pipeline_inputs.user_styles.clone();
        next_inputs.layout = html::pipeline::LayoutConstraints {
            viewport_width: self.pipeline_inputs.layout.viewport_width,
            viewport_height: self.pipeline_inputs.layout.viewport_height,
            line_height: self.pipeline_inputs.style_environment.root_font_size as f64,
            image_sizing_policy: self.pipeline_inputs.layout.image_sizing_policy,
            text_composition_policy: self.pipeline_inputs.layout.text_composition_policy,
        };
        next_inputs.resource_revision = self.pipeline_inputs.resource_revision;
        next_inputs.stylesheet_revision = self.pipeline_inputs.stylesheet_revision;
        next_inputs.font_environment = self.pipeline_inputs.font_environment;
        next_inputs.image_metrics_revision = self.pipeline_inputs.image_metrics_revision;
        next_inputs.image_metrics = self.pipeline_inputs.image_metrics.clone();
        next_inputs.paint = self.pipeline_inputs.paint;
        next_inputs
    }

    fn load_document_for_index(&mut self, glyph_shaper: &mut impl GlyphShaper, doc_index: usize) -> load::LoadedRenderDocument {
        let uri = self.nav.document_uris[doc_index].clone();
        load::load_document_with_settings(
            self.provider.clone(),
            &uri,
            self.root_font_size,
            self.pipeline_inputs.layout.viewport_width,
            self.pipeline_inputs.reader_overrides.clone(),
            self.pipeline_inputs.layout.image_sizing_policy,
            self.pipeline_inputs.layout.text_composition_policy,
            self.note_display,
            glyph_shaper,
        )
    }

    pub fn handle_link_click(&mut self, glyph_shaper: &mut impl GlyphShaper, glyph_idx: u32) -> bool {
        let view = self.document.render_view();
        let addressing = view.addressing();
        let Some(href_idx) = addressing.link_for_glyph(glyph_idx) else {
            return false;
        };
        let explicit_noteref = addressing.is_note_reference(glyph_idx);
        let href = view.string(href_idx).to_string();
        if let Some(preview) = self.footnote_preview_for_reference(glyph_shaper, &href, explicit_noteref) {
            self.host.emit(RendererEvent::FootnoteOpened(preview));
            return true;
        }
        self.navigate_to_href(glyph_shaper, &href);
        true
    }

    fn footnote_preview(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str) -> Option<crate::FootnotePreview> {
        self.footnote_preview_for_reference(glyph_shaper, href, false)
    }

    /// Builds the preview for a note reference by asking the engine to lay the
    /// note out on its own, rather than re-deriving it from source. The note is
    /// held out of the reading flow, so it owns no glyphs in the document on
    /// screen; a scoped layout is what makes its content available.
    ///
    /// There is nothing to preview when notes read in place: the reference
    /// resolves as an ordinary link to text the reader can already see.
    fn footnote_preview_for_reference(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str, explicit_noteref: bool) -> Option<crate::FootnotePreview> {
        if self.note_display == crate::NoteDisplay::AsAuthored {
            return None;
        }
        let (_, fragment) = href.split_once('#')?;
        if fragment.is_empty() {
            return None;
        }
        let (doc, _) = self.resolve_href_target(href)?;

        // A note in another spine document has to be laid out before it can be
        // asked anything. Loading it is not wasted: a reader following the
        // reference lands there next, and the cache keeps it.
        let mut freshly_loaded = None;
        if doc != self.nav.current_doc_index && self.document_cache.document(doc).is_none() {
            self.nav.document_uris.get(doc)?;
            freshly_loaded = Some(self.load_document_for_index(glyph_shaper, doc));
        }

        let (session, laid_out) = match freshly_loaded.as_ref() {
            Some(loaded) => (&loaded.session, &loaded.document),
            None if doc == self.nav.current_doc_index => (&self.pipeline_session, &self.document),
            None => {
                let cached = self.document_cache.document(doc)?;
                (&cached.session, &cached.document)
            }
        };

        // One authority for what a note is, whichever document it lives in.
        if !explicit_noteref && !laid_out.render_view().addressing().is_note_target(fragment) {
            return None;
        }

        let constraints = html::layout::LayoutConstraints::new(self.layout.col_width, self.root_font_size as f64).ok()?;
        let note = session.layout_note(fragment, constraints, glyph_shaper)?;
        let blocks = Self::note_preview_blocks(&note);

        if let Some(loaded) = freshly_loaded {
            self.document_cache.insert_document(doc, CachedDocument { session: loaded.session, inputs: loaded.inputs, document: loaded.document });
        }
        (!blocks.is_empty()).then(|| crate::FootnotePreview { href: href.to_owned(), blocks })
    }

    /// Flattens a laid-out note into the block-per-line shape the host renders.
    /// Structure comes from the note's own boxes, so list items and quotations
    /// keep the marking they had in the document.
    fn note_preview_blocks(note: &LaidOutDocument) -> Vec<String> {
        const BLOCK_TAGS: [&str; 10] = ["h1", "h2", "h3", "h4", "h5", "h6", "p", "li", "blockquote", "dd"];
        let doc = DocQuery::new(note);
        let boxes = doc.boxes();
        let text = doc.text();
        let glyph_text = |ranges: Vec<std::ops::Range<u32>>| {
            let raw = ranges
                .into_iter()
                .flat_map(|range| range.filter_map(|glyph| text.glyph_at(glyph as usize).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())))
                .collect::<String>();
            raw.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        let mut blocks = Vec::new();
        for box_idx in 0..boxes.len() {
            let Some(tag) = boxes.tag(box_idx) else { continue };
            let Some(matched) = BLOCK_TAGS.iter().find(|candidate| tag.eq_ignore_ascii_case(candidate)) else { continue };
            // A block nested inside one already taken would repeat its text.
            if doc.boxes().ancestors(box_idx).any(|ancestor| boxes.tag(ancestor).is_some_and(|tag| BLOCK_TAGS.iter().any(|candidate| tag.eq_ignore_ascii_case(candidate)))) {
                continue;
            }
            let content = glyph_text(doc.box_text_glyph_ranges(box_idx, false));
            if content.is_empty() {
                continue;
            }
            blocks.push(match *matched {
                "li" => format!("\u{2022} {content}"),
                "blockquote" => format!("\u{201c}{content}\u{201d}"),
                _ => content,
            });
        }

        if blocks.is_empty() {
            let whole = glyph_text(vec![0..text.glyph_count() as u32]);
            if !whole.is_empty() {
                blocks.push(whole);
            }
        }
        blocks
    }

    fn resolve_doc_index_for_path(&self, normalized_path: &str) -> Option<usize> {
        self.find_doc_index_by_uri_or_suffix(normalized_path).or_else(|| {
            let base_uri = self.nav.document_uris.get(self.nav.current_doc_index).map(|s: &String| s.as_str()).unwrap_or("");
            let resolved = self.provider.resolve(base_uri, normalized_path);
            self.find_doc_index_by_uri_or_suffix(resolved.as_str())
        })
    }

    pub(crate) fn resolve_href_target(&self, href: &str) -> Option<(usize, Option<String>)> {
        if let Some(anchor) = href.strip_prefix('#') {
            return Some((self.nav.current_doc_index, Self::fragment_part(anchor)));
        }

        let (path_part, fragment) = href.split_once('#').unwrap_or((href, ""));
        let fragment = Self::fragment_part(fragment);
        if path_part.is_empty() {
            return Some((self.nav.current_doc_index, fragment));
        }

        let normalized_path = normalize_href_path(path_part);
        let Some(idx) = self.resolve_doc_index_for_path(normalized_path.as_str()) else {
            let base_uri = self.nav.document_uris.get(self.nav.current_doc_index).map(|s: &String| s.as_str()).unwrap_or("");
            println!("resolve_href_target: no match for href '{}' (path='{}', normalized='{}', base='{}')", href, path_part, normalized_path, base_uri);
            return None;
        };
        Some((idx, fragment))
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.resolve_href_target(href)
    }

    pub fn navigate_to_href(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str) -> bool {
        self.push_current_location_if_missing();

        let from = self.current_location();
        let Some((doc_index, fragment)) = self.resolve_href_target(href) else {
            return false;
        };
        let to = (doc_index, fragment.clone());
        if doc_index != self.nav.current_doc_index {
            self.load_document_at(glyph_shaper, doc_index, DocAnchor::Start);
        }
        if let Some(fragment) = fragment.as_deref()
            && !self.jump_to_id(fragment)
        {
            return false;
        }
        if from != to {
            self.push_history_location(from);
            self.push_history_location(to);
        }
        true
    }

    fn navigate_history_step(&mut self, glyph_shaper: &mut impl GlyphShaper, step: isize) -> bool {
        let Some(target) = self.nav.history.step(step, self.host.as_ref()) else {
            return false;
        };
        let ok = self.navigate_to_location(glyph_shaper, target);
        self.update_history_availability_signals();
        ok
    }

    pub fn navigate_history_back(&mut self, glyph_shaper: &mut impl GlyphShaper) -> bool {
        self.navigate_history_step(glyph_shaper, -1)
    }

    pub fn navigate_history_forward(&mut self, glyph_shaper: &mut impl GlyphShaper) -> bool {
        self.navigate_history_step(glyph_shaper, 1)
    }

    fn navigate_to_location(&mut self, glyph_shaper: &mut impl GlyphShaper, target: (usize, Option<String>)) -> bool {
        let (doc_index, anchor) = target;
        if doc_index != self.nav.current_doc_index {
            self.load_document_at(glyph_shaper, doc_index, DocAnchor::Start);
        }
        if let Some(anchor) = anchor.as_deref() {
            self.jump_to_id(anchor)
        } else {
            self.jump_to_forward_offset(0.0, false);
            true
        }
    }

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
        if self.viewport.reached_end {
            if self.nav.current_doc_index + 1 < self.nav.document_uris.len() {
                self.load_document_at(glyph_shaper, self.nav.current_doc_index + 1, DocAnchor::Start);
                return;
            }
            return;
        }

        let mut next_start_offset_y = self.viewport.start_offset_y;
        if let Some(end_idx) = self.frame.current_page_end_line {
            let next_idx = end_idx + 1;
            if let Some(line) = self.document.render_view().text().line(next_idx) {
                next_start_offset_y = line.point().y;
            }
        } else if self.viewport.next_start_offset_y > self.viewport.start_offset_y {
            next_start_offset_y = self.viewport.next_start_offset_y;
        }
        self.cache_current_prepared_page();
        self.jump_to_forward_offset(next_start_offset_y, true);
    }

    pub fn prev_page(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.frame.current_page_start_line == Some(0) || self.viewport.start_offset_y <= 0.0 {
            if self.nav.current_doc_index > 0 {
                self.load_document_at(glyph_shaper, self.nav.current_doc_index - 1, DocAnchor::End);
                return;
            }
            self.jump_to_forward_offset(0.0, false);
            return;
        }

        let back_anchor_end_line = self.frame.current_page_start_line.and_then(|idx| idx.checked_sub(1));
        let end_offset_y = back_anchor_end_line.and_then(|anchor_idx| self.document.render_view().text().line(anchor_idx)).map(|line| line.point().y + line.height()).unwrap_or(self.viewport.start_offset_y);
        self.cache_current_prepared_page();
        if let Some(anchor_idx) = back_anchor_end_line
            && self.restore_prepared_page_ending_at(anchor_idx)
        {
            self.host.request_repaint();
            self.nav.pending_nav_anchor_update = true;
            return;
        }
        self.viewport.direction = PaintDirection::Backward;
        self.viewport.back_anchor_end_line = back_anchor_end_line;
        self.viewport.end_offset_y = end_offset_y;
        self.host.request_repaint();
        self.nav.pending_nav_anchor_update = true;
    }

    pub fn next_document(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.nav.current_doc_index + 1 < self.nav.document_uris.len() {
            self.load_document_at(glyph_shaper, self.nav.current_doc_index + 1, DocAnchor::Start);
        }
    }

    pub fn prev_document(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.nav.current_doc_index > 0 {
            self.load_document_at(glyph_shaper, self.nav.current_doc_index - 1, DocAnchor::End);
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
        let base_changed = (rounded - self.layout.base_col_width).abs() >= f64::EPSILON;
        let effective = if self.layout.size.width > 0.0 { rounded.min(self.layout.size.width) } else { rounded };
        let effective_changed = (effective - self.layout.col_width).abs() >= 0.1;
        if !base_changed && !effective_changed {
            return;
        }
        if effective_changed && let Err(error) = self.relayout_for_column_width(glyph_shaper, effective) {
            self.emit_operation_failed(crate::RendererOperation::UpdateDocument, error.to_string());
            return;
        }
        if base_changed {
            self.layout.base_col_width = rounded;
            self.host.emit(RendererEvent::ColumnWidthChanged(self.layout.base_col_width));
        }
        if effective_changed {
            self.layout.col_width = effective;
        }
    }

    pub fn set_scale(&mut self, new_scale: f64) {
        let clamped = new_scale.clamp(0.5, 3.0);
        if (clamped - self.layout.scale).abs() < f64::EPSILON {
            return;
        }
        self.layout.scale = clamped;
        self.host.emit(RendererEvent::ScaleChanged(self.layout.scale));
        self.host.request_repaint();
    }

    pub fn load_document_at(&mut self, glyph_shaper: &mut impl GlyphShaper, doc_index: usize, anchor: DocAnchor) {
        if doc_index >= self.nav.document_uris.len() {
            return;
        }

        if doc_index == self.nav.current_doc_index {
            match anchor {
                DocAnchor::Start => {
                    self.viewport.direction = PaintDirection::Forward;
                    self.viewport.start_offset_y = 0.0;
                    self.viewport.end_offset_y = 0.0;
                }
                DocAnchor::End => {
                    self.viewport.direction = PaintDirection::Backward;
                    if let Some(last) = self.document.render_view().text().lines().last() {
                        self.viewport.end_offset_y = last.point().y + last.height();
                        self.viewport.reached_end = true;
                        self.viewport.back_anchor_end_line = Some(self.document.render_view().text().line_count().saturating_sub(1));
                    }
                }
                DocAnchor::Glyph(glyph_idx) => {
                    self.restore_glyph_position(glyph_idx);
                }
            }
            self.host.request_repaint();
            if matches!(anchor, DocAnchor::Glyph(_)) {
                self.update_nav_signal();
            } else {
                self.nav.pending_nav_anchor_update = true;
            }
            return;
        }

        self.cache_current_document();

        if let Some(mut cached) = self.document_cache.take_document(doc_index) {
            let Ok(document) = cached.session.rehydrate_glyphs(glyph_shaper) else {
                let fallback = self.load_document_for_index(glyph_shaper, doc_index);
                self.nav.current_doc_index = doc_index;
                self.replace_document_state(fallback, true);
                self.reset_view_state();
                self.finish_document_navigation(anchor);
                return;
            };
            cached.document = document;
            let requested_inputs = self.rebased_inputs_for_doc_index(doc_index, &cached.inputs);
            if cached.inputs == requested_inputs {
                self.nav.current_doc_index = doc_index;
                self.replace_document_state(load::LoadedRenderDocument { session: cached.session, inputs: cached.inputs, document: cached.document, source: requested_inputs.source }, true);
            } else {
                self.nav.current_doc_index = doc_index;
                let requested_source = requested_inputs.source.clone();
                self.replace_document_state(load::LoadedRenderDocument { session: cached.session, inputs: cached.inputs, document: cached.document, source: requested_source }, false);
                if self.apply_pipeline_inputs(requested_inputs, glyph_shaper).is_err() {
                    let fallback = self.load_document_for_index(glyph_shaper, doc_index);
                    self.nav.current_doc_index = doc_index;
                    self.replace_document_state(fallback, true);
                }
            }
            self.reset_view_state();
        } else {
            let document = self.load_document_for_index(glyph_shaper, doc_index);
            self.nav.current_doc_index = doc_index;
            self.replace_document_state(document, true);
            self.reset_view_state();
        }

        self.finish_document_navigation(anchor);
    }

    pub fn reset_view_state(&mut self) {
        self.viewport = ViewportState::default();
        self.frame.clear();
        self.frame_scratch.clear_transient();
        self.selection = SelectionState::default();
        self.table_selection = crate::TableSelectionState::default();
    }

}

fn normalize_href_path(path: &str) -> String {
    if path.starts_with('/') || path.contains(':') {
        return path.to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use kurbo::{Point, Size};

    use crate::text_backend::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper};
    use crate::{FileSystemProvider, RecordingPainter, RendererCore, RendererEvent, RendererHost, RendererInitialConfig, RendererOperation, load};

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        next_id: GlyphId,
        reject_glyphs: bool,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.next_id = 0;
        }

        fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, crate::layout::ShapeError> {
            if self.reject_glyphs {
                return Err(crate::layout::ShapeError::unregistered_glyph_id(u32::MAX, glyph_metrics.len()));
            }
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key)
                && glyph_metrics.contains(glyph)
            {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(crate::layout::ShapeError::rejected_metric)?;
            let glyph = glyph_metrics.register(metric)?;
            assert_eq!(glyph, self.next_id, "glyph shaper and document registry diverged");
            self.next_id += 1;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    #[derive(Default)]
    struct TestHost {
        events: RefCell<Vec<RendererEvent>>,
        repaint_requests: Cell<usize>,
        clipboard_text: RefCell<Option<String>>,
    }

    impl RendererHost for TestHost {
        fn request_repaint(&self) {
            self.repaint_requests.set(self.repaint_requests.get() + 1);
        }
        fn request_style(&self) {}
        fn schedule(&self, _delay: Duration, callback: Box<dyn FnOnce() + Send>) {
            callback();
        }
        fn schedule_repaint(&self, _delay: Duration) {}
        fn set_clipboard(&self, text: &str) -> Result<(), String> {
            *self.clipboard_text.borrow_mut() = Some(text.to_owned());
            Ok(())
        }
        fn set_clipboard_image(&self, _width: usize, _height: usize, _rgba: Vec<u8>) -> Result<(), String> {
            Ok(())
        }
        fn emit(&self, event: RendererEvent) {
            self.events.borrow_mut().push(event);
        }
    }

    #[derive(Default)]
    struct CountingProvider {
        documents: HashMap<String, String>,
        read_count: Arc<AtomicUsize>,
    }

    impl CountingProvider {
        fn new(documents: Vec<(&str, &str)>) -> (Arc<Self>, Arc<AtomicUsize>) {
            let read_count = Arc::new(AtomicUsize::new(0));
            let mut mapped = HashMap::new();
            for (uri, body) in documents {
                mapped.insert(uri.to_owned(), body.to_owned());
            }
            (Arc::new(Self { documents: mapped, read_count: read_count.clone() }), read_count)
        }
    }

    impl crate::ResourceProvider for CountingProvider {
        fn read_bytes(&self, uri: &str) -> std::io::Result<Vec<u8>> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            self.documents.get(uri).cloned().map(|value| value.into_bytes()).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, uri.to_owned()))
        }

        fn exists(&self, uri: &str) -> bool {
            self.documents.contains_key(uri)
        }

        fn resolve(&self, _base: &str, href: &str) -> String {
            href.to_owned()
        }

        fn list_html_candidates(&self, _root: &str) -> std::io::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn pagination_core(source: &str, viewport: Size, max_columns: u8) -> (RendererCore, TestShaper) {
        pagination_core_with_policy(source, viewport, max_columns, html::pipeline::TextCompositionPolicy::WebCompatible)
    }

    fn pagination_core_with_policy(source: &str, viewport: Size, max_columns: u8, text_composition_policy: html::pipeline::TextCompositionPolicy) -> (RendererCore, TestShaper) {
        pagination_core_with_policy_at_uri("document.html", source, viewport, max_columns, text_composition_policy)
    }

    fn pagination_core_with_policy_at_uri(uri: &str, source: &str, viewport: Size, max_columns: u8, text_composition_policy: html::pipeline::TextCompositionPolicy) -> (RendererCore, TestShaper) {
        let (provider, _) = CountingProvider::new(vec![(uri, source)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let config = RendererInitialConfig {
            font_size: 16.0,
            column_width: 200.0,
            max_column_count: Some(max_columns),
            image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
            text_composition_policy,
            ..RendererInitialConfig::default()
        };
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec![uri.to_owned()], 0, Some("0:0"), config);
        core.configure_layout_for_viewport(&mut shaper, viewport);
        core.paint_forward(&mut RecordingPainter::default());
        (core, shaper)
    }

    #[test]
    fn default_reader_column_count_is_derived_only_from_available_width() {
        let (provider, _) = CountingProvider::new(vec![("document.html", "<html><body>Wide reader</body></html>")]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let config = RendererInitialConfig { column_width: 200.0, max_column_count: None, ..RendererInitialConfig::default() };
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);

        core.configure_layout_for_viewport(&mut shaper, Size::new(1_100.0, 600.0));

        assert_eq!(core.layout.col_count, 5.0);
    }

    fn line_index_for_character(core: &RendererCore, wanted: char) -> usize {
        let text = core.document.render_view().text();
        let glyph = glyph_index_for_character(core, wanted);
        text.line_index_for_glyph(glyph).expect("fixture character must own a line")
    }

    fn glyph_index_for_character(core: &RendererCore, wanted: char) -> u32 {
        let text = core.document.render_view().text();
        (0..text.glyph_count() as u32).find(|index| text.glyph_at(*index as usize).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == wanted)).expect("fixture character must exist")
    }

    fn visible_positions(core: &RendererCore) -> Vec<(usize, i32, f64)> {
        core.frame.last_line_positions.iter().map(|(line, screen)| (line, screen.col_index, screen.point.y)).collect()
    }

    #[test]
    fn optional_vertical_rhythm_only_stretches_existing_block_gaps() {
        let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0 0 10px;line-height:20px'>B</p><p style='margin:0;line-height:20px'>C</p></body></html>";
        let (mut natural, _) = pagination_core_with_policy(html, Size::new(200.0, 75.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let natural_positions = visible_positions(&natural);
        let a = line_index_for_character(&natural, 'A');
        let b = line_index_for_character(&natural, 'B');

        natural.vertical_rhythm = true;
        natural.paint_forward(&mut RecordingPainter::default());
        let refined_positions = visible_positions(&natural);

        assert_eq!(natural_positions.iter().map(|(line, column, _)| (*line, *column)).collect::<Vec<_>>(), refined_positions.iter().map(|(line, column, _)| (*line, *column)).collect::<Vec<_>>());
        assert_eq!(natural.frame.last_line_positions.get(a).map(|screen| screen.point.y), Some(0.0), "the first line remains anchored");
        assert_eq!(natural.frame.last_line_positions.get(b).map(|screen| screen.point.y), Some(32.0), "the sole safe block gap receives the 2px cap");
        assert_eq!(natural.document.render_view().text().line(b).map(|line| line.point().y), Some(30.0), "continuous CSS layout is untouched");
    }

    #[test]
    fn vertical_rhythm_leaves_the_terminal_document_column_ragged() {
        let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0;line-height:20px'>B</p></body></html>";
        let (mut core, _) = pagination_core_with_policy(html, Size::new(200.0, 75.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let before = visible_positions(&core);

        core.vertical_rhythm = true;
        core.paint_forward(&mut RecordingPainter::default());

        assert_eq!(visible_positions(&core), before);
    }

    #[test]
    fn web_compatible_pagination_ignores_vertical_rhythm() {
        let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0 0 10px;line-height:20px'>B</p><p style='margin:0;line-height:20px'>C</p></body></html>";
        let (mut core, _) = pagination_core(html, Size::new(200.0, 75.0), 1);
        let before = visible_positions(&core);

        core.vertical_rhythm = true;
        core.paint_forward(&mut RecordingPainter::default());

        assert_eq!(visible_positions(&core), before, "WPT/browser-compatible geometry must remain greedy and unadjusted");
    }

    #[test]
    fn book_paginator_keeps_a_compact_list_item_intact() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px'>L<br>M</li></ul></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first = line_index_for_character(&core, 'L');
        let second = line_index_for_character(&core, 'M');

        assert_eq!(core.frame.last_line_positions.get(first).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
        assert_eq!(core.frame.last_line_positions.get(second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    }

    #[test]
    fn book_paginator_keeps_a_term_with_the_first_two_definition_lines() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><dl style='margin:0'><dt style='line-height:20px'>T</dt><dd style='margin:0;line-height:20px'>D<br>E</dd></dl></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let term = line_index_for_character(&core, 'T');
        let definition = line_index_for_character(&core, 'D');
        let definition_second = line_index_for_character(&core, 'E');

        assert_eq!(core.frame.last_line_positions.get(term).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
        assert_eq!(core.frame.last_line_positions.get(definition).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
        assert_eq!(core.frame.last_line_positions.get(definition_second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 40.0)));
    }

    #[test]
    fn book_paginator_keeps_compact_preformatted_code_and_splits_oversized_code() {
        let compact = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD</code></pre></body></html>";
        let (compact_core, _) = pagination_core_with_policy(compact, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let compact_first = line_index_for_character(&compact_core, 'C');
        let compact_second = line_index_for_character(&compact_core, 'D');
        let compact_first_position = compact_core.frame.last_line_positions.get(compact_first).expect("first compact code line");
        let compact_second_position = compact_core.frame.last_line_positions.get(compact_second).expect("second compact code line");
        assert_eq!((compact_first_position.col_index, compact_second_position.col_index), (1, 1));
        assert!(compact_second_position.point.y > compact_first_position.point.y);

        let oversized = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD\nE\nF</code></pre></body></html>";
        let (oversized_core, _) = pagination_core_with_policy(oversized, Size::new(640.0, 50.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
        let oversized_first = line_index_for_character(&oversized_core, 'C');
        let oversized_last = line_index_for_character(&oversized_core, 'F');
        let first_column = oversized_core.frame.last_line_positions.get(oversized_first).map(|screen| screen.col_index).expect("first oversized code line");
        let last_column = oversized_core.frame.last_line_positions.get(oversized_last).map(|screen| screen.col_index).expect("last oversized code line");
        assert!(last_column > first_column, "oversized code must fall back to splitting across columns");
    }

    #[test]
    fn web_paginator_does_not_apply_semantic_keeps() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px'>L<br>M</li></ul></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
        let first = line_index_for_character(&core, 'L');
        let second = line_index_for_character(&core, 'M');

        assert_eq!(core.frame.last_line_positions.get(first).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.frame.last_line_positions.get(second).map(|screen| screen.col_index), Some(1));
    }

    #[test]
    fn paginator_keeps_a_line_that_fits_exactly_on_the_column_bottom() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.current_page_start_line, Some(a));
        assert_eq!(core.frame.current_page_end_line, Some(b));
        assert_eq!(core.frame.last_line_positions.get(a).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| screen.col_index), Some(0));
        assert!(core.frame.last_line_positions.get(c).is_none(), "the first line beyond the exact bottom edge belongs to the next page");
    }

    #[test]
    fn paginator_moves_an_overflowing_line_whole_to_the_next_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 39.0), 2);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let first = core.frame.last_line_positions.get(a).expect("first line visible");
        let second = core.frame.last_line_positions.get(b).expect("overflow line visible in the following column");

        assert_eq!(first.col_index, 0);
        assert_eq!(second.col_index, 1);
        assert_eq!(second.point.y, 0.0, "an overflowing line must restart at the column top");
    }

    #[test]
    fn paginator_counts_document_vertical_gaps_when_fitting_a_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='height:10px'></div><div style='line-height:20px'>B</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let first = core.frame.last_line_positions.get(a).expect("first line visible");
        let second = core.frame.last_line_positions.get(b).expect("second line visible");

        assert!(core.document.render_view().text().line(b).expect("second source line").point().y > core.document.render_view().text().line(a).expect("first source line").point().y + 20.0);
        assert_eq!((first.col_index, second.col_index), (0, 1));
        assert_eq!(second.point.y, 0.0);
    }

    #[test]
    fn paginator_uses_tall_inline_image_height_for_column_fitting() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A<img src='missing.png' style='width:10px;height:35px'></div><div style='line-height:20px'>B</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let images = core.document.render_view().fragments().images_for_line(a);

        assert!(images.iter().any(|image| image.size().height == 35.0), "fixture must retain the 35px replaced-content height");
        assert_eq!(core.frame.last_line_positions.get(a).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| screen.col_index), Some(1));
    }

    #[test]
    fn paginator_places_an_oversized_line_once_and_advances_to_the_next_page() {
        let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div></body></html>";
        let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.current_page_start_line, Some(a));
        assert_eq!(core.frame.current_page_end_line, Some(a));
        assert_eq!(core.frame.last_line_positions.get(a).map(|screen| screen.point.y), Some(0.0));
        assert!(core.frame.last_line_positions.get(b).is_none());

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());

        assert_eq!(core.frame.current_page_start_line, Some(b));
        assert!(core.frame.last_line_positions.get(b).is_some(), "pagination must make progress beyond the oversized line");
    }

    #[test]
    fn paginator_roundtrips_backward_across_an_oversized_line() {
        let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></body></html>";
        let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn paginator_moves_an_oversized_image_line_to_a_fresh_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div><img src='missing.png' style='width:10px;height:60px;vertical-align:top'>B</div><div style='line-height:20px'>C</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.last_line_positions.get(a).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
        assert!(core.frame.last_line_positions.get(c).is_none(), "the oversized image consumes its destination column");
    }

    #[test]
    fn paginator_honors_forced_modern_and_legacy_css_breaks() {
        for property in ["break-before:page", "page-break-before:always"] {
            let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>");
            let (core, _) = pagination_core(&html, Size::new(200.0, 40.0), 1);
            let a = line_index_for_character(&core, 'A');
            let b = line_index_for_character(&core, 'B');

            assert!(core.frame.last_line_positions.get(a).is_some());
            assert!(core.frame.last_line_positions.get(b).is_none(), "{property} must start B in the next fragmentainer");
            assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 40.0);
        }
    }

    #[test]
    fn book_paginator_distinguishes_column_breaks_from_page_breaks() {
        let column_html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;break-before:column'>B</div></body></html>";
        let (column_core, _) = pagination_core_with_policy(column_html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
        let column_b = line_index_for_character(&column_core, 'B');
        assert_eq!(column_core.frame.last_line_positions.get(column_b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));

        for property in ["break-before:page", "page-break-before:always", "break-before:left", "break-before:right"] {
            let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>");
            let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
            let b = line_index_for_character(&core, 'B');

            assert!(core.frame.last_line_positions.get(b).is_none(), "{property} must skip the remaining reader columns");
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "{property} must begin at the next reader page origin");
        }
    }

    #[test]
    fn book_paginator_anchors_forced_breaks_on_empty_boxes_to_following_content() {
        for property in ["break-before:column", "break-after:column"] {
            let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='{property}'></div><div style='line-height:20px'>B</div></body></html>");
            let (core, _) = pagination_core_with_policy(&html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
            let a = line_index_for_character(&core, 'A');
            let b = line_index_for_character(&core, 'B');

            assert_eq!(core.frame.last_line_positions.get(a).map(|screen| screen.col_index), Some(0));
            assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{property} on an empty box must anchor to B");
        }
    }

    #[test]
    fn forced_break_inside_an_avoided_container_takes_precedence() {
        let html =
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "avoid must not move content preceding an internal forced break");
        assert_eq!(core.frame.last_line_positions.get(c).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    }

    #[test]
    fn forced_column_break_does_not_leave_a_margin_only_empty_column() {
        let html = "<html><body style='margin:0'><div style='height:90px;margin-bottom:20px;line-height:20px'>A</div><div style='break-before:column;line-height:20px'>B</div></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(700.0, 100.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');

        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 110.0, "continuous layout must retain the ordinary block margin");
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    }

    #[test]
    fn book_paginator_applies_break_after_without_fragmenting_continuous_layout() {
        for (property, first_page_has_b) in [("break-after:column", true), ("break-after:page", false), ("page-break-after:always", false)] {
            let html = format!("<html><body style='margin:0'><div style='height:30px;margin-bottom:10px;line-height:20px;{property}'>A</div><div style='line-height:20px'>B</div></body></html>");
            let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
            let b = line_index_for_character(&core, 'B');

            assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 40.0, "forced pagination must not rewrite continuous document geometry");
            if first_page_has_b {
                assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
            } else {
                assert!(core.frame.last_line_positions.get(b).is_none(), "{property} must skip the remaining columns");
                core.next_page(&mut shaper);
                core.paint_forward(&mut RecordingPainter::default());
                assert_eq!(core.frame.last_line_positions.get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
            }
        }
    }

    #[test]
    fn paginator_forced_break_wins_over_an_adjoining_avoid() {
        let html = "<html><body style='margin:0'><div style='line-height:20px;break-after:page'>A</div><div style='line-height:20px;break-before:avoid'>B</div></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert!(core.frame.last_line_positions.get(a).is_some());
        assert!(core.frame.last_line_positions.get(b).is_none());
        assert_eq!(core.document.render_view().text().line(a).expect("A line").point().y, 0.0);
        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 40.0);
    }

    #[test]
    fn paginator_keeps_a_break_inside_avoid_block_together() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(a).is_some());
        assert!(core.frame.last_line_positions.get(b).is_none());
        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 40.0);
        assert_eq!(core.document.render_view().text().line(c).expect("C line").point().y, 60.0);
    }

    #[test]
    fn book_paginator_applies_break_inside_avoid_without_changing_continuous_geometry() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 40.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(c).expect("C line").point().y, 40.0);
        assert!(core.frame.last_line_positions.get(b).is_none());
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(c).map(|screen| screen.point.y), Some(20.0));
    }

    #[test]
    fn paginator_honors_break_after_avoid_as_keep_with_next() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:avoid'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let heading = line_index_for_character(&core, 'H');
        let b = line_index_for_character(&core, 'B');

        assert!(core.frame.last_line_positions.get(a).is_some());
        assert!(core.frame.last_line_positions.get(heading).is_none());
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 40.0);
        assert_eq!(core.document.render_view().text().line(b).expect("following line").point().y, 60.0);
    }

    #[test]
    fn book_paginator_automatically_keeps_a_heading_with_two_following_lines() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D<br>E</p></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let a = line_index_for_character(&core, 'A');
        let heading = line_index_for_character(&core, 'H');
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(a).is_some());
        assert!(core.frame.last_line_positions.get(heading).is_none(), "the heading must move instead of becoming the last line of the page");
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(b).expect("first paragraph line").point().y, 40.0);
        assert_eq!(core.document.render_view().text().line(c).expect("second paragraph line").point().y, 60.0);
    }

    #[test]
    fn web_paginator_leaves_automatic_heading_keep_disabled() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let heading = line_index_for_character(&core, 'H');

        assert!(core.frame.last_line_positions.get(heading).is_some(), "web-compatible pagination must retain browser-style heading placement");
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
    }

    #[test]
    fn book_paginator_moves_a_large_multiline_heading_to_a_fresh_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H<br>I</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 80.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let heading = line_index_for_character(&core, 'H');
        let second_heading_line = line_index_for_character(&core, 'I');

        assert!(core.frame.last_line_positions.get(heading).is_none());
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(second_heading_line).expect("second heading line").point().y, 40.0);
    }

    #[test]
    fn book_paginator_does_not_move_an_oversized_heading_group() {
        let html =
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;widows:1;orphans:1'>H<br>I<br>J</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let heading = line_index_for_character(&core, 'H');

        assert!(core.frame.last_line_positions.get(heading).is_some(), "a group taller than a full column must use best-effort pagination");
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
    }

    #[test]
    fn authored_forced_break_after_heading_overrides_automatic_keep() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:page'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let heading = line_index_for_character(&core, 'H');
        let b = line_index_for_character(&core, 'B');

        assert!(core.frame.last_line_positions.get(heading).is_some(), "the authored forced break must leave the heading before the break");
        assert!(core.frame.last_line_positions.get(b).is_none(), "the following paragraph must begin on the next reader page");
        assert_eq!(core.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(b).expect("following line").point().y, 40.0, "forced pagination must not insert a gap into continuous layout");
    }

    #[test]
    fn automatic_heading_keep_roundtrips_through_backward_pagination() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D</p></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn book_paginator_keeps_compact_semantic_sidebars_in_one_column() {
        for (uri, semantic, html) in [
            ("document.html", "aside", "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>"),
            (
                "document.html",
                "ARIA complementary",
                "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='complementary' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></section></body></html>",
            ),
            (
                "document.xhtml",
                "EPUB sidebar",
                "<html xmlns='http://www.w3.org/1999/xhtml' xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='sidebar' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></section></body></html>",
            ),
        ] {
            let (core, _) = pagination_core_with_policy_at_uri(uri, html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
            let sidebar = line_index_for_character(&core, 'S');
            let sidebar_end = line_index_for_character(&core, 'T');

            assert_eq!(core.frame.last_line_positions.get(sidebar).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{semantic} must move as one compact block");
            assert_eq!(core.frame.last_line_positions.get(sidebar_end).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
        }
    }

    #[test]
    fn book_paginator_keeps_bounded_compact_semantic_structures_in_one_column() {
        for (uri, semantic, attribute) in [
            ("document.xhtml", "EPUB dedication", "epub:type='dedication'"),
            ("document.xhtml", "EPUB abstract", "epub:type='abstract'"),
            ("document.xhtml", "EPUB notice", "epub:type='notice'"),
            ("document.xhtml", "EPUB pullquote", "epub:type='pullquote'"),
            ("document.xhtml", "prefixed EPUB theorem", "epub:type='math:theorem'"),
            ("document.xhtml", "EPUB proof", "epub:type='proof'"),
            ("document.xhtml", "EPUB example", "epub:type='example'"),
            ("document.xhtml", "EPUB exercise", "epub:type='exercise'"),
            ("document.html", "ARIA abstract", "role='doc-abstract'"),
            ("document.html", "ARIA example", "role='doc-example'"),
        ] {
            let html = format!(
                "<html xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section {attribute} style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>"
            );
            let (core, _) = pagination_core_with_policy_at_uri(uri, &html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
            let start = line_index_for_character(&core, 'B');
            let end = line_index_for_character(&core, 'C');

            assert_eq!(core.frame.last_line_positions.get(start).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{semantic} must move as one compact structure");
            assert_eq!(core.frame.last_line_positions.get(end).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)), "{semantic} must remain intact");
        }
    }

    #[test]
    fn book_paginator_splits_an_oversized_semantic_structure_normally() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='theorem' style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let start = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(start).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "an oversized semantic structure must use available space instead of moving as one unit");
    }

    #[test]
    fn authored_break_inside_a_compact_semantic_structure_wins() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='abstract' style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let start = line_index_for_character(&core, 'B');
        let after_break = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.last_line_positions.get(start).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
        assert_eq!(core.frame.last_line_positions.get(after_break).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    }

    #[test]
    fn book_paginator_keeps_a_semantic_statement_label_with_two_content_lines() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>X</div><section epub:type='theorem' style='margin:0'><p epub:type='label' style='margin:0;line-height:20px'>L</p><p style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</p></section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let label = line_index_for_character(&core, 'L');
        let first = line_index_for_character(&core, 'B');
        let second = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.last_line_positions.get(label).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
        assert_eq!(core.frame.last_line_positions.get(first).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
        assert_eq!(core.frame.last_line_positions.get(second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 40.0)));
    }

    #[test]
    fn web_paginator_does_not_add_semantic_sidebar_placement() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside epub:type='sidebar' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
        let sidebar = line_index_for_character(&core, 'S');

        assert_eq!(core.frame.last_line_positions.get(sidebar).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
    }

    #[test]
    fn book_paginator_starts_major_epub_sections_on_a_fresh_reader_page() {
        for semantic in [
            "chapter",
            "part",
            "appendix",
            "prologue",
            "epilogue",
            "foreword",
            "preface",
            "introduction",
            "conclusion",
            "acknowledgments",
            "afterword",
            "bibliography",
            "glossary",
            "index",
            "colophon",
            "titlepage",
            "halftitlepage",
            "copyright-page",
            "frontmatter",
            "bodymatter",
            "backmatter",
        ] {
            let html = format!(
                "<html xmlns='http://www.w3.org/1999/xhtml' xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='landmarks {semantic}' style='margin:0;line-height:20px'>B</section></body></html>"
            );
            let (mut core, mut shaper) = pagination_core_with_policy_at_uri("document.xhtml", &html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
            let section = line_index_for_character(&core, 'B');

            assert!(core.frame.last_line_positions.get(section).is_none(), "EPUB {semantic} must skip all remaining columns on the current reader page");
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "EPUB {semantic} must start at the first column");
        }
    }

    #[test]
    fn book_paginator_starts_major_dpub_sections_on_a_fresh_reader_page() {
        for semantic in [
            "doc-chapter",
            "doc-part",
            "doc-appendix",
            "doc-prologue",
            "doc-epilogue",
            "doc-foreword",
            "doc-preface",
            "doc-introduction",
            "doc-conclusion",
            "doc-acknowledgments",
            "doc-afterword",
            "doc-bibliography",
            "doc-glossary",
            "doc-index",
            "doc-colophon",
        ] {
            let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='region {semantic}' style='margin:0;line-height:20px'>B</section></body></html>");
            let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
            let section = line_index_for_character(&core, 'B');

            assert!(core.frame.last_line_positions.get(section).is_none(), "ARIA {semantic} must skip all remaining columns on the current reader page");
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "ARIA {semantic} must start at the first column");
        }
    }

    #[test]
    fn generic_section_remains_in_normal_book_flow() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='margin:0;line-height:20px'>B</section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let section = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
    }

    #[test]
    fn authored_avoid_suppresses_a_semantic_page_start() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px;break-before:avoid'>B</section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let section = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
    }

    #[test]
    fn semantic_page_start_at_document_origin_does_not_create_a_blank_page() {
        let html = "<html><body style='margin:0'><section epub:type='titlepage' style='margin:0;line-height:20px'>B</section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let section = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
    }

    #[test]
    fn web_paginator_does_not_add_expanded_semantic_section_breaks() {
        for (semantic, html) in [
            ("EPUB appendix", "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px'>B</section></body></html>"),
            ("ARIA bibliography", "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='doc-bibliography' style='margin:0;line-height:20px'>B</section></body></html>"),
        ] {
            let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
            let section = line_index_for_character(&core, 'B');

            assert_eq!(core.frame.last_line_positions.get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "{semantic} must remain ordinary flow in web mode");
        }
    }

    #[test]
    fn authored_column_break_overrides_a_semantic_chapter_page_break() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px;break-before:column'>B</section></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let chapter = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(chapter).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    }

    #[test]
    fn web_paginator_does_not_add_semantic_chapter_breaks() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px'>B</section></body></html>";
        let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
        let chapter = line_index_for_character(&core, 'B');

        assert_eq!(core.frame.last_line_positions.get(chapter).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
    }

    #[test]
    fn book_paginator_moves_a_compact_figure_and_caption_to_a_fresh_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let image_label = line_index_for_character(&core, 'I');
        let caption = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(image_label).is_none(), "the compact figure must not be split across columns");
        assert_eq!(core.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 40.0);
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(image_label).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(caption).map(|screen| screen.point.y), Some(20.0));
    }

    #[test]
    fn book_paginator_keeps_an_image_only_figure_whole() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'></figure></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 30.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let image_line = core.document.render_view().fragments().images().iter().next().expect("fixture image fragment").line_idx();

        assert!(core.frame.last_line_positions.get(image_line).is_none(), "the image-only figure must move intact");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(image_line).map(|screen| screen.point.y), Some(0.0));
    }

    #[test]
    fn book_paginator_keeps_a_replaced_image_with_its_multiline_caption() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'><figcaption style='line-height:20px'>C<br>D</figcaption></figure></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let image_line = core.document.render_view().fragments().images().iter().next().expect("fixture image fragment").line_idx();
        let caption_first = line_index_for_character(&core, 'C');
        let caption_second = line_index_for_character(&core, 'D');

        assert!(core.frame.last_line_positions.get(image_line).is_none(), "the image must move instead of leaving its caption behind");
        assert!(core.frame.last_line_positions.get(caption_first).is_none());
        assert!(core.frame.last_line_positions.get(caption_second).is_none());

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(image_line).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(caption_first).map(|screen| screen.point.y), Some(20.0));
        assert_eq!(core.frame.last_line_positions.get(caption_second).map(|screen| screen.point.y), Some(40.0));
    }

    #[test]
    fn web_paginator_leaves_automatic_figure_placement_disabled() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 50.0), 1);
        let image_label = line_index_for_character(&core, 'I');

        assert!(core.frame.last_line_positions.get(image_label).is_some(), "web-compatible pagination must preserve normal flow placement");
        assert_eq!(core.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
    }

    #[test]
    fn book_paginator_keeps_a_bottom_caption_with_oversized_figure_content() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let image_label = line_index_for_character(&core, 'I');
        let caption = line_index_for_character(&core, 'C');

        assert_eq!(core.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 40.0);
        assert!(core.frame.last_line_positions.get(image_label).is_none());
        assert!(core.frame.last_line_positions.get(caption).is_none());
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(image_label).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(caption).map(|screen| screen.point.y), Some(20.0));
    }

    #[test]
    fn book_paginator_keeps_a_top_caption_with_oversized_figure_content() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><figcaption style='line-height:20px'>C</figcaption><div style='line-height:20px'>I</div><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let caption = line_index_for_character(&core, 'C');
        let image_label = line_index_for_character(&core, 'I');

        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(image_label).expect("figure content line").point().y, 40.0);
        assert!(core.frame.last_line_positions.get(caption).is_none());
        assert!(core.frame.last_line_positions.get(image_label).is_none());
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(caption).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(image_label).map(|screen| screen.point.y), Some(20.0));
    }

    #[test]
    fn authored_forced_break_between_figure_content_and_caption_wins() {
        let html = "<html><body style='margin:0'><figure style='margin:0'><div style='line-height:20px;break-after:page'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let image_label = line_index_for_character(&core, 'I');
        let caption = line_index_for_character(&core, 'C');

        assert_eq!(core.document.render_view().text().line(image_label).expect("figure content line").point().y, 0.0);
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "forced pagination must not insert a gap into continuous layout");
        assert!(core.frame.last_line_positions.get(caption).is_none(), "the caption must begin on the next reader page");
    }

    #[test]
    fn automatic_figure_placement_roundtrips_through_backward_pagination() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure><div style='line-height:20px'>D</div></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn book_paginator_prefers_a_compact_table_in_one_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let caption = line_index_for_character(&core, 'C');
        let first_row = line_index_for_character(&core, 'B');
        let second_row = line_index_for_character(&core, 'D');

        assert!(core.frame.last_line_positions.get(caption).is_none(), "the table caption must move with the compact table");
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "layout geometry remains continuous");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(caption).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(first_row).map(|screen| screen.point.y), Some(20.0));
        assert_eq!(core.frame.last_line_positions.get(second_row).map(|screen| screen.point.y), Some(40.0));
    }

    #[test]
    fn web_paginator_leaves_automatic_table_placement_disabled() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 70.0), 1);
        let caption = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(caption).is_some());
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 20.0);
    }

    #[test]
    fn book_paginator_uses_remaining_space_for_a_table_taller_than_a_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let caption = line_index_for_character(&core, 'C');
        let first_row = line_index_for_character(&core, 'B');
        let second_row = line_index_for_character(&core, 'D');

        assert_eq!(core.frame.last_line_positions.get(caption).map(|screen| screen.point.y), Some(20.0), "an oversized table should use the remainder after preceding content");
        assert_eq!(core.frame.last_line_positions.get(first_row).map(|screen| screen.point.y), Some(40.0));
        assert!(core.frame.last_line_positions.get(second_row).is_none(), "the next complete row should move when it no longer fits");
        assert_eq!(core.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "layout geometry remains continuous");
    }

    #[test]
    fn book_paginator_breaks_an_oversized_table_between_rows() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_row = line_index_for_character(&core, 'B');
        let first_row_second_line = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(first_row).is_none());
        assert_eq!(core.document.render_view().text().line(first_row).expect("first row line").point().y, 20.0, "layout geometry remains continuous");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(first_row).map(|screen| screen.point.y), Some(0.0));
        let first_y = core.frame.last_line_positions.get(first_row).expect("first row line is visible").point.y;
        let second_y = core.frame.last_line_positions.get(first_row_second_line).expect("the paginator must not split a row").point.y;
        assert!(second_y > first_y);
    }

    #[test]
    fn book_paginator_repeats_explicit_table_header_on_a_continuation_column() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></tbody></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 70.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let continued_row = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.repeated_table_headers.len(), 1);
        assert_eq!(core.frame.repeated_table_headers[0].col_index, 1);
        assert_eq!(core.frame.last_line_positions.get(continued_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    }

    #[test]
    fn book_paginator_repeats_a_leading_all_th_row_without_thead() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><th style='padding:0;line-height:20px'>H</th></tr><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 70.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let continued_row = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.repeated_table_headers.len(), 1);
        assert_eq!(core.frame.last_line_positions.get(continued_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    }

    #[test]
    fn book_paginator_omits_a_repeated_header_when_no_body_row_would_fit() {
        let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H<br>I</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr></tbody></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let body_row = line_index_for_character(&core, 'B');

        assert!(core.frame.repeated_table_headers.is_empty());
        assert_eq!(core.frame.last_line_positions.get(body_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    }

    #[test]
    fn table_row_boundary_does_not_move_the_previous_rows_last_line() {
        let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>A<br>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first = line_index_for_character(&core, 'A');
        let first_last = line_index_for_character(&core, 'B');
        let second = line_index_for_character(&core, 'C');

        assert_eq!(core.frame.last_line_positions.get(first).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.frame.last_line_positions.get(first_last).map(|screen| screen.col_index), Some(0), "a line ending exactly at the next row's top still belongs to the previous row");
        assert_eq!(core.frame.last_line_positions.get(second).map(|screen| screen.col_index), Some(1));
    }

    #[test]
    fn book_paginator_does_not_break_through_a_rowspan_group() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>R</td><td style='padding:0;line-height:20px'>Q</td></tr><tr><td rowspan='2' style='padding:0;line-height:20px'>S</td><td style='padding:0;line-height:20px'>T</td></tr><tr><td style='padding:0;line-height:20px'>U</td></tr></table></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_group_row = line_index_for_character(&core, 'T');
        let second_group_row = line_index_for_character(&core, 'U');

        for _ in 0..4 {
            if core.frame.last_line_positions.get(first_group_row).is_some() {
                break;
            }
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
        }
        assert!(core.frame.last_line_positions.get(first_group_row).is_some(), "the rowspan group must eventually be visible");
        assert!(core.frame.last_line_positions.get(second_group_row).is_some(), "rows connected by a rowspan must stay on the same page");
    }

    #[test]
    fn book_paginator_keeps_a_bottom_table_caption_with_the_last_row() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><caption style='caption-side:bottom;line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
        let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let last_row = line_index_for_character(&core, 'B');
        let caption = line_index_for_character(&core, 'C');

        let row_y = core.document.render_view().text().line(last_row).expect("last row line").point().y;
        let caption_line = core.document.render_view().text().line(caption).expect("bottom caption line");
        assert_eq!(row_y, 40.0, "layout geometry remains continuous");
        let (group_top, group_bottom) = core.paginator().table_row_group_bounds_containing(row_y).expect("the final row owns a pagination group");
        assert_eq!(group_top, row_y);
        assert!(group_bottom >= caption_line.point().y + caption_line.height(), "the final row's pagination group must include its bottom caption");
    }

    #[test]
    fn authored_forced_break_between_table_rows_wins() {
        let html =
            "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr style='break-after:page'><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_row = line_index_for_character(&core, 'A');
        let second_row = line_index_for_character(&core, 'B');

        assert_eq!(core.document.render_view().text().line(first_row).expect("first row line").point().y, 0.0);
        assert_eq!(core.document.render_view().text().line(second_row).expect("second row line").point().y, 20.0, "forced pagination does not alter layout geometry");
        assert!(core.frame.last_line_positions.get(second_row).is_none());
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert!(core.frame.repeated_table_headers.is_empty(), "ordinary td rows must not be inferred as table headers");
        assert_eq!(core.frame.last_line_positions.get(second_row).map(|screen| screen.point.y), Some(0.0));
    }

    #[test]
    fn automatic_table_placement_roundtrips_through_backward_pagination() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn paginator_applies_default_widow_and_orphan_limits() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(a).is_some());
        assert!(core.frame.last_line_positions.get(b).is_none(), "a 1/1 paragraph split violates the default 2-line limits");
        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 40.0);
        assert_eq!(core.document.render_view().text().line(c).expect("C line").point().y, 60.0);
    }

    #[test]
    fn book_paginator_applies_line_limits_without_changing_continuous_geometry() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
        let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 40.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(c).expect("C line").point().y, 40.0);
        assert!(core.frame.last_line_positions.get(b).is_none());
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.frame.last_line_positions.get(b).map(|screen| screen.point.y), Some(0.0));
        assert_eq!(core.frame.last_line_positions.get(c).map(|screen| screen.point.y), Some(20.0));
    }

    #[test]
    fn paginator_honors_authored_widow_and_orphan_limits() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let b = line_index_for_character(&core, 'B');
        let c = line_index_for_character(&core, 'C');

        assert!(core.frame.last_line_positions.get(b).is_some(), "an authored one-line orphan allowance should permit B at the column bottom");
        assert!(core.frame.last_line_positions.get(c).is_none());
        assert_eq!(core.document.render_view().text().line(b).expect("B line").point().y, 20.0);
        assert_eq!(core.document.render_view().text().line(c).expect("C line").point().y, 40.0);
    }

    #[test]
    fn paginator_roundtrips_backward_across_a_semantic_break() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
        let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn paginator_keeps_table_cell_lines_with_equal_document_y_together() {
        let html = "<html><body style='margin:0'><table style='border-spacing:0'><tr><td style='padding:0;line-height:20px'>A</td><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 20.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');
        let source_a = core.document.render_view().text().line(a).expect("first cell line").point().y;
        let source_b = core.document.render_view().text().line(b).expect("second cell line").point().y;
        let screen_a = core.frame.last_line_positions.get(a).expect("first cell visible");
        let screen_b = core.frame.last_line_positions.get(b).expect("second cell visible");

        assert_eq!(source_a, source_b, "table cell lines must exercise the equal-document-y path");
        assert_eq!(screen_a.col_index, screen_b.col_index);
        assert_eq!(screen_a.point.y, screen_b.point.y);
    }

    #[test]
    fn text_hit_testing_distinguishes_adjacent_table_cells_at_the_same_y() {
        let html =
            "<html><body style='margin:0'><table style='border-spacing:0;table-layout:fixed;width:200px'><tr><td style='padding:0;line-height:20px'>LEFT</td><td style='padding:0;line-height:20px'>RIGHT</td></tr></table></body></html>";
        let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let left_glyph = glyph_index_for_character(&core, 'L');
        let right_glyph = glyph_index_for_character(&core, 'R');
        let left_line_idx = core.document.render_view().text().line_index_for_glyph(left_glyph).expect("left cell line");
        let right_line_idx = core.document.render_view().text().line_index_for_glyph(right_glyph).expect("right cell line");
        let left_line = core.document.render_view().text().line(left_line_idx).expect("left cell geometry");
        let right_line = core.document.render_view().text().line(right_line_idx).expect("right cell geometry");
        let left_screen = core.frame.last_line_positions.get(left_line_idx).expect("left cell visible");
        let right_screen = core.frame.last_line_positions.get(right_line_idx).expect("right cell visible");
        let left_point = Point::new(left_screen.point.x + core.text_geometry().glyph_x_in_line(&left_line, left_glyph) + 0.1, left_screen.point.y + left_line.height() / 2.0);
        let right_point = Point::new(right_screen.point.x + core.text_geometry().glyph_x_in_line(&right_line, right_glyph) + 0.1, right_screen.point.y + right_line.height() / 2.0);

        assert_eq!(left_screen.point.y, right_screen.point.y, "fixture must exercise vertically overlapping cell lines");
        assert_eq!(core.hit_test_glyph(left_point), Some(left_glyph));
        assert_eq!(core.hit_test_glyph(right_point), Some(right_glyph));
    }

    #[test]
    fn semantic_selection_promotes_table_cells_and_embeds_a_markdown_table() {
        let html = "<html><body style='margin:0'><p>Before</p><table><tr><th>Head A</th><th>Head B</th></tr><tr><td>One</td><td>Two</td></tr></table><p>After</p></body></html>";
        let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
        let glyph_count = core.document.render_view().text().glyph_count() as u32;

        core.selection.selection_anchor = Some(0);
        core.selection.selection_active = Some(glyph_count);
        core.selection.semantic_selection = true;
        core.update_selection_text();

        let markdown = core.selection.selected_text_markdown.as_deref().expect("semantic Markdown selection");
        assert!(markdown.contains("Before"));
        assert!(markdown.contains("| Head A | Head B |\n| --- | --- |\n| One | Two |"));
        assert!(markdown.contains("After"));
        assert_eq!(core.selection_view().semantic_cell_glyph_ranges(0, glyph_count).len(), 4);

        let one = glyph_index_for_character(&core, 'O');
        core.selection.selection_anchor = Some(one);
        core.selection.selection_active = Some(one + 1);
        core.update_selection_text();

        assert_eq!(core.selection.selected_text.as_deref(), Some("O"), "annotation text remains tied to the actual glyph range");
        assert_eq!(core.selection.selected_text_markdown.as_deref(), Some("|  |\n| --- |\n| One |"));
        assert_eq!(core.selection_view().semantic_cell_glyph_ranges(one, one + 1).len(), 1);
    }

    #[test]
    fn semantic_selection_promotes_complete_nested_list_items() {
        let html = "<html><body style='margin:0'><p>Before</p><ol start='5'><li>Alpha<ul><li>Beta</li></ul></li><li>Gamma</li></ol><p>After</p></body></html>";
        let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
        let alpha = glyph_index_for_character(&core, 'p');

        core.selection.selection_anchor = Some(alpha);
        core.selection.selection_active = Some(alpha + 1);
        core.selection.semantic_selection = true;
        core.update_selection_text();

        assert_eq!(core.selection.selected_text.as_deref(), Some("p"), "annotation text remains tied to the actual glyph range");
        assert_eq!(core.selection.selected_text_markdown.as_deref(), Some("5. Alpha\n    - Beta"));
        let promoted = core.selection_view().semantic_promoted_glyph_ranges(alpha, alpha + 1);
        assert!(promoted.len() >= 2, "the complete parent and nested item text should be selected");

        let beta = glyph_index_for_character(&core, 't');
        core.selection.selection_anchor = Some(beta);
        core.selection.selection_active = Some(beta + 1);
        core.update_selection_text();
        assert_eq!(core.selection.selected_text_markdown.as_deref(), Some("- Beta"), "a nested item selected alone should remain a portable standalone list item");
    }

    #[test]
    fn semantic_selection_promotes_code_blocks_only_when_crossing_their_boundary() {
        let html = "<html><body style='margin:0'><p>Before</p><pre><code class='language-rust'>let x = ```;\nnext();</code></pre><p>After</p></body></html>";
        let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
        let selected = glyph_index_for_character(&core, 'x');

        core.selection.selection_anchor = Some(selected);
        core.selection.selection_active = Some(selected + 1);
        core.selection.semantic_selection = true;
        core.update_selection_text();

        assert_eq!(core.selection.selected_text.as_deref(), Some("x"));
        assert_eq!(core.selection.selected_text_markdown.as_deref(), Some("`x`"));
        assert!(core.selection_view().semantic_promoted_glyph_ranges(selected, selected + 1).is_empty(), "a selection contained by the code block must remain granular");

        let before = glyph_index_for_character(&core, 'B');
        core.selection.selection_anchor = Some(before);
        core.selection.selection_active = Some(selected + 1);
        core.update_selection_text();

        let markdown = core.selection.selected_text_markdown.as_deref().expect("cross-boundary Markdown selection");
        assert!(markdown.contains("Before"));
        assert!(markdown.contains("````rust\nlet x = ```;\nnext();\n````"));
        assert!(!core.selection_view().semantic_promoted_glyph_ranges(before, selected + 1).is_empty(), "crossing into the code block must promote it completely");
    }

    #[test]
    fn paginator_forward_then_uncached_backward_restores_the_same_page_geometry() {
        let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div><div style='line-height:20px'>D</div><div style='line-height:20px'>E</div></body></html>";
        let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
        let first_page = visible_positions(&core);
        let first_range = (core.frame.current_page_start_line, core.frame.current_page_end_line);

        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_ne!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range, "forward navigation must reach a different page");

        core.prepared_pages.clear();
        core.prev_page(&mut shaper);
        assert!(matches!(core.viewport.direction, crate::PaintDirection::Backward));
        core.prepare_backward();

        assert_eq!((core.frame.current_page_start_line, core.frame.current_page_end_line), first_range);
        assert_eq!(visible_positions(&core), first_page);
    }

    #[test]
    fn core_loads_and_navigates_between_documents_without_a_window_backend() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-core-nav-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let first = fixture.join("one.html");
        let second = fixture.join("two.html");
        std::fs::write(&first, "<html><body><a href=\"two.html#target\">Next</a></body></html>").unwrap();
        std::fs::write(&second, "<html><body><h1 id=\"target\">Target</h1></body></html>").unwrap();

        let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
        let uris = vec![first.to_string_lossy().into_owned(), second.to_string_lossy().into_owned()];
        let mut shaper = TestShaper::default();
        let host = Rc::new(TestHost::default());
        let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

        core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
        core.paint_forward(&mut RecordingPainter::default());
        let view = core.document.render_view();
        let text = view.text();
        let addressing = view.addressing();
        let linked_glyph = (0..text.glyph_count() as u32).find(|glyph| addressing.link_for_glyph(*glyph).is_some()).expect("fixture should contain a linked glyph");
        let line_idx = core.doc().find_line_for_glyph(linked_glyph).expect("linked glyph should be laid out");
        let screen = core.frame.last_line_positions.get(line_idx).expect("linked line should be painted");
        let line = core.document.render_view().text().line(line_idx).expect("linked line should be present");
        let position = Point::new(screen.point.x + core.text_geometry().glyph_x_in_line(&line, linked_glyph) + 0.1, screen.point.y + line.height() / 2.0);
        assert!(core.link_at(position));
        assert!(core.begin_selection_at(position, false));
        assert_eq!(core.finish_selection(), Some(linked_glyph));

        assert!(core.handle_link_click(&mut shaper, linked_glyph));
        assert_eq!(core.nav.current_doc_index, 1);
        assert!(core.nav.current_anchor_id(core.nav_view()).as_deref() == Some("target") || core.nav.nav_anchor_glyph.is_some());
        assert!(core.navigate_history_back(&mut shaper));
        assert_eq!(core.nav.current_doc_index, 0);
        assert!(core.navigate_history_forward(&mut shaper));
        assert_eq!(core.nav.current_doc_index, 1);
        assert!(host.events.borrow().iter().any(|event| matches!(event, RendererEvent::TitleChanged(_))));

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn commands_preserve_search_selection_and_settings_without_a_window_backend() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-core-commands-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document_path = fixture.join("index.html");
        std::fs::write(&document_path, "<html><head><title>Commands</title></head><body><p id=\"spoken\"><strong>Needle</strong> and needle.</p></body></html>").unwrap();

        let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
        let uris = vec![document_path.to_string_lossy().into_owned()];
        let mut shaper = TestShaper::default();
        let loaded = load::load_document(provider.clone(), &uris[0], 16.0, 600.0, &mut shaper);
        let host = Rc::new(TestHost::default());
        let mut core = RendererCore::new(host.clone(), loaded, provider, uris, 0, RendererInitialConfig::default());
        assert_eq!(core.pipeline_inputs.layout.image_sizing_policy, html::pipeline::ImageSizingPolicy::SmartStandalone, "reader UI documents should enable smart standalone image sizing at initial layout");

        core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
        core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
        core.apply(&mut shaper, crate::RendererCommand::SetScale(1.5));
        assert_eq!(core.layout.base_col_width, 480.0);
        assert_eq!(core.root_font_size, 18.0);
        assert_eq!(core.layout.scale, 1.5);
        let reader_overrides_before_theme = core.pipeline_inputs.reader_overrides.clone();
        let repaint_before_theme = host.repaint_requests.get();
        core.apply(&mut shaper, crate::RendererCommand::SetReaderPaintPalette(crate::ReaderPaintPalette { foreground: Some(0xe7e2d8ff), background: Some(0x171916ff) }));
        assert_eq!(core.pipeline_inputs.reader_overrides, reader_overrides_before_theme, "theme changes must not rerun the style pipeline");
        assert!(host.repaint_requests.get() > repaint_before_theme);
        let width_event_count = host.events.borrow().iter().filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_))).count();
        let repaint_count = host.repaint_requests.get();
        core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
        assert_eq!(host.events.borrow().iter().filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_))).count(), width_event_count,);
        assert_eq!(host.repaint_requests.get(), repaint_count);

        core.apply(&mut shaper, crate::RendererCommand::SetSearchActive(true));
        core.apply(&mut shaper, crate::RendererCommand::SetSearchQuery("needle".to_string()));
        assert_eq!(core.highlight.matches.len(), 2);
        assert_eq!(core.highlight.current_match, 0);
        core.apply(&mut shaper, crate::RendererCommand::NavigateSearch(1));
        assert_eq!(core.highlight.current_match, 1);

        let (start, end) = core.highlight.matches[0];
        core.selection.selection_anchor = Some(start);
        core.selection.selection_active = Some(end);
        core.update_selection_text();
        assert_eq!(core.selection.selected_text.as_deref(), Some("Needle"));
        assert_eq!(core.selection.selected_text_markdown.as_deref(), Some("**Needle**"));
        let range_cfi = crate::cfi::generate_cfi_range(&core.document, 0, start, end).expect("selection has a range CFI");
        assert_eq!(crate::cfi::parse_cfi_range(&core.document, &range_cfi), Some((0, start, end)));
        core.apply(
            &mut shaper,
            crate::RendererCommand::SetAnnotations(vec![crate::RendererAnnotation {
                id: "annotation-1".to_owned(),
                cfi_range: range_cfi,
                exact_text: "Needle".to_owned(),
                prefix: None,
                suffix: None,
                style: crate::AnnotationStyle::Highlight,
                color: [255, 220, 0, 96],
            }]),
        );
        assert!(core.activate_annotation_at_glyph(start));
        core.apply(
            &mut shaper,
            crate::RendererCommand::SetAnnotations(vec![crate::RendererAnnotation {
                id: "annotation-recovered".to_owned(),
                cfi_range: "epubcfi(/6/2!/999,/1:0,/1:6)".to_owned(),
                exact_text: "Needle".to_owned(),
                prefix: None,
                suffix: Some(" and".to_owned()),
                style: crate::AnnotationStyle::Underline,
                color: [255, 220, 0, 96],
            }]),
        );
        assert!(core.activate_annotation_at_glyph(start));
        core.apply(&mut shaper, crate::RendererCommand::SetMediaOverlayTarget(Some("#spoken".to_owned())));
        assert!(core.media_overlay.visible.is_some_and(|(overlay_start, overlay_end)| overlay_start <= start && overlay_end > end));
        core.apply(&mut shaper, crate::RendererCommand::SetMediaOverlayTarget(None));
        assert_eq!(core.media_overlay.visible, None);
        assert_eq!(core.copy_selection_to_clipboard(), Ok(true));
        assert_eq!(host.clipboard_text.borrow().as_deref(), Some("**Needle**"));

        core.configure_layout_for_viewport(&mut shaper, Size::new(1_600.0, 800.0));
        assert_eq!(core.layout.col_count, 2.0);
        assert!(core.layout.col_gap >= 0.0);

        let events = host.events.borrow();
        assert!(events.iter().any(|event| matches!(event, RendererEvent::ColumnWidthChanged(width) if *width == 480.0)));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 18.0)));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::ScaleChanged(scale) if *scale == 1.5)));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::SearchActiveChanged(true))));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::MatchInfo { current: 2, total: 2 })));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-1")));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-recovered")));
        assert!(host.repaint_requests.get() > 0);
        drop(events);

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn failed_pipeline_update_keeps_committed_settings_and_reports_the_error() {
        let (provider, _) = CountingProvider::new(vec![("doc.html", "<html><body><p>Transactional settings</p></body></html>")]);
        let provider: Arc<dyn crate::ResourceProvider> = provider;
        let mut shaper = TestShaper::default();
        let loaded = load::load_document(provider.clone(), "doc.html", 16.0, 600.0, &mut shaper);
        let host = Rc::new(TestHost::default());
        let mut core = RendererCore::new(host.clone(), loaded, provider, vec!["doc.html".to_owned()], 0, RendererInitialConfig::default());
        let committed_size = core.root_font_size;
        let committed_input_size = core.pipeline_inputs.style_environment.root_font_size;
        host.events.borrow_mut().clear();

        shaper.reject_glyphs = true;
        core.set_root_font_size(&mut shaper, 24.0);

        assert_eq!(core.root_font_size, committed_size);
        assert_eq!(core.pipeline_inputs.style_environment.root_font_size, committed_input_size);
        let events = host.events.borrow();
        assert!(!events.iter().any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 24.0)));
        assert!(events.iter().any(|event| matches!(event, RendererEvent::OperationFailed { operation: RendererOperation::UpdateDocument, message } if !message.is_empty())));
    }

    #[test]
    fn pre_frame_relayout_preserves_the_document_start_before_the_first_glyph() {
        let (provider, _) = CountingProvider::new(vec![("document.html", "<html><body style='margin:0'><div style='height:100px;background:green'></div><p style='margin:0'>First text</p></body></html>")]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, None, RendererInitialConfig::default());

        assert_eq!(core.viewport.start_offset_y, 0.0);
        core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
        assert_eq!(core.viewport.start_offset_y, 0.0, "relayout before the first frame must retain leading non-text content");

        core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
        core.paint_forward(&mut RecordingPainter::default());
        let document_y = core.document.render_view().text().line(0).expect("fixture has a text line").point().y;
        let screen_y = core.frame.last_line_positions.get(0).expect("first line is visible").point.y;
        assert_eq!(screen_y, document_y, "the first frame must not scroll directly to its first glyph");
    }

    #[test]
    fn omitted_nav_state_stays_at_document_start_when_semantic_relayout_moves_a_later_paragraph() {
        let html = "<html><body style='margin:10px'><h1 style='margin:0;line-height:20px'>Heading</h1><div style='height:30px'></div><p style='margin:0;line-height:20px;widows:2;orphans:2'>B<br>C</p></body></html>";
        let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, None, RendererInitialConfig::default());

        core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 80.0));
        core.paint_forward(&mut RecordingPainter::default());

        let heading = line_index_for_character(&core, 'H');
        let moved = line_index_for_character(&core, 'B');
        assert_eq!(core.viewport.start_offset_y, 0.0, "a publication without saved state must retain the true document origin");
        assert_eq!(core.frame.current_page_start_line, Some(heading));
        assert!(core.frame.last_line_positions.get(heading).is_some());
        assert!(core.frame.last_line_positions.get(moved).is_none(), "the protected paragraph should move without becoming the restored startup anchor");
        assert_eq!(core.document.render_view().text().line(moved).expect("moved line").point().y, 60.0, "continuous layout must not contain a pagination gap");
    }

    #[test]
    fn navigation_reuses_cached_documents_between_adjacent_spine_entries() {
        let (provider, read_count) = CountingProvider::new(vec![("doc0.html", "<html><body><a href=\"doc1.html\">next</a></body></html>"), ("doc1.html", "<html><body><a href=\"doc0.html\">prev</a></body></html>")]);

        let host = Rc::new(TestHost::default());
        let uris = vec!["doc0.html".to_owned(), "doc1.html".to_owned()];
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

        let indexed_baseline = read_count.load(Ordering::Relaxed);
        assert_eq!(indexed_baseline, 1);

        core.next_document(&mut shaper);
        assert_eq!(core.nav.current_doc_index, 1);
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);

        core.prev_document(&mut shaper);
        assert_eq!(core.nav.current_doc_index, 0);
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);

        core.next_document(&mut shaper);
        assert_eq!(core.nav.current_doc_index, 1);
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);
    }

    #[test]
    fn navigation_cache_uses_eviction_when_capacity_is_exceeded() {
        let (provider, read_count) = CountingProvider::new(vec![
            ("doc0.html", "<html><body><p>doc0</p></body></html>"),
            ("doc1.html", "<html><body><p>doc1</p></body></html>"),
            ("doc2.html", "<html><body><p>doc2</p></body></html>"),
            ("doc3.html", "<html><body><p>doc3</p></body></html>"),
            ("doc4.html", "<html><body><p>doc4</p></body></html>"),
        ]);

        let host = Rc::new(TestHost::default());
        let uris = vec!["doc0.html".to_owned(), "doc1.html".to_owned(), "doc2.html".to_owned(), "doc3.html".to_owned(), "doc4.html".to_owned()];
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

        let indexed_baseline = read_count.load(Ordering::Relaxed);
        assert_eq!(indexed_baseline, 1);

        for _ in 0..4 {
            core.next_document(&mut shaper);
        }
        assert_eq!(core.nav.current_doc_index, 4);
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 4);

        while core.nav.current_doc_index > 0 {
            core.prev_document(&mut shaper);
        }
        assert_eq!(core.nav.current_doc_index, 0);

        assert!(core.nav.history.is_at_newest(), "walking back to the first document should leave the history cursor at its newest entry");
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 6);
    }

    #[test]
    fn publication_search_stops_between_spine_documents_when_cancelled() {
        let (provider, read_count) = CountingProvider::new(vec![("doc0.html", "<p>needle first</p>"), ("doc1.html", "<p>needle second</p>"), ("doc2.html", "<p>needle third</p>")]);
        let documents = vec!["doc0.html".to_owned(), "doc1.html".to_owned(), "doc2.html".to_owned()];
        let cancellation_checks = Cell::new(0usize);

        let mut streamed = Vec::new();
        let matches = crate::search_publication_streaming(
            provider.as_ref(),
            &documents,
            "needle",
            crate::SearchOptions::default(),
            || {
                let checks = cancellation_checks.get();
                cancellation_checks.set(checks + 1);
                checks > 1
            },
            |batch| streamed.extend_from_slice(batch),
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(streamed.len(), 1);
        assert_eq!(matches[0].doc, 0);
        assert_eq!(read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn footnote_preview_requires_note_semantics_and_resolves_cross_spine_targets() {
        let (provider, read_count) = CountingProvider::new(vec![
            ("chapter.html", "<p><a epub:type=\"noteref\" href=\"notes.html#n1\">1</a><a href=\"notes.html#ordinary\">more</a></p>"),
            ("notes.html", "<aside id=\"n1\" epub:type=\"footnote\"><p><em>Structured</em> note text</p><ul><li>First source</li></ul></aside><section id=\"ordinary\">Ordinary section</section>"),
        ]);
        let host = Rc::new(TestHost::default());
        let uris = vec!["chapter.html".to_owned(), "notes.html".to_owned()];
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());
        let initial_reads = read_count.load(Ordering::Relaxed);

        assert_eq!(core.footnote_preview(&mut shaper, "notes.html#n1").map(|preview| preview.blocks), Some(vec!["Structured note text".to_owned(), "• First source".to_owned()]));
        let indexed_reads = read_count.load(Ordering::Relaxed);
        assert_eq!(indexed_reads, initial_reads + 1, "the target spine document should be indexed once on first use");
        assert_eq!(core.footnote_preview(&mut shaper, "notes.html#ordinary"), None);
        assert_eq!(read_count.load(Ordering::Relaxed), indexed_reads, "subsequent targets in the same spine document must reuse its semantic index");
    }

    #[test]
    fn note_reference_classification_is_per_link_even_when_hrefs_match() {
        let html = "<html><body><p><a href='#target'>R</a><a role='doc-noteref' href='#target'>N</a></p><div id='target'><p>Popup body</p></div></body></html>";
        let (provider, read_count) = CountingProvider::new(vec![("document.html", html)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
        let regular = glyph_index_for_character(&core, 'R');
        let noteref = glyph_index_for_character(&core, 'N');
        let addressing = core.document.render_view().addressing();

        assert!(!addressing.is_note_reference(regular));
        assert!(addressing.is_note_reference(noteref));
        let reads_after_preparation = read_count.load(Ordering::Relaxed);

        assert!(core.handle_link_click(&mut shaper, regular));
        assert!(!host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(_))), "an ordinary link sharing the href must navigate normally");
        host.events.borrow_mut().clear();

        assert!(core.handle_link_click(&mut shaper, noteref));
        assert!(host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(preview) if preview.blocks == ["Popup body"])));
        assert_eq!(read_count.load(Ordering::Relaxed), reads_after_preparation, "same-document previews must use the index built during preparation");
    }

    #[test]
    fn semantic_footnotes_are_popup_only_and_absent_from_paginated_text() {
        let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
        let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);
        let text = core.document.render_view().text();
        let noteref = glyph_index_for_character(&core, '1');
        assert!(core.document.render_view().addressing().is_note_reference(noteref));
        let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();

        assert!(visible_source.contains("Reading"));
        assert!(visible_source.contains("Continues"));
        assert!(!visible_source.contains("zebra"), "the footnote body must not consume paginated layout space");
        assert_eq!(core.footnote_preview(&mut shaper, "#note").map(|preview| preview.blocks), Some(vec!["Popup-only zebra".to_owned()]));
    }

    #[test]
    fn as_authored_notes_read_inline_instead_of_being_held_for_a_popup() {
        // The same fixture as the popup test above, differing only in the
        // display mode. Under `AsAuthored` the reader injects no suppression
        // rule, so the note reads where its author placed it.
        let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
        let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let config = RendererInitialConfig {
            font_size: 16.0,
            column_width: 200.0,
            max_column_count: Some(1),
            image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
            text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
            note_display: crate::NoteDisplay::AsAuthored,
            ..RendererInitialConfig::default()
        };
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);
        core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
        core.paint_forward(&mut RecordingPainter::default());

        let text = core.document.render_view().text();
        let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();
        assert!(visible_source.contains("zebra"), "an as-authored note occupies the reading flow rather than being held back for a popup");

        // The engine's note semantics are untouched by the display mode: the
        // target is still a note, which is what a later mode switch relies on.
        assert!(core.document.render_view().addressing().is_note_target("note"));
    }

    #[test]
    fn an_as_authored_note_reference_navigates_rather_than_opening_a_popup() {
        // The note reads in place under this mode, so a popup would repeat
        // text already on the page. The reference resolves as a plain link.
        let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside></body></html>";
        let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let config = RendererInitialConfig {
            font_size: 16.0,
            column_width: 200.0,
            max_column_count: Some(1),
            image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
            text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
            note_display: crate::NoteDisplay::AsAuthored,
            ..RendererInitialConfig::default()
        };
        let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);
        core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
        core.paint_forward(&mut RecordingPainter::default());

        let noteref = glyph_index_for_character(&core, '1');
        assert!(core.document.render_view().addressing().is_note_reference(noteref), "the reference keeps its note semantics; only the presentation differs");

        assert!(core.handle_link_click(&mut shaper, noteref));

        assert!(!host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(_))), "a note that reads in place must not also be shown as a popup");
        assert_eq!(core.footnote_preview(&mut shaper, "#note"), None);
    }

    #[test]
    fn namespaced_xhtml_footnotes_are_popup_only_and_absent_from_paginated_text() {
        let html = r##"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><p>Reading<a epub:type="noteref" href="#note">1</a></p><aside id="note" epub:type="footnote"><p>Popup-only magenta</p></aside><p>Continues</p></body>
</html>"##;
        let (provider, _) = CountingProvider::new(vec![("document.xhtml", html)]);
        let host = Rc::new(TestHost::default());
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.xhtml".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
        core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
        core.paint_forward(&mut RecordingPainter::default());

        let text = core.document.render_view().text();
        let noteref = glyph_index_for_character(&core, '1');
        assert!(core.document.render_view().addressing().is_note_reference(noteref), "namespaced EPUB noteref semantics must survive preparation");
        let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();
        assert!(visible_source.contains("Reading"));
        assert!(visible_source.contains("Continues"));
        assert!(!visible_source.contains("magenta"), "the namespaced XHTML footnote body must not consume paginated layout space");
        assert_eq!(core.footnote_preview(&mut shaper, "#note").map(|preview| preview.blocks), Some(vec!["Popup-only magenta".to_owned()]));
    }
}
