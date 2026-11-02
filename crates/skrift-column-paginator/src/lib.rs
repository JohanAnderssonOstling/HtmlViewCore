#[cfg(test)]
use html::layout::LineId;
use html::configuration::DocumentUpdate;
use std::rc::Rc;
use std::sync::Arc;

pub use html_view_doc::DocQuery;
pub use html_view_navigate::{NavigationState, SpineAnchor};
pub use html_view_paint::{AnnotationOverlayState, HighlightState, MediaOverlayHighlightState};
pub use html_view_select::{SelectionMode, SelectionState, TableSelectionState};
pub use html_view_types::*;
use kurbo::Point;

use crate::spine_item::SpineItem;
use html::layout::GlyphShaper;
pub use html::layout::{TextDecorationLines, UsedBorderRadii};
use html::pipeline::{EarliestStage, PipelineChanges};
pub use html::pipeline::{
    ImageSizingPolicy, ReaderStyleOverrides, RootFontSize, TextCompositionPolicy,
};
pub use html::resources::{
    BlockingJob, BlockingJobSpawner, FileSystemProvider, InlineJobSpawner, ResourceMetadata,
    ResourceProvider,
};
use html::resources::{ImageLoader, ImageService};
use html::search::SearchIndex;
pub use html::style::FontFace;

pub use html_view_cfi as cfi;

mod document_view;
mod spine_item;

pub mod layout {
    pub use html::layout::{
        LaidOutDocument, LayoutConstraintError, LayoutConstraints, LayoutTimings, PreparedDocument,
        ShapeError, ShapedDocument,
    };
}

pub mod parser {
    pub use html::pipeline::{DocumentFactory, ParsedHtml, parse_html_document};
}

pub mod resources {
    pub use html::resources::{
        FileSystemProvider, ImagePipelinePoll, ResourceMetadata, ResourceProvider,
    };
}

/// Renderer-neutral text backend contract. UI adapters should use this module
/// rather than importing layout or CSS model types.
pub mod text_backend {
    pub use html::layout::{
        CharacterPlacement, FontMetrics, FontMetricsRequest, FontRelativeMetrics, FontSlant,
        GlyphHandle, GlyphId, GlyphMetric, GlyphRequest, GlyphResourceGeneration,
        GlyphResourceStore, GlyphResources, GlyphShaper, OpenTypeFeature, ResourceShaper,
        ShapeError, ShapedGlyph, ShapedLine, ShapedTextRun, TextRunId, TextRunShapeRequest,
        TextShapeRequest, TextStyleSpan,
    };
}

pub use html::layout::LaidOutDocument;
pub use html::layout::{ReadingFragment, ReadingTarget};
pub use html::render::{Painter, RecordingPainter, TextRunFragment};

mod session;
pub use session::{
    FramePainter, PointerDownOptions, PointerMoveOutcome, PreparedFrame, ReadingLine,
    RendererSession, VisibleSourceText,
};

pub fn search_publication(
    provider: &dyn ResourceProvider,
    document_uris: &[String],
    query: &str,
    options: SearchOptions,
) -> Vec<BookSearchMatch> {
    search_publication_cancellable(provider, document_uris, query, options, || false)
}

pub fn search_publication_cancellable(
    provider: &dyn ResourceProvider,
    document_uris: &[String],
    query: &str,
    options: SearchOptions,
    cancelled: impl FnMut() -> bool,
) -> Vec<BookSearchMatch> {
    search_publication_streaming(provider, document_uris, query, options, cancelled, |_| {})
}

/// Searches a publication cooperatively and reports bounded incremental
/// batches. The returned vector is retained for non-streaming callers.
pub fn search_publication_streaming(
    provider: &dyn ResourceProvider,
    document_uris: &[String],
    query: &str,
    options: SearchOptions,
    cancelled: impl FnMut() -> bool,
    on_batch: impl FnMut(&[BookSearchMatch]),
) -> Vec<BookSearchMatch> {
    search_publication_with_cfi(
        provider,
        document_uris,
        &cfi::PublicationCfi::default(),
        query,
        options,
        cancelled,
        on_batch,
    )
}

