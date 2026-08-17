use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

pub use html_view_doc::DocQuery;
pub use html_view_navigate::{NavigationState, SpineAnchor};
pub use html_view_paint::{AnnotationOverlayState, HighlightState, MediaOverlayHighlightState};
pub use html_view_select::{SelectionMode, SelectionState, TableSelectionState};
pub use html_view_types::*;
use kurbo::Point;

use crate::document_cache::DocumentCache;
use crate::document_view::DocumentView;
use crate::prepared_page::PreparedPageCache;
use crate::spine_item::SpineItem;
use html::layout::GlyphShaper;
pub use html::layout::{DocumentTocNode, TextDecorationLines, UsedBorderRadii};
use html::pipeline::{EarliestStage, PipelineInputs};
pub use html::pipeline::{ImageSizingPolicy, RootFontSize, TextCompositionPolicy};
pub use html::resources::{FileSystemProvider, ResourceMetadata, ResourceProvider, TocEntry};
use html::resources::{ImagePipeline, ImageService};

pub use html_view_cfi as cfi;

mod document_cache;
mod document_view;
mod prepared_page;
mod spine_item;

pub mod layout {
    pub use html::layout::{LaidOutDocument, LayoutConstraintError, LayoutConstraints, LayoutTimings, PreparedDocument, ShapeError, ShapedDocument};
}

pub mod parser {
    pub use html::pipeline::{BuildPipelineTimings, DocumentFactory, ParsedHtml, parse_html_document};
}

pub mod resources {
    pub use html::resources::{FileSystemProvider, ImagePipelinePoll, ResourceMetadata, ResourceProvider, TocEntry};
}

/// Renderer-neutral text backend contract. UI adapters should use this module
/// rather than importing layout or CSS model types.
pub mod text_backend {
    pub use html::layout::{
        CharacterPlacement, FontMetricsRequest, FontRelativeMetrics, FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper, OpenTypeFeature, ShapeError, ShapedLine, ShapedTextRun, TextRunId, TextRunShapeRequest, TextShapeRequest,
        TextStyleSpan,
    };
}

pub use html::layout::LaidOutDocument;
pub use html::render::{Painter, RecordingPainter, TextRunFragment};

mod session;
pub use session::{FramePainter, PointerDownOptions, PointerMoveOutcome, PreparedFrame, RendererSession};

const DEFAULT_DOCUMENT_CACHE_CAPACITY: usize = 3;

fn document_text_index(source: &str, uri: &str) -> Option<html::parse::DocumentTextIndex> {
    let parsed = html::parse::parse_document(source, html::parse::MarkupSyntax::from_uri(uri)).ok()?;
    html::parse::DocumentTextIndex::from_parsed(&parsed).ok()
}

pub fn search_publication(provider: &dyn ResourceProvider, document_uris: &[String], query: &str, options: SearchOptions) -> Vec<BookSearchMatch> {
    search_publication_cancellable(provider, document_uris, query, options, || false)
}

pub fn search_publication_cancellable(provider: &dyn ResourceProvider, document_uris: &[String], query: &str, options: SearchOptions, cancelled: impl FnMut() -> bool) -> Vec<BookSearchMatch> {
    search_publication_streaming(provider, document_uris, query, options, cancelled, |_| {})
}

/// Searches a publication cooperatively and reports bounded incremental
/// batches. The returned vector is retained for non-streaming callers.
pub fn search_publication_streaming(provider: &dyn ResourceProvider, document_uris: &[String], query: &str, options: SearchOptions, mut cancelled: impl FnMut() -> bool, mut on_batch: impl FnMut(&[BookSearchMatch])) -> Vec<BookSearchMatch> {
    const MAX_RESULTS: usize = 10_000;
    const BATCH_SIZE: usize = 64;
    if query.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    let mut batch_start = 0usize;
    for (doc, uri) in document_uris.iter().enumerate() {
        if cancelled() || results.len() >= MAX_RESULTS {
            break;
        }
        let Ok(source) = provider.read_string(uri) else {
            continue;
        };
        let Some(index) = document_text_index(&source, uri) else {
            continue;
        };
        let text = index.text();
        let chars: Vec<char> = text.chars().collect();
        for (start, end) in html_view_doc::find_text_matches(text, query, options) {
            if cancelled() || results.len() >= MAX_RESULTS {
                break;
            }
            let Some(position) = index.source_position_in(start..end) else {
                continue;
            };
            let excerpt_start = start.saturating_sub(42);
            let excerpt_end = (end + 70).min(chars.len());
            let excerpt = chars[excerpt_start..excerpt_end].iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
            results.push(BookSearchMatch { doc, target: BookSearchTarget::SourceCfi(crate::cfi::generate_cfi_from_source_position(doc, &position)), excerpt });
            if results.len() - batch_start >= BATCH_SIZE {
                on_batch(&results[batch_start..]);
                batch_start = results.len();
            }
        }
    }
    if batch_start < results.len() {
        on_batch(&results[batch_start..]);
    }
    results
}