/// Search results use the same package addresses as navigation and selection.
pub fn search_publication_with_cfi(
    provider: &dyn ResourceProvider,
    document_uris: &[String],
    cfi: &cfi::PublicationCfi,
    query: &str,
    options: SearchOptions,
    mut cancelled: impl FnMut() -> bool,
    mut on_batch: impl FnMut(&[BookSearchMatch]),
) -> Vec<BookSearchMatch> {
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
        let Ok(index) = SearchIndex::load(provider, uri) else {
            continue;
        };
        for hit in index.find(query, options.into()) {
            if cancelled() || results.len() >= MAX_RESULTS {
                break;
            }
            let excerpt = index
                .excerpt(hit.range, 42, 70)
                .unwrap_or_else(|| query.to_owned());
            let Some(location) = cfi.to_publication(
                &crate::cfi::generate_cfi_from_source_position(doc, &hit.source),
            ) else {
                continue;
            };
            results.push(BookSearchMatch {
                doc,
                target: BookSearchTarget::SourceCfi(location),
                excerpt,
            });
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

pub fn publication_text_lengths(
    provider: &dyn ResourceProvider,
    document_uris: &[String],
) -> Vec<u64> {
    document_uris
        .iter()
        .map(|uri| {
            SearchIndex::load(provider, uri)
                .ok()
                .map(|index| index.statistics().non_whitespace_characters)
                .unwrap_or(1)
                .max(1)
        })
        .collect()
}

#[derive(Default)]
struct FrameScratch {
    line_position_buffers: Vec<Vec<LineScreen>>,
    pagination: html_view_paginate::PaginationScratch,
}

impl FrameScratch {
    fn take_line_position_buffer(&mut self, frame: &mut VisibleFrame) -> Vec<LineScreen> {
        self.line_position_buffers
            .pop()
            .unwrap_or_else(|| frame.take_line_positions().into_buffer())
    }

    fn recycle_line_position_buffer(&mut self, mut buffer: Vec<LineScreen>) {
        buffer.clear();
        if buffer.capacity() > 0 {
            self.line_position_buffers.push(buffer);
        }
    }
}

#[cfg(test)]
mod frame_cache_tests {
    use super::*;

    #[test]
    fn visible_line_positions_keep_global_indexes_and_reuse_capacity() {
        let mut buffer = Vec::with_capacity(32);
        buffer.push(LineScreen {
            point: Point::new(0.0, 0.0),
            col_index: 0,
        });
        let mut positions = VisibleLinePositions::with_buffer(40, buffer);
        positions.push(LineId::new(40), LineScreen {
            point: Point::new(10.0, 20.0),
            col_index: 1,
        });

        assert_eq!(positions.first_cursor(), 40);
        assert_eq!(positions.last_cursor(), Some(40));
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

/// The provider-backed, parse-and-style portion of initial renderer creation.
/// It contains no UI framework objects or platform text-system handles and can
/// therefore be built on a host worker before a renderer entity is installed.
pub struct RendererPreparation {
    loaded: load::PreparedRenderDocument,
    provider: Arc<dyn ResourceProvider>,
    image_loader: Option<Arc<dyn ImageLoader>>,
    document_uris: Vec<String>,
    doc_index: usize,
    glyph_index: u32,
    nav_state: Option<String>,
    initial_anchor: Option<String>,
    config: RendererInitialConfig,
}

pub(crate) struct RendererCore {
    host: Rc<dyn RendererHost>,
    /// The spine item being read: the document, what is on screen of it, and
    /// the pipeline and resources addressed by it.
    page: SpineItem,
    /// The note currently shown beside the page, if any. Selecting and hit
    /// testing work here through the same code the page uses.
    note: Option<document_view::OpenNote>,
    provider: Arc<dyn ResourceProvider>,
    /// One source-keyed worker pool and decoded-image budget for the book.
    image_service: ImageService,
    root_font_size: f32,
    viewport: ViewportState,
    column_viewport_request: Option<nav::ColumnViewportRequest>,
    pending_navigation: Option<nav::PendingNavigation>,
    pending_file_events:
        std::cell::RefCell<Vec<std::sync::mpsc::Receiver<Result<RendererEvent, String>>>>,
    prepared_navigation: Option<(usize, load::LoadedRenderDocument)>,
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
        DocQuery::from_view(self.page.render_view())
    }

    pub fn display_cache_keys(&self) -> FrameDisplayCacheKeys {
        let geometry = self.frame_geometry_cache_key();
        let key = |display_revision, resources_revision| DisplayCacheKey {
            geometry,
            display_revision,
            resources_revision,
        };
        FrameDisplayCacheKeys {
            base: key(self.revisions.base_display, self.revisions.resources),
            overlay: key(self.revisions.overlay, 0),
        }
    }

    pub fn frame_geometry_cache_key(&self) -> FrameGeometryCacheKey {
        self.frame_geometry_cache_key_for(&self.viewport)
    }

    fn frame_geometry_cache_key_for(&self, viewport: &ViewportState) -> FrameGeometryCacheKey {
        let composition = viewport.composition();
        FrameGeometryCacheKey {
            layout_revision: self.revisions.layout,
            viewport_width_bits: self.page.layout.size.width.to_bits(),
            viewport_height_bits: self.page.layout.size.height.to_bits(),
            column_width_bits: self.page.layout.col_width.to_bits(),
            column_gap_bits: self.page.layout.col_gap.to_bits(),
            column_count: self.page.layout.col_count.clamp(0.0, 255.0) as u8,
            scale_bits: self.page.layout.scale.to_bits(),
            page_offset_bits: composition.active_offset_y().to_bits(),
            page_anchor_line: match composition.direction() {
                PaintDirection::Forward => composition.start_line(),
                PaintDirection::Backward => viewport.backward_composition().anchor_end_line(),
            },
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
        let scale = self.page.layout.scale;
        let dx = self.origin.x * (scale - 1.0);
        let dy = self.origin.y * (scale - 1.0);
        Point::new((position.x + dx) / scale, (position.y + dy) / scale)
    }

    /// The inverse of [`Self::to_layout_point`]: layout space back to the
    /// coordinates the surface is drawn in.
    pub fn from_layout_point(&self, position: Point) -> Point {
        let scale = self.page.layout.scale;
        let dx = self.origin.x * (scale - 1.0);
        let dy = self.origin.y * (scale - 1.0);
        Point::new(position.x * scale - dx, position.y * scale - dy)
    }

    pub fn activate_link_cursor(&mut self) {
        if self.page.selection.activate_link_cursor() {
            self.host.request_style();
        }
    }

    pub fn deactivate_link_cursor(&mut self) {
        if self.page.selection.deactivate_link_cursor() {
            self.host.request_style();
        }
    }

    pub(crate) fn emit_operation_failed(&self, operation: RendererOperation, message: String) {
        self.host
            .emit(RendererEvent::OperationFailed { operation, message });
    }

    #[cfg(test)]
    pub fn new(
        host: Rc<dyn RendererHost>,
        loaded: load::LoadedRenderDocument,
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        current_doc_index: usize,
        config: RendererInitialConfig,
    ) -> Self {
        Self::new_with_image_loader(
            host,
            loaded,
            provider,
            document_uris,
            current_doc_index,
            config,
            None,
        )
    }

    fn new_with_image_loader(
        host: Rc<dyn RendererHost>,
        loaded: load::LoadedRenderDocument,
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        current_doc_index: usize,
        config: RendererInitialConfig,
        image_loader: Option<Arc<dyn ImageLoader>>,
    ) -> Self {
        let root_font_size = loaded
            .inputs()
            .expect("loaded document has pipeline inputs")
            .style_environment
            .root_font_size as f32;
        let jobs = host.blocking_job_spawner();
        let image_service = match image_loader {
            Some(loader) => {
                ImageService::with_loader(loader, jobs, ImageService::DEFAULT_BYTE_BUDGET)
            }
            None => ImageService::with_job_spawner(
                provider.clone(),
                jobs,
                ImageService::DEFAULT_BYTE_BUDGET,
            ),
        };
        image_service.set_completion_waker(host.resource_waker());
        let column_width = config.column_width.clamp(200.0, 1200.0).round();
        let layout = ColumnLayout {
            col_width: column_width,
            base_col_width: column_width,
            scale: config.scale.clamp(0.5, 3.0),
            max_col_count: config.max_column_count.map(|count| count.max(1)),
            ..Default::default()
        };
        let mut highlight = HighlightState::default();
        highlight.restart(config.search_query, SearchOptions::default());
        if config.search_active {
            highlight.activate();
        }
        Self {
            host,
            page: SpineItem::new(loaded, layout, image_service.clone()),
            note: None,
            provider,
            image_service,
            root_font_size,
            viewport: ViewportState::default(),
            column_viewport_request: None,
            pending_navigation: None,
            pending_file_events: Default::default(),
            prepared_navigation: None,
            frame_scratch: FrameScratch::default(),
            revisions: RendererRevisions::default(),
            highlight,
            annotations: AnnotationOverlayState::default(),
            media_overlay: MediaOverlayHighlightState::default(),
            nav: {
                let mut nav = NavigationState::new(document_uris, current_doc_index);
                nav.cfi = cfi::PublicationCfi::new(config.publication_cfi_bases);
                nav
            },
            origin: Point::ZERO,
            interaction_palette: config.interaction_palette,
            reader_palette: config.paint_palette,
            vertical_rhythm: config.vertical_rhythm,
            note_display: config.note_display,
        }
    }

    fn pipeline_changes_for_layout(&self, viewport_width: f64) -> PipelineChanges {
        let height = self.page.layout.size.height;
        self.page
            .session
            .configuration_changes(
                DocumentUpdate::default()
                    .width(viewport_width)
                    .height((height.is_finite() && height >= 0.0).then_some(height)),
            )
            .expect("effective column dimensions must be finite and positive")
    }

    fn pipeline_changes_for_style(&self, root_font_size: f32) -> PipelineChanges {
        self.page
            .session
            .configuration_changes(DocumentUpdate::default().font_size(root_font_size.max(1.0)))
            .expect("reader font size must be finite and positive")
    }

    fn apply_pipeline_changes(
        &mut self,
        requested_inputs: PipelineChanges,
        glyph_shaper: &mut impl GlyphShaper,
    ) -> Result<(), html::pipeline::PipelineError> {
        self.apply_pipeline_changes_impl(requested_inputs, glyph_shaper, true)
    }

    fn apply_pipeline_changes_without_repaint(
        &mut self,
        requested_inputs: PipelineChanges,
        glyph_shaper: &mut impl GlyphShaper,
    ) -> Result<(), html::pipeline::PipelineError> {
        self.apply_pipeline_changes_impl(requested_inputs, glyph_shaper, false)
    }

    fn apply_pipeline_changes_impl(
        &mut self,
        requested_inputs: PipelineChanges,
        glyph_shaper: &mut impl GlyphShaper,
        request_repaint: bool,
    ) -> Result<(), html::pipeline::PipelineError> {
        // Before the first frame, `current_glyph_position` resolves to the
        // document's first glyph even when decoration or replaced content
        // precedes it. Relayout at the true document start must preserve the
        // zero offset instead of manufacturing a glyph anchor that scrolls
        // past that leading content.
        let first_glyph = self
            .page
            .render_view()
            .text()
            .line(0)
            .map(|line| line.start());
        let anchor_is_document_start = match (self.nav.anchor_glyph_here(), first_glyph) {
            (None, _) => true,
            (Some(anchor), Some(first)) => anchor == first,
            (Some(_), None) => false,
        };
        let composition = self.viewport.composition();
        let preserve_document_start = anchor_is_document_start
            && matches!(composition.direction(), PaintDirection::Forward)
            && composition.start_offset_y() <= 0.1
            && self
                .page
                .frame
                .page()
                .start_line()
                .is_none_or(|line| line == 0);
        let anchor_glyph = if preserve_document_start {
            None
        } else {
            self.nav
                .anchor_glyph_here()
                .or_else(|| self.nav.current_glyph_position(self.nav_view()))
        };
        let anchor_cfi = if preserve_document_start {
            None
        } else {
            self.nav.current_cfi(self.nav_view())
        };
        let anchor_is_element = !preserve_document_start
            && self
                .nav
                .current_source_position(self.nav_view())
                .is_some_and(|source| source.utf16_offset.is_none());

        self.host.set_font_overrides(
            requested_inputs
                .reader_overrides
                .as_ref()
                .unwrap_or(&self.page.inputs().reader_overrides),
        );
        self.host.set_glyph_document(self.nav.location().document());
        let update = self
            .page
            .session
            .apply_changes(requested_inputs, glyph_shaper)?;
        self.host
            .prepare_font_faces(self.page.session.font_faces(), self.provider.clone());

        self.page.session.document().ok_or_else(|| {
            html::pipeline::PipelineError(
                "pipeline update completed without a laid-out document".to_owned(),
            )
        })?;
        self.root_font_size = self.page.root_font_size();
        if update.stage == EarliestStage::Paint {
            // Paint changes preserve pagination, selection and reading position.
            // Revisions invalidate the displayed colors without discarding geometry.
            self.revisions
                .invalidate_pipeline_from(EarliestStage::Paint);
            if request_repaint {
                self.host.request_repaint();
            }
            return Ok(());
        }
        if update.stage == EarliestStage::None {
            // reset_view_state also clears selection even when all pipeline
            // products were reusable.
            self.revisions.invalidate_overlay();
        } else {
            self.revisions.invalidate_pipeline_from(update.stage);
        }
        self.reset_view_state();
        self.resolve_media_overlay_highlight();
        if anchor_is_element && let Some(cfi) = anchor_cfi.as_deref() {
            self.restore_position_from_cfi(cfi);
        } else if update.anchors_preserved {
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
        self.host.emit(RendererEvent::TitleChanged(
            self.page.render_view().title().map(str::to_owned),
        ));
        self.nav.history.emit_availability(self.host.as_ref());
        self.host.emit(RendererEvent::SearchActiveChanged(
            self.highlight.is_active(),
        ));
        self.highlight.emit_match_signal(self.host.as_ref());
        self.host.emit(RendererEvent::ColumnWidthChanged(
            self.page.layout.base_col_width,
        ));
        self.host
            .emit(RendererEvent::FontSizeChanged(self.root_font_size));
        self.host
            .emit(RendererEvent::ScaleChanged(self.page.layout.scale));
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
            RendererCommand::NavigateDocument(step) => {
                self.navigate_document_jump(glyph_shaper, step)
            }
            RendererCommand::SetPosition { doc, glyph } => {
                self.set_position(glyph_shaper, doc, glyph);
            }
            RendererCommand::NavigateToCfi(cfi) => {
                self.cancel_pending_navigation();
                self.navigate_cfi(glyph_shaper, &cfi, true);
            }
            RendererCommand::SetCfiPosition(Some(cfi)) => {
                self.cancel_pending_navigation();
                self.nav.cancel_jump();
                self.navigate_cfi(glyph_shaper, &cfi, false);
            }
            RendererCommand::SetCfiPosition(None) => {}
            RendererCommand::SetProgressFraction(fraction) => {
                self.set_progress_fraction(glyph_shaper, fraction)
            }
            RendererCommand::SetDocumentTextLengths(lengths) => {
                if self.nav.install_document_text_lengths(lengths) {
                    self.update_nav_signal();
                    self.host.request_repaint();
                }
            }
            RendererCommand::SetColumnWidth(width) => self.set_col_width(glyph_shaper, width),
            RendererCommand::SetColumnViewportPlan {
                available_width,
                column_width,
                column_count,
                column_gap,
            } => self.set_column_viewport_plan(
                glyph_shaper,
                available_width,
                column_width,
                column_count,
                column_gap,
            ),
            RendererCommand::SetMaxColumnCount(count) => {
                let count = count.map(|count| count.max(1));
                if self.page.layout.max_col_count != count {
                    self.page.layout.max_col_count = count;
                    self.page.layout.finish_viewport_reconfiguration(None);
                    self.host.request_repaint();
                }
            }
            RendererCommand::RefreshFonts => {
                let inputs = PipelineChanges {
                    font_environment: Some(self.page.inputs().font_environment.next()),
                    ..Default::default()
                };
                if let Err(error) = self.apply_pipeline_changes(inputs, glyph_shaper) {
                    self.emit_operation_failed(
                        RendererOperation::UpdateDocument,
                        error.to_string(),
                    );
                }
            }
            RendererCommand::SetFontSize(size) => self.set_root_font_size(glyph_shaper, size),
            RendererCommand::SetReaderStyleOverrides(overrides) => {
                if self.page.inputs().reader_overrides != overrides {
                    let inputs = PipelineChanges {
                        reader_overrides: Some(overrides),
                        ..Default::default()
                    };
                    if let Err(error) = self.apply_pipeline_changes(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error.to_string(),
                        );
                    }
                }
            }
            RendererCommand::SetNoteDisplay(display) => {
                if self.note_display != display {
                    self.note_display = display;
                    let inputs = PipelineChanges {
                        note_flow: Some(load::note_flow_for(display)),
                        ..Default::default()
                    };
                    // Note bodies are held back by suppressing their boxes, so
                    // a switch has to rebuild the box tree, not just repaint.
                    self.note = None;
                    self.prepared_navigation = None;
                    if let Err(error) = self.apply_pipeline_changes(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error.to_string(),
                        );
                    }
                }
            }
            RendererCommand::SetImageSizingPolicy(policy) => {
                let mut layout = self.page.inputs().layout;
                if layout.image_sizing_policy != policy {
                    layout.image_sizing_policy = policy;
                    let inputs = PipelineChanges {
                        layout: Some(layout),
                        ..Default::default()
                    };
                    if let Err(error) = self.apply_pipeline_changes(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error.to_string(),
                        );
                    }
                }
            }
            RendererCommand::SetTextCompositionPolicy(policy) => {
                let mut layout = self.page.inputs().layout;
                if layout.text_composition_policy != policy {
                    layout.text_composition_policy = policy;
                    let inputs = PipelineChanges {
                        layout: Some(layout),
                        ..Default::default()
                    };
                    if let Err(error) = self.apply_pipeline_changes(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error.to_string(),
                        );
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
            RendererCommand::SetSearch { query, options } => {
                self.set_search(glyph_shaper, &query, options)
            }
            RendererCommand::SetSearchResults {
                query,
                options,
                results,
            } => self.set_search_results(glyph_shaper, query, options, results),
            RendererCommand::AppendSearchResults {
                query,
                options,
                results,
            } => self.append_search_results(glyph_shaper, query, options, results),
            RendererCommand::NavigateSearch(step) if step > 0 => self.next_match(glyph_shaper),
            RendererCommand::NavigateSearch(_) => self.prev_match(glyph_shaper),
            RendererCommand::NavigateSearchTo(index) => self.select_match(glyph_shaper, index),
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
            RendererCommand::SetReadingTarget(target) => {
                self.media_overlay.set_reading_target(target);
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
                if let Some(text) = self.page.selected_text().filter(|text| !text.is_empty()) {
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

    pub(crate) fn set_position(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        document: usize,
        glyph: Option<u32>,
    ) {
        if document != self.nav.location().document() {
            self.load_document_at(
                glyph_shaper,
                document,
                glyph.map(DocAnchor::Glyph).unwrap_or(DocAnchor::Start),
            );
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
        self.nav
            .toc_anchors
            .set_strings_by_doc(anchors_by_doc, document_count);
        let view = html_view_navigate::NavView {
            doc: DocQuery::from_view(self.page.render_view()),
            viewport: &self.viewport,
            frame: &self.page.frame,
        };
        self.nav.update_toc_anchor_filter(view);
    }

    pub fn set_search_query(&mut self, query: &str) {
        let options = self.highlight.current_document_options();
        self.highlight.restart(query.to_owned(), options);
        self.highlight.set_local_matches(
            self.nav.location().document(),
            DocQuery::from_view(self.page.render_view()),
        );
        let empty = self.highlight.matches().is_empty();
        self.highlight.emit_results(self.host.as_ref());
        if empty {
            self.request_overlay_repaint();
        } else {
            self.navigate_to_current_match();
        }
    }

    /// Finds and highlights matches without moving the reader.
    ///
    /// Setting a query is not navigation: the reader is still typing, and
    /// jumping the page on each keystroke moves the text out from under them.
    /// Position changes only through `NavigateSearch`, which the search bar's
    /// buttons and Enter drive.
    pub fn set_search(
        &mut self,
        _glyph_shaper: &mut impl GlyphShaper,
        query: &str,
        options: SearchOptions,
    ) {
        self.highlight.restart(query.to_owned(), options);
        let local_scope = options.scope == SearchScope::CurrentDocument;
        if query.is_empty() {
            // restart already cleared both result sets.
        } else if local_scope {
            self.highlight.set_local_matches(
                self.nav.location().document(),
                DocQuery::from_view(self.page.render_view()),
            );
        } else {
            let results = search_publication_with_cfi(
                self.provider.as_ref(),
                self.nav.documents().uris(),
                &self.nav.cfi,
                query,
                options,
                || false,
                |_| {},
            );
            self.highlight
                .set_results(query.to_owned(), options, results);
            self.refresh_visible_search_matches();
        }
        self.highlight.emit_results(self.host.as_ref());
        self.request_overlay_repaint();
    }

    /// Installs results found off the main thread. Highlights only, for the same
    /// reason as [`Self::set_search`].
    pub fn set_search_results(
        &mut self,
        _glyph_shaper: &mut impl GlyphShaper,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) {
        self.highlight.set_results(query, options, results);
        self.refresh_visible_search_matches();
        self.request_overlay_repaint();
        self.highlight.emit_results(self.host.as_ref());
    }

    /// Adds results from a streaming scan. Highlights only, for the same reason
    /// as [`Self::set_search`]: results arriving late must not yank the page.
    pub fn append_search_results(
        &mut self,
        _glyph_shaper: &mut impl GlyphShaper,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) {
        self.highlight.append_results(query, options, results);
        self.refresh_visible_search_matches();
        self.request_overlay_repaint();
        self.highlight.emit_results(self.host.as_ref());
    }

    fn navigate_to_book_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let Some(hit) = self.highlight.current_book_match() else {
            return;
        };
        if self.defer_document_read(
            hit.doc,
            nav::NavigationRequest::Search(self.highlight.query().to_owned()),
        ) {
            return;
        }
        let from = self.nav.current_cfi(self.nav_view());
        if hit.doc != self.nav.location().document() {
            self.set_position(glyph_shaper, hit.doc, None);
        }
        if self.refresh_visible_search_matches() {
            if let Some(from) = from {
                self.nav.begin_jump(from);
            }
            self.navigate_to_current_match();
        }
    }

    /// Projects search results onto the loaded document without reading another
    /// file or changing the viewport. Navigation uses the same source mapping.
    fn refresh_visible_search_matches(&mut self) -> bool {
        self.highlight
            .refresh_visible_matches(DocQuery::from_view(self.page.render_view()));
        let Some(hit) = self
            .highlight
            .current_book_match()
            .filter(|hit| hit.doc == self.nav.location().document())
        else {
            return false;
        };
        let occurrence = match hit.target {
            BookSearchTarget::LocalOccurrence(occurrence) => Some(occurrence),
            BookSearchTarget::SourceCfi(cfi) => self
                .nav
                .cfi
                .to_internal(&cfi)
                .and_then(|cfi| crate::cfi::parse_cfi(&self.page, &cfi))
                .and_then(|(doc, glyph)| (doc == self.nav.location().document()).then_some(glyph))
                .and_then(|glyph| {
                    self.highlight
                        .matches()
                        .iter()
                        .position(|range| range.contains(&glyph))
                }),
        };
        let Some(occurrence) = occurrence else {
            return false;
        };
        self.highlight.focus_local_match(occurrence);
        true
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

    pub fn select_match(&mut self, glyph_shaper: &mut impl GlyphShaper, index: usize) {
        if self.highlight.select_match(index) {
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