pub fn publication_text_lengths(provider: &dyn ResourceProvider, document_uris: &[String]) -> Vec<u64> {
    document_uris
        .iter()
        .map(|uri| provider.read_string(uri).ok().and_then(|source| document_text_index(&source, uri)).map(|index| index.text().chars().filter(|character| !character.is_whitespace()).count() as u64).unwrap_or(1).max(1))
        .collect()
}

#[derive(Default)]
struct FrameScratch {
    line_position_buffers: Vec<Vec<LineScreen>>,
    desired_images: HashSet<u32>,
    pagination: html_view_paginate::PaginationScratch,
}

impl FrameScratch {
    fn take_line_position_buffer(&mut self, frame: &mut VisibleFrame) -> Vec<LineScreen> {
        self.line_position_buffers.pop().unwrap_or_else(|| frame.take_line_positions().into_buffer())
    }

    fn recycle_line_position_buffer(&mut self, mut buffer: Vec<LineScreen>) {
        buffer.clear();
        if buffer.capacity() > 0 {
            self.line_position_buffers.push(buffer);
        }
    }

    fn clear_transient(&mut self) {
        self.desired_images.clear();
    }
}

#[cfg(test)]
mod frame_cache_tests {
    use super::*;

    #[test]
    fn visible_line_positions_keep_global_indexes_and_reuse_capacity() {
        let mut buffer = Vec::with_capacity(32);
        buffer.push(LineScreen { point: Point::new(0.0, 0.0), col_index: 0 });
        let mut positions = VisibleLinePositions::with_buffer(40, buffer);
        positions.push(LineScreen { point: Point::new(10.0, 20.0), col_index: 1 });

        assert_eq!(positions.first_line(), 40);
        assert_eq!(positions.last_line(), Some(40));
        assert_eq!(positions.get(40).map(|screen| screen.col_index), Some(1));
        assert!(positions.get(39).is_none());

        let capacity = positions.capacity();
        let mut frame = VisibleFrame::default();
        frame.install_whole_document(1, positions);
        let mut scratch = FrameScratch::default();
        frame.clear();
        let reused = scratch.take_line_position_buffer(&mut frame);
        assert!(reused.capacity() >= capacity);
    }
}

pub(crate) struct RendererCore {
    host: Rc<dyn RendererHost>,
    /// The spine item being read: the document, what is on screen of it, and
    /// the pipeline and resources addressed by it.
    page: SpineItem,
    /// The note currently shown beside the page, if any. Selecting and hit
    /// testing work here through the same code the page uses.
    note: Option<DocumentView>,
    /// The spine item whose glyph registry owns the open note.
    note_document: Option<usize>,
    provider: Arc<dyn ResourceProvider>,
    /// One source-keyed worker pool and decoded-image budget for the book.
    image_service: ImageService,
    root_font_size: f32,
    viewport: ViewportState,
    document_cache: DocumentCache,
    prepared_pages: PreparedPageCache,
    frame_scratch: FrameScratch,
    revisions: RendererRevisions,
    highlight: HighlightState,
    annotations: AnnotationOverlayState,
    media_overlay: MediaOverlayHighlightState,
    nav: NavigationState,
    origin: Point,
    interaction_palette: InteractionPalette,
    reader_palette: ReaderPaintPalette,
    vertical_rhythm: bool,
    /// Presentation policy for note bodies. Held here rather than derived from
    /// `pipeline_inputs` because it decides what goes *into* those inputs, and
    /// every subsequent document load has to agree with the first.
    note_display: NoteDisplay,
}

impl RendererCore {
    pub(crate) fn doc(&self) -> DocQuery<'_> {
        DocQuery::new(&self.page.view.document)
    }

    pub fn display_cache_keys(&self) -> FrameDisplayCacheKeys {
        let geometry = self.frame_geometry_cache_key();
        let key = |display_revision, resources_revision| DisplayCacheKey {
            layout_revision: geometry.layout_revision,
            display_revision,
            resources_revision,
            viewport_width_bits: geometry.viewport_width_bits,
            viewport_height_bits: geometry.viewport_height_bits,
            column_width_bits: geometry.column_width_bits,
            column_gap_bits: geometry.column_gap_bits,
            column_count: geometry.column_count,
            scale_bits: geometry.scale_bits,
            page_offset_bits: geometry.page_offset_bits,
            direction: geometry.direction,
        };
        FrameDisplayCacheKeys { base: key(self.revisions.base_display, self.revisions.resources), overlay: key(self.revisions.overlay, 0) }
    }

    pub fn frame_geometry_cache_key(&self) -> FrameGeometryCacheKey {
        self.frame_geometry_cache_key_for(&self.viewport)
    }

    fn frame_geometry_cache_key_for(&self, viewport: &ViewportState) -> FrameGeometryCacheKey {
        let composition = viewport.composition();
        FrameGeometryCacheKey {
            layout_revision: self.revisions.layout,
            viewport_width_bits: self.page.view.layout.size.width.to_bits(),
            viewport_height_bits: self.page.view.layout.size.height.to_bits(),
            column_width_bits: self.page.view.layout.col_width.to_bits(),
            column_gap_bits: self.page.view.layout.col_gap.to_bits(),
            column_count: self.page.view.layout.col_count.clamp(0.0, 255.0) as u8,
            scale_bits: self.page.view.layout.scale.to_bits(),
            page_offset_bits: composition.active_offset_y().to_bits(),
            direction: match composition.direction() {
                PaintDirection::Forward => 0,
                PaintDirection::Backward => 1,
            },
        }
    }

    fn request_base_repaint(&mut self) {
        self.revisions.invalidate_base();
        self.host.request_repaint();
    }

    fn request_overlay_repaint(&mut self) {
        self.revisions.invalidate_overlay();
        self.host.request_repaint();
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette) {
        if self.interaction_palette == palette {
            return;
        }
        self.interaction_palette = palette;
        self.request_overlay_repaint();
    }

    pub fn to_layout_point(&self, position: Point) -> Point {
        let scale = self.page.view.layout.scale;
        let dx = self.origin.x * (scale - 1.0);
        let dy = self.origin.y * (scale - 1.0);
        Point::new((position.x + dx) / scale, (position.y + dy) / scale)
    }

    pub fn activate_link_cursor(&mut self) {
        if self.page.view.selection.activate_link_cursor() {
            self.host.request_style();
        }
    }

    pub fn deactivate_link_cursor(&mut self) {
        if self.page.view.selection.deactivate_link_cursor() {
            self.host.request_style();
        }
    }

    pub(crate) fn emit_operation_failed(&self, operation: RendererOperation, message: String) {
        self.host.emit(RendererEvent::OperationFailed { operation, message });
    }

    pub fn new(host: Rc<dyn RendererHost>, loaded: load::LoadedRenderDocument, provider: Arc<dyn ResourceProvider>, document_uris: Vec<String>, current_doc_index: usize, config: RendererInitialConfig) -> Self {
        let document = loaded.document;
        let root_font_size = loaded.inputs.style_environment.root_font_size as f32;
        let image_resources = document.render_view().images().to_vec();
        let image_service = ImageService::new(provider.clone(), ImageService::DEFAULT_BYTE_BUDGET);
        image_service.set_completion_waker(host.resource_waker());
        let image_pipeline = ImagePipeline::with_service(Arc::new(image_resources), image_service.clone());
        let document_cache = DocumentCache::new(DEFAULT_DOCUMENT_CACHE_CAPACITY);
        let column_width = config.column_width.clamp(200.0, 1200.0).round();
        let layout = ColumnLayout { col_width: column_width, base_col_width: column_width, scale: config.scale.clamp(0.5, 3.0), max_col_count: config.max_column_count.map(|count| count.max(1)), ..Default::default() };
        let mut highlight = HighlightState::default();
        highlight.restart(config.search_query, SearchOptions::default());
        if config.search_active {
            highlight.activate();
        }
        Self {
            host,
            page: SpineItem { session: loaded.session, inputs: loaded.inputs, images: image_pipeline, view: DocumentView::new(document, layout) },
            note: None,
            note_document: None,
            provider,
            image_service,
            root_font_size,
            viewport: ViewportState::default(),
            document_cache,
            prepared_pages: PreparedPageCache::new(),
            frame_scratch: FrameScratch::default(),
            revisions: RendererRevisions::default(),
            highlight,
            annotations: AnnotationOverlayState::default(),
            media_overlay: MediaOverlayHighlightState::default(),
            nav: NavigationState::new(document_uris, current_doc_index),
            origin: Point::ZERO,
            interaction_palette: config.interaction_palette,
            reader_palette: config.paint_palette,
            vertical_rhythm: config.vertical_rhythm,
            note_display: config.note_display,
        }
    }

    fn next_pipeline_inputs_for_layout(&self, viewport_width: f64) -> PipelineInputs {
        let mut next = self.page.inputs.clone();
        next.layout = html::pipeline::LayoutConstraints {
            viewport_width,
            viewport_height: (self.page.view.layout.size.height.is_finite() && self.page.view.layout.size.height >= 0.0).then_some(self.page.view.layout.size.height),
            line_height: self.page.inputs.layout.line_height,
            image_sizing_policy: self.page.inputs.layout.image_sizing_policy,
            text_composition_policy: self.page.inputs.layout.text_composition_policy,
        };
        let viewport_height = (self.page.view.layout.size.height.is_finite() && self.page.view.layout.size.height > 0.0).then_some(self.page.view.layout.size.height);
        next.style_environment.media = html::pipeline::MediaEnvironment::screen(viewport_width, viewport_height).expect("effective column dimensions must be finite and positive");
        next
    }

    fn next_pipeline_inputs_for_style(&self, root_font_size: f32) -> PipelineInputs {
        let mut next = self.page.inputs.clone();
        next.style_environment.root_font_size = root_font_size.max(1.0).round() as u32;
        next.layout = html::pipeline::LayoutConstraints {
            viewport_width: next.layout.viewport_width,
            viewport_height: next.layout.viewport_height,
            line_height: next.style_environment.root_font_size as f64,
            image_sizing_policy: next.layout.image_sizing_policy,
            text_composition_policy: next.layout.text_composition_policy,
        };
        next
    }

    fn apply_pipeline_inputs(&mut self, requested_inputs: PipelineInputs, glyph_shaper: &mut impl GlyphShaper) -> Result<(), html::pipeline::PipelineError> {
        self.apply_pipeline_inputs_impl(requested_inputs, glyph_shaper, true)
    }

    fn apply_pipeline_inputs_without_repaint(&mut self, requested_inputs: PipelineInputs, glyph_shaper: &mut impl GlyphShaper) -> Result<(), html::pipeline::PipelineError> {
        self.apply_pipeline_inputs_impl(requested_inputs, glyph_shaper, false)
    }

    fn apply_pipeline_inputs_impl(&mut self, requested_inputs: PipelineInputs, glyph_shaper: &mut impl GlyphShaper, request_repaint: bool) -> Result<(), html::pipeline::PipelineError> {
        // Before the first frame, `current_glyph_position` resolves to the
        // document's first glyph even when decoration or replaced content
        // precedes it. Relayout at the true document start must preserve the
        // zero offset instead of manufacturing a glyph anchor that scrolls
        // past that leading content.
        let first_glyph = self.page.view.document.render_view().text().line(0).map(|line| line.start());
        let anchor_is_document_start = match (self.nav.anchor_glyph_here(), first_glyph) {
            (None, _) => true,
            (Some(anchor), Some(first)) => anchor == first,
            (Some(_), None) => false,
        };
        let composition = self.viewport.composition();
        let preserve_document_start = anchor_is_document_start && matches!(composition.direction(), PaintDirection::Forward) && composition.start_offset_y() <= 0.1 && self.page.view.frame.page().start_line().is_none_or(|line| line == 0);
        let anchor_glyph = if preserve_document_start { None } else { self.nav.anchor_glyph_here().or_else(|| self.nav.current_glyph_position(self.nav_view())) };
        let anchor_cfi = if preserve_document_start { None } else { self.nav.current_cfi(self.nav_view()) };

        self.host.set_glyph_document(self.nav.location().document());
        let update = self.page.session.update(requested_inputs.clone(), glyph_shaper)?;

        let document = self.page.session.document().cloned().ok_or_else(|| html::pipeline::PipelineError("pipeline update completed without a laid-out document".to_owned()))?;
        self.root_font_size = self.page.install_pipeline_result(document, requested_inputs);
        if update.stage == EarliestStage::None {
            // reset_view_state also clears selection even when all pipeline
            // products were reusable.
            self.revisions.invalidate_overlay();
        } else {
            self.revisions.invalidate_pipeline_from(update.stage);
        }
        self.reset_view_state();
        if update.anchors_preserved {
            if let Some(glyph_idx) = anchor_glyph {
                self.restore_glyph_position(glyph_idx);
            }
        } else if let Some(cfi) = anchor_cfi
            && !self.restore_position_from_cfi(&cfi)
            && let Some(glyph_idx) = anchor_glyph
        {
            self.restore_glyph_position(glyph_idx);
        }

        if request_repaint {
            self.host.request_repaint();
        }
        Ok(())
    }

    pub fn emit_state_snapshot(&mut self) {
        self.host.emit(RendererEvent::TitleChanged(self.page.view.document.render_view().title().map(str::to_owned)));
        self.host.emit(RendererEvent::TocChanged(Self::build_document_toc_entries_for(&self.page.view.document)));
        self.nav.history.emit_availability(self.host.as_ref());
        self.host.emit(RendererEvent::SearchActiveChanged(self.highlight.is_active()));
        self.highlight.emit_match_signal(self.host.as_ref());
        self.host.emit(RendererEvent::ColumnWidthChanged(self.page.view.layout.base_col_width));
        self.host.emit(RendererEvent::FontSizeChanged(self.root_font_size));
        self.host.emit(RendererEvent::ScaleChanged(self.page.view.layout.scale));
        self.update_nav_signal();
    }

    pub fn apply(&mut self, glyph_shaper: &mut impl GlyphShaper, command: RendererCommand) {
        match command {
            RendererCommand::NavigateToHref(href) if !href.is_empty() => {
                self.navigate_to_href(glyph_shaper, &href);
            }
            RendererCommand::NavigateToHref(_) => {}
            RendererCommand::NavigateHistory(step) if step < 0 => {
                self.navigate_history_back(glyph_shaper);
            }
            RendererCommand::NavigateHistory(step) if step > 0 => {
                self.navigate_history_forward(glyph_shaper);
            }
            RendererCommand::NavigateHistory(_) => {}
            RendererCommand::NavigateDocument(step) if step < 0 => self.prev_document(glyph_shaper),
            RendererCommand::NavigateDocument(_) => self.next_document(glyph_shaper),
            RendererCommand::SetPosition { doc, glyph } => {
                self.set_position(glyph_shaper, doc, glyph);
            }
            RendererCommand::SetCfiPosition(Some(cfi)) => {
                self.set_cfi_position(&cfi);
            }
            RendererCommand::SetCfiPosition(None) => {}
            RendererCommand::SetProgressFraction(fraction) => self.set_progress_fraction(glyph_shaper, fraction),
            RendererCommand::SetDocumentTextLengths(lengths) => {
                if self.nav.install_document_text_lengths(lengths) {
                    self.update_nav_signal();
                }
            }
            RendererCommand::SetColumnWidth(width) => self.set_col_width(glyph_shaper, width),
            RendererCommand::SetMaxColumnCount(count) => {
                let count = count.map(|count| count.max(1));
                if self.page.view.layout.max_col_count != count {
                    self.page.view.layout.max_col_count = count;
                    self.host.request_repaint();
                }
            }
            RendererCommand::SetFontSize(size) => self.set_root_font_size(glyph_shaper, size),
            RendererCommand::SetReaderStyleOverrides(overrides) => {
                let mut inputs = self.page.inputs.clone();
                if inputs.reader_overrides != overrides {
                    inputs.reader_overrides = overrides;
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetNoteDisplay(display) => {
                if self.note_display != display {
                    self.note_display = display;
                    let mut inputs = self.page.inputs.clone();
                    inputs.note_flow = load::note_flow_for(display);
                    // Note bodies are held back by suppressing their boxes, so
                    // a switch has to rebuild the box tree, not just repaint.
                    // Documents cached under the previous mode disagree with
                    // the new one and cannot be navigated back into.
                    self.document_cache.clear();
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetImageSizingPolicy(policy) => {
                let mut inputs = self.page.inputs.clone();
                if inputs.layout.image_sizing_policy != policy {
                    inputs.layout.image_sizing_policy = policy;
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetTextCompositionPolicy(policy) => {
                let mut inputs = self.page.inputs.clone();
                if inputs.layout.text_composition_policy != policy {
                    inputs.layout.text_composition_policy = policy;
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetReaderPaintPalette(palette) => {
                if self.reader_palette != palette {
                    self.reader_palette = palette;
                    self.request_base_repaint();
                }
            }
            RendererCommand::SetScale(scale) => self.set_scale(scale),
            RendererCommand::ActivateSearch => self.activate_search(),
            RendererCommand::DeactivateSearch => self.deactivate_search(),
            RendererCommand::SetSearchQuery(query) => self.set_search_query(&query),
            RendererCommand::SetSearch { query, options } => self.set_search(glyph_shaper, &query, options),
            RendererCommand::SetSearchResults { query, options, results } => self.set_search_results(glyph_shaper, query, options, results),
            RendererCommand::AppendSearchResults { query, options, results } => self.append_search_results(glyph_shaper, query, options, results),
            RendererCommand::NavigateSearch(step) if step > 0 => self.next_match(glyph_shaper),
            RendererCommand::NavigateSearch(_) => self.prev_match(glyph_shaper),
            RendererCommand::SetAnnotations(annotations) => {
                self.annotations.replace(annotations);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::UpsertAnnotation(annotation) => {
                self.annotations.upsert(annotation);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::RemoveAnnotation(id) => {
                self.annotations.remove(&id);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::SetMediaOverlayTarget(target_href) => {
                self.media_overlay.set_target(target_href);
                self.resolve_media_overlay_highlight();
                self.request_overlay_repaint();
            }
            RendererCommand::RequestVisibleText => self.emit_visible_text(),
            RendererCommand::CopySelection => {
                if let Err(message) = self.copy_selection_to_clipboard() {
                    self.emit_operation_failed(RendererOperation::CopySelection, message);
                }
            }
            RendererCommand::CopySelectionCitation { title, cfi } => {
                let text = self.page.view.selection.content().plain().unwrap_or_default();
                if !text.is_empty() {
                    let citation = match cfi {
                        Some(cfi) => format!("“{text}” — {title}, {cfi}"),
                        None => format!("“{text}” — {title}"),
                    };
                    if let Err(message) = self.host.set_clipboard(&citation) {
                        self.emit_operation_failed(RendererOperation::CopySelection, message);
                    }
                }
            }
        }
    }

    pub(crate) fn set_position(&mut self, glyph_shaper: &mut impl GlyphShaper, document: usize, glyph: Option<u32>) {
        if document != self.nav.location().document() {
            self.load_document_at(glyph_shaper, document, glyph.map(DocAnchor::Glyph).unwrap_or(DocAnchor::Start));
        } else if let Some(glyph) = glyph {
            self.restore_glyph_position(glyph);
            self.host.request_repaint();
        }
    }

    pub(crate) fn set_cfi_position(&mut self, cfi: &str) -> bool {
        let restored = self.restore_position_from_cfi(cfi);
        if restored {
            self.host.request_repaint();
        }
        restored
    }

    pub(crate) fn activate_search(&mut self) {
        if !self.highlight.activate() {
            return;
        }
        self.host.emit(RendererEvent::SearchActiveChanged(true));
        self.request_overlay_repaint();
    }

    pub(crate) fn deactivate_search(&mut self) {
        if !self.highlight.deactivate() {
            return;
        }
        self.host.emit(RendererEvent::SearchActiveChanged(false));
        self.set_search_query("");
    }

    pub fn set_toc_anchor_strings_by_doc(&mut self, anchors_by_doc: Vec<Vec<String>>) {
        let document_count = self.nav.documents().uris().len();
        self.nav.toc_anchors.set_strings_by_doc(anchors_by_doc, document_count);
        let view = html_view_navigate::NavView { doc: DocQuery::new(&self.page.view.document), viewport: &self.viewport, frame: &self.page.view.frame };
        self.nav.update_toc_anchor_filter(view);
    }

    pub fn set_search_query(&mut self, query: &str) {
        let options = self.highlight.current_document_options();
        self.highlight.restart(query.to_owned(), options);
        let matches = self.doc().find_matches_with_options(query, options);
        let empty = matches.is_empty();
        self.highlight.set_local_matches(self.nav.location().document(), matches);
        self.highlight.emit_results(self.host.as_ref());
        if empty {
            self.request_overlay_repaint();
        } else {
            self.navigate_to_current_match();
        }
    }

    pub fn set_search(&mut self, glyph_shaper: &mut impl GlyphShaper, query: &str, options: SearchOptions) {
        self.highlight.restart(query.to_owned(), options);
        let local_scope = options.scope == SearchScope::CurrentDocument;
        if query.is_empty() {
            // restart already cleared both result sets.
        } else if local_scope {
            let matches = self.doc().find_matches_with_options(query, options);
            self.highlight.set_local_matches(self.nav.location().document(), matches);
        } else {
            let results = search_publication(self.provider.as_ref(), self.nav.documents().uris(), query, options);
            self.highlight.set_results(query.to_owned(), options, results);
            self.navigate_to_book_match(glyph_shaper);
        }
        self.highlight.emit_results(self.host.as_ref());
        if local_scope && !self.highlight.is_empty() {
            self.navigate_to_current_match();
        } else if self.highlight.is_empty() {
            self.request_overlay_repaint();
        }
    }

    pub fn set_search_results(&mut self, glyph_shaper: &mut impl GlyphShaper, query: String, options: SearchOptions, results: Vec<BookSearchMatch>) {
        self.highlight.set_results(query, options, results);
        if self.highlight.is_empty() {
            self.request_overlay_repaint();
        } else {
            self.navigate_to_book_match(glyph_shaper);
        }
        self.highlight.emit_results(self.host.as_ref());
    }

    pub fn append_search_results(&mut self, glyph_shaper: &mut impl GlyphShaper, query: String, options: SearchOptions, results: Vec<BookSearchMatch>) {
        if self.highlight.append_results(query, options, results) {
            self.navigate_to_book_match(glyph_shaper);
        } else {
            self.request_overlay_repaint();
        }
        self.highlight.emit_results(self.host.as_ref());
    }

    fn navigate_to_book_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let Some(hit) = self.highlight.current_book_match() else {
            return;
        };
        if hit.doc != self.nav.location().document() {
            self.set_position(glyph_shaper, hit.doc, None);
        }
        let matches = self.doc().find_matches_with_options(self.highlight.query(), self.highlight.options());
        self.highlight.install_visible_matches(matches);
        let occurrence = match hit.target {
            BookSearchTarget::LocalOccurrence(occurrence) => Some(occurrence),
            BookSearchTarget::SourceCfi(cfi) => crate::cfi::parse_cfi(&self.page.view.document, &cfi)
                .and_then(|(doc, glyph)| (doc == self.nav.location().document()).then_some(glyph))
                .and_then(|glyph| self.highlight.matches().iter().position(|&(start, end)| start <= glyph && glyph < end)),
        };
        let Some(occurrence) = occurrence else {
            return;
        };
        self.highlight.focus_local_match(occurrence);
        self.navigate_to_current_match();
    }

    fn navigate_after_match_step(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        self.highlight.emit_match_signal(self.host.as_ref());
        self.navigate_to_book_match(glyph_shaper);
    }

    pub fn next_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.highlight.next_match() {
            self.navigate_after_match_step(glyph_shaper);
        }
    }

    pub fn prev_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        if self.highlight.previous_match() {
            self.navigate_after_match_step(glyph_shaper);
        }
    }
}

#[path = "renderer/selection.rs"]
mod selection;

#[path = "renderer/table_selection.rs"]
mod table_selection;

#[path = "renderer/annotations.rs"]
mod annotations;
#[path = "renderer/highlight.rs"]
mod highlight;

#[path = "renderer/render.rs"]
mod render;

#[path = "renderer/nav.rs"]
mod nav;

#[path = "renderer/interaction.rs"]
mod interaction;

pub use html_view_load as load;
