#[cfg(test)]
use html_engine_model::page_placements;
use html::configuration::{DocumentUpdate, Update};
#[cfg(test)]
use html::layout::LineId;
use html::layout::RenderView;
use html::pagination::PageRequest;
use std::mem::discriminant;
use std::sync::Arc;

use html_view_navigate::NavigationState;
use html_view_paint::{AnnotationOverlayState, HighlightState, MediaOverlayHighlightState};
pub use html::selection::SelectionMode;
/// Shared reader gesture threshold, also used by shell click recognition.
pub const WORD_SELECTION_DRAG_DISTANCE: f32 = 4.0;
use html_view_types::{
    ColumnLayout, DocAnchor, DocumentGlyph, DocumentImage, ViewportState, VisibleFrame,
};
pub use html_view_types::{
    AnnotationStyle, BookSearchMatch, BookSearchResults, BookSearchTarget, ClipboardWrite, Color,
    DisplayCacheKey, FontLoadRequest, FootnotePopupAnchor, FootnotePreview, FrameDisplayCacheKeys,
    FrameGeometryCacheKey, InteractionPalette, NoteDisplay, OpenedImage, PopupAnchorCorner, ReaderPaintPalette,
    ReadingPosition, RendererAnnotation, RendererCommand, RendererEffects, RendererEvent,
    RendererInitialConfig, RendererOperation, RendererRevisions, RendererServices, SearchOptions,
    SearchScope, SourcePosition,
};
use kurbo::Point;
#[cfg(test)]
use kurbo::Rect;

use crate::spine_item::SpineItem;
use html::{Document, PreparedDocument};
use html::text::GlyphShaper;
use html::Error;
pub use html::{
    document::RootFontSize,
    layout::{ImageSizingPolicy, TextCompositionPolicy},
    style::ReaderStyleOverrides,
};
pub use html::resources::{
    BlockingJob, BlockingJobSpawner, FileSystemProvider, InlineJobSpawner, ResourceMetadata,
    ResourceProvider,
};
use html::resources::{ImageLoader, ImageService};
use html::search::SearchIndex;
pub use html::search::{TextPatternError, TextPatternMatcher};
pub use html::text::FontFace;

pub use html_view_cfi as cfi;

mod document_view;
mod spine_item;

/// Renderer-neutral text backend contract. UI adapters should use this module
/// rather than importing layout or CSS model types.
pub mod text_backend {
    pub use html::text::{
        FontMetrics, FontSlant, GlyphHandle, ShapedRun, TextRunShapeRequest,
        GlyphResourceGeneration, GlyphShaper, ShapedGlyph,
    };
}

pub use html::reading::{ReadingFragment, ReadingTarget};
use html::render::Painter;

mod session;
pub use session::{
    ContentHit, FramePainter, PointerDownOptions, PointerDownOutcome, PointerMoveOutcome,
    PreparedFrame, ReadingLine, RendererSession, VisibleTextMatch, VisibleTextMatches,
};

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

#[derive(Default)]
struct FrameScratch {
    pagination: html::pagination::PaginationScratch,
    overlay: html_view_paint::OverlayScratch,
    note_selection: html::selection::SelectionPaintScratch,
}

#[cfg(test)]
mod frame_cache_tests {
    use super::*;
    use html_view_types::LineScreen;

    #[test]
    fn visible_line_positions_keep_global_indexes_and_reuse_capacity() {
        let mut buffer = Vec::with_capacity(32);
        buffer.push(LineScreen {
            point: Point::new(0.0, 0.0),
            col_index: 0,
            visible_y: None,
        });
        let capacity = buffer.capacity();
        let mut positions = page_placements::with_buffer(40, buffer);
        page_placements::push(
            &mut positions,
            LineId::new(40),
            LineScreen {
                point: Point::new(10.0, 20.0),
                col_index: 1,
                visible_y: None,
            },
        );

        assert!(page_placements::at_cursor(&positions, 40).is_some());
        assert!(page_placements::at_cursor(&positions, 41).is_none());
        assert_eq!(
            positions
                .get(LineId::new(40))
                .map(|screen| screen.col_index),
            Some(1)
        );
        assert!(positions.get(LineId::new(39)).is_none());

        let mut frame = VisibleFrame::default();
        frame.install_page(html::pagination::PagePositions {
            positions,
            start_line: 40,
            last_line: Some(40),
            ..Default::default()
        });
        frame.clear();
        let reused = frame.take_line_positions().into_buffer();
        assert!(reused.capacity() >= capacity);
    }
}

/// The provider-backed, parse-and-style portion of initial renderer creation.
/// Fresh preparation contains no UI framework objects or platform text handles
/// and can run on a worker. Suspended documents can also retain completed
/// geometry and glyph products until resumed with a compatible backend.
pub struct RendererPreparation {
    loaded: PreparedDocument,
    provider: Arc<dyn ResourceProvider>,
    image_loader: Option<Arc<dyn ImageLoader>>,
    document_uris: Vec<String>,
    doc_index: usize,
    glyph_index: u32,
    nav_state: Option<String>,
    initial_anchor: Option<String>,
    config: RendererInitialConfig,
    font_revision: Option<Arc<()>>,
    refresh_fonts: bool,
}

pub(crate) struct RendererCore {
    services: RendererServices,
    effects: RendererEffects,
    note_popup_width: Option<f64>,
    /// The spine item being read: the document, what is on screen of it, and
    /// the pipeline and resources addressed by it.
    page: SpineItem,
    /// The note currently shown beside the page, if any. Selecting and hit
    /// testing work here through the same code the page uses.
    note: Option<document_view::OpenNote>,
    provider: Arc<dyn ResourceProvider>,
    /// One source-keyed worker pool and decoded-image budget for the book.
    image_service: ImageService,
    viewport: ViewportState,
    column_viewport_request: Option<nav::ColumnViewportRequest>,
    pending_navigation: Option<nav::PendingNavigation>,
    pending_file_events: Vec<std::sync::mpsc::Receiver<Result<RendererEvent, html::Error>>>,
    prepared_navigation: Option<(usize, Document)>,
    frame_scratch: FrameScratch,
    revisions: RendererRevisions,
    highlight: HighlightState,
    annotations: AnnotationOverlayState,
    media_overlay: MediaOverlayHighlightState,
    nav: NavigationState,
    interaction_palette: InteractionPalette,
    reader_palette: ReaderPaintPalette,
    vertical_rhythm: bool,
    /// Presentation policy for note bodies. Held here rather than derived from
    /// `pipeline_inputs` because it decides what goes *into* those inputs, and
    /// every subsequent document load has to agree with the first.
    note_display: NoteDisplay,
}

impl RendererCore {
    pub(crate) fn doc(&self) -> RenderView<'_> {
        self.page.render_view()
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
        let request = self.viewport.request();
        FrameGeometryCacheKey {
            layout_revision: self.revisions.layout,
            viewport_width_bits: self.page.layout.size.width.to_bits(),
            viewport_height_bits: self.page.layout.size.height.to_bits(),
            column_width_bits: self.page.layout.col_width.to_bits(),
            column_gap_bits: self.page.layout.col_gap.to_bits(),
            column_count: self.page.layout.col_count.clamp(0.0, 255.0) as u8,
            scale_bits: self.page.layout.scale.to_bits(),
            page_request: (
                discriminant(&request),
                match request {
                    PageRequest::StartingAt(cursor) | PageRequest::EndingAt(cursor) => {
                        cursor as u64
                    }
                    PageRequest::StartingAtOffset(offset) | PageRequest::EndingAtOffset(offset) => {
                        offset.to_bits()
                    }
                },
            ),
        }
    }

    fn request_base_repaint(&mut self) {
        self.revisions.invalidate_base();
        self.effects.request_repaint();
    }

    fn request_overlay_repaint(&mut self) {
        self.revisions.invalidate_overlay();
        self.effects.request_repaint();
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette) {
        if self.interaction_palette == palette {
            return;
        }
        self.interaction_palette = palette;
        self.request_overlay_repaint();
    }

    fn to_layout_point(&self, position: Point) -> Point {
        let scale = self.page.layout.scale;
        Point::new(position.x / scale, position.y / scale)
    }

    fn from_layout_point(&self, position: Point) -> Point {
        let scale = self.page.layout.scale;
        Point::new(position.x * scale, position.y * scale)
    }

    pub(crate) fn emit_operation_failed(&mut self, operation: RendererOperation, message: impl Into<Error>) {
        let message: Error = message.into();
        self.effects
            .emit(RendererEvent::OperationFailed { operation, message: Arc::new(message) });
    }

    #[cfg(test)]
    pub fn new(
        services: RendererServices,
        loaded: Document,
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        current_doc_index: usize,
        config: RendererInitialConfig,
    ) -> Self {
        Self::new_with_image_loader(
            services,
            loaded,
            provider,
            document_uris,
            current_doc_index,
            config,
            None,
        )
    }

    fn new_with_image_loader(
        services: RendererServices,
        loaded: Document,
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        current_doc_index: usize,
        config: RendererInitialConfig,
        image_loader: Option<Arc<dyn ImageLoader>>,
    ) -> Self {
        let image_service = Self::create_image_service(&services, provider.clone(), image_loader);
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
            services,
            effects: RendererEffects::default(),
            note_popup_width: None,
            page: SpineItem::new(loaded, layout, image_service.clone()),
            note: None,
            provider,
            image_service,
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
            interaction_palette: config.interaction_palette,
            reader_palette: config.paint_palette,
            vertical_rhythm: config.vertical_rhythm,
            note_display: config.note_display,
        }
    }

    fn create_image_service(
        services: &RendererServices,
        provider: Arc<dyn ResourceProvider>,
        image_loader: Option<Arc<dyn ImageLoader>>,
    ) -> ImageService {
        let jobs = services.blocking_jobs.clone();
        let image_service = match image_loader {
            Some(loader) => {
                ImageService::with_loader(loader, jobs, ImageService::DEFAULT_BYTE_BUDGET)
            }
            None => ImageService::with_job_spawner(
                provider,
                jobs,
                ImageService::DEFAULT_BYTE_BUDGET,
            ),
        };
        image_service.set_completion_waker(services.resource_waker.clone());
        image_service
    }

    fn document_update_for_layout(&self, viewport_width: f64) -> DocumentUpdate {
        // Keep document geometry continuous; page breaks belong to viewport composition.
        DocumentUpdate::default()
            .width(viewport_width)
            .height(None)
    }

    fn document_update_for_typography(&self, root_font_size: f32) -> DocumentUpdate {
        DocumentUpdate::default().font_size(root_font_size.max(1.0))
    }

    fn apply_document_update(
        &mut self,
        requested_inputs: DocumentUpdate,
        glyph_shaper: &mut impl GlyphShaper,
    ) -> Result<(), Error> {
        self.apply_document_update_impl(requested_inputs, glyph_shaper, true)
    }

    fn apply_document_update_without_repaint(
        &mut self,
        requested_inputs: DocumentUpdate,
        glyph_shaper: &mut impl GlyphShaper,
    ) -> Result<(), Error> {
        self.apply_document_update_impl(requested_inputs, glyph_shaper, false)
    }

    fn apply_document_update_impl(
        &mut self,
        requested_inputs: DocumentUpdate,
        glyph_shaper: &mut impl GlyphShaper,
        request_repaint: bool,
    ) -> Result<(), Error> {
        let anchor = self.nav.reflow_anchor(self.nav_view());
        let previous_font_faces = self.page.session.font_faces();
        let previously_used_publication_fonts = self
            .page
            .configuration()
            .reader_overrides()
            .uses_publication_fonts();
        let before = self.page.session.revisions();
        let update = self.page.session.apply(requested_inputs, glyph_shaper)?;
        let after = self.page.session.revisions();
        let faces = self.page.session.font_faces();
        let use_publication_fonts = self
            .page
            .configuration()
            .reader_overrides()
            .uses_publication_fonts();
        if use_publication_fonts != previously_used_publication_fonts
            || (!Arc::ptr_eq(&faces, &previous_font_faces) && faces != previous_font_faces)
        {
            self.effects.font_loads.push(FontLoadRequest {
                faces,
                use_publication_fonts,
                provider: self.provider.clone(),
            });
        }

        self.page.session.render_view().ok_or_else(|| {
            Error::message("pipeline update completed without a laid-out document")
        })?;
        if update == Update::Paint {
            // Paint changes preserve pagination, selection and reading position.
            // Revisions invalidate the displayed colors without discarding geometry.
            self.revisions.update_document(before, after);
            if request_repaint {
                self.effects.request_repaint();
            }
            return Ok(());
        }
        if update == Update::Unchanged {
            return Ok(());
        }
        self.revisions.update_document(before, after);
        let anchor_glyph = anchor.glyph;
        let anchor_is_element = anchor
            .source
            .as_ref()
            .is_some_and(|source| source.utf16_offset.is_none());
        let anchor_cfi = anchor.source.and_then(|source| {
            self.nav
                .cfi
                .source_position_cfi(&html_view_types::SourcePosition {
                    document: self.nav.current_document(),
                    path: source.path,
                    utf16_offset: source.utf16_offset,
                })
        });

        self.reset_view_state();
        self.resolve_media_overlay_highlight();
        if matches!(
            update,
            Update::Layout {
                anchors_preserved: false
            }
        ) {
            self.resolve_visible_annotations();
            self.refresh_visible_search_matches();
        }
        if anchor_is_element && let Some(cfi) = anchor_cfi.as_deref() {
            self.restore_position_from_cfi(cfi);
        } else if matches!(
            update,
            Update::Layout {
                anchors_preserved: true
            }
        ) {
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
            self.effects.request_repaint();
        }
        Ok(())
    }

    pub fn emit_state_snapshot(&mut self) {
        self.effects.emit(RendererEvent::TitleChanged(
            self.page.render_view().title().map(str::to_owned),
        ));
        self.effects.emit(RendererEvent::SearchActiveChanged(
            self.highlight.is_active(),
        ));
        self.highlight.emit_match_signal(&mut self.effects);
        self.effects.emit(RendererEvent::ColumnWidthChanged(
            self.page.layout.base_col_width,
        ));
        self.effects
            .emit(RendererEvent::FontSizeChanged(self.page.root_font_size()));
        self.effects
            .emit(RendererEvent::ScaleChanged(self.page.layout.scale));
        self.nav.emit_reading_position_snapshot(&mut self.effects);
    }

    pub fn apply(&mut self, glyph_shaper: &mut impl GlyphShaper, command: RendererCommand) {
        match command {
            RendererCommand::NavigateToHref(href) if !href.is_empty() => {
                self.navigate_to_href(glyph_shaper, &href);
            }
            RendererCommand::NavigateToHref(_) => {}
            RendererCommand::NavigateDocument(step) => {
                self.navigate_document_jump(glyph_shaper, step)
            }
            RendererCommand::NavigatePage(step) => {
                if step > 0 {
                    self.next_page(glyph_shaper);
                } else if step < 0 {
                    self.prev_page(glyph_shaper);
                }
            }
            RendererCommand::NavigateLine(step) => {
                if step > 0 {
                    self.next_line(glyph_shaper);
                } else if step < 0 {
                    self.prev_line(glyph_shaper);
                }
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
                    self.effects.request_repaint();
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
                    self.page.layout.recompute_columns();
                    self.effects.request_repaint();
                }
            }
            RendererCommand::RefreshFonts => {
                let inputs = DocumentUpdate::default().fonts_changed();
                if let Err(error) = self.apply_document_update(inputs, glyph_shaper) {
                    self.emit_operation_failed(
                        RendererOperation::UpdateDocument,
                        error,
                    );
                }
            }
            RendererCommand::SetFontSize(size) => self.set_root_font_size(glyph_shaper, size),
            RendererCommand::SetReaderStyleOverrides(overrides) => {
                if self.page.configuration().reader_overrides() != &overrides {
                    let inputs = DocumentUpdate::default().reader_overrides(overrides);
                    if let Err(error) = self.apply_document_update(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error,
                        );
                    }
                }
            }
            RendererCommand::SetNoteDisplay(display) => {
                if self.note_display != display {
                    let inputs = DocumentUpdate::default().note_flow(load::note_flow_for(display));
                    match self.apply_document_update(inputs, glyph_shaper) {
                        Ok(()) => {
                            self.note_display = display;
                            self.note = None;
                            self.prepared_navigation = None;
                        }
                        Err(error) => self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error,
                        ),
                    }
                }
            }
            RendererCommand::SetImageSizingPolicy(policy) => {
                if self.page.configuration().image_sizing_policy() != policy {
                    let inputs = DocumentUpdate::default().image_sizing_policy(policy);
                    if let Err(error) = self.apply_document_update(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error,
                        );
                    }
                }
            }
            RendererCommand::SetTextCompositionPolicy(policy) => {
                if self.page.configuration().text_composition_policy() != policy {
                    let inputs = DocumentUpdate::default().text_composition_policy(policy);
                    if let Err(error) = self.apply_document_update(inputs, glyph_shaper) {
                        self.emit_operation_failed(
                            RendererOperation::UpdateDocument,
                            error,
                        );
                    }
                }
            }
            RendererCommand::SetReaderPaintPalette(palette) => {
                if self.reader_palette != palette {
                    self.reader_palette = palette;
                    self.request_base_repaint();
                    if self.page.selection.link_hovered() {
                        self.request_overlay_repaint();
                    }
                }
            }
            RendererCommand::SetScale(scale) => self.set_scale(scale),
            RendererCommand::ActivateSearch => self.activate_search(),
            RendererCommand::DeactivateSearch => self.deactivate_search(),
            RendererCommand::SetSearchQuery(query) => self.set_search_query(query),
            RendererCommand::SetSearch { query, options } => self.set_search(&query, options),
            RendererCommand::SetSearchResults {
                query,
                options,
                results,
            } => self.set_search_results(query, options, results),
            RendererCommand::AppendSearchResults {
                query,
                options,
                results,
            } => self.append_search_results(query, options, results),
            RendererCommand::NavigateSearch(step) if step > 0 => self.next_match(glyph_shaper),
            RendererCommand::NavigateSearch(_) => self.prev_match(glyph_shaper),
            RendererCommand::NavigateSearchTo(index) => self.select_match(glyph_shaper, index),
            RendererCommand::SetAnnotations(annotations) => {
                self.annotations.replace(annotations);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::SetActiveAnnotation(id) => {
                self.annotations.set_active(id);
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
                self.copy_selection_to_clipboard();
            }
            RendererCommand::CopySelectionCitation { title, cfi } => {
                if let Some(text) = self.page.selected_text().filter(|text| !text.is_empty()) {
                    let citation = match cfi {
                        Some(cfi) => format!("“{text}” — {title}, {cfi}"),
                        None => format!("“{text}” — {title}"),
                    };
                    self.effects.clipboard.push(ClipboardWrite::Text(citation));
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
        if document != self.nav.current_document() {
            self.load_document_at(
                glyph_shaper,
                document,
                glyph.map(DocAnchor::Glyph).unwrap_or(DocAnchor::Start),
            );
        } else if let Some(glyph) = glyph {
            self.restore_glyph_position(glyph);
            self.effects.request_repaint();
        }
    }

    pub(crate) fn set_cfi_position(&mut self, cfi: &str) -> bool {
        let restored = self.restore_position_from_cfi(cfi);
        if restored {
            self.effects.request_repaint();
        }
        restored
    }

    pub(crate) fn activate_search(&mut self) {
        if !self.highlight.activate() {
            return;
        }
        self.effects.emit(RendererEvent::SearchActiveChanged(true));
        self.request_overlay_repaint();
    }

    pub(crate) fn deactivate_search(&mut self) {
        if !self.highlight.deactivate() {
            return;
        }
        self.effects.emit(RendererEvent::SearchActiveChanged(false));
        self.set_search_query(String::new());
    }

    fn set_search_query(&mut self, query: String) {
        let options = self.highlight.current_document_options();
        self.highlight.restart(query, options);
        self.highlight
            .set_local_matches(self.nav.current_document(), self.page.render_view());
        let empty = self.highlight.matches().is_empty();
        self.highlight.emit_results(&mut self.effects);
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
    pub fn set_search(&mut self, query: &str, options: SearchOptions) {
        self.highlight.restart(query.to_owned(), options);
        let local_scope = options.scope == SearchScope::CurrentDocument;
        if query.is_empty() {
            // restart already cleared both result sets.
        } else if local_scope {
            self.highlight
                .set_local_matches(self.nav.current_document(), self.page.render_view());
        } else {
            let results = search_publication_with_cfi(
                self.provider.as_ref(),
                self.nav.document_uris(),
                &self.nav.cfi,
                query,
                options,
                || false,
                |_| {},
            );
            self.highlight.set_results(results);
            self.refresh_visible_search_matches();
        }
        self.highlight.emit_results(&mut self.effects);
        self.request_overlay_repaint();
    }

    /// Installs results found off the main thread. Highlights only, for the same
    /// reason as [`Self::set_search`].
    pub fn set_search_results(
        &mut self,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) {
        self.highlight.restart(query, options);
        self.highlight.set_results(results);
        self.highlight
            .refresh_visible_matches(self.nav.current_document(), self.page.render_view());
        self.focus_current_search_match();
        self.request_overlay_repaint();
        self.highlight.emit_results(&mut self.effects);
    }

    /// Adds results from a streaming scan. Highlights only, for the same reason
    /// as [`Self::set_search`]: results arriving late must not yank the page.
    pub fn append_search_results(
        &mut self,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) {
        self.highlight.append_results(
            query,
            options,
            results,
            self.page.render_view(),
            &mut self.effects,
        );
        self.focus_current_search_match();
        self.request_overlay_repaint();
        self.highlight.emit_match_signal(&mut self.effects);
    }

    fn navigate_to_book_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let Some(hit) = self.highlight.current_book_match() else {
            return;
        };
        let doc = hit.doc;
        if self.defer_document_read(
            doc,
            nav::NavigationRequest::Search(self.highlight.query().to_owned()),
        ) {
            return;
        }
        let from = self.nav.current_cfi(self.nav_view());
        if doc != self.nav.current_document() {
            self.set_position(glyph_shaper, doc, None);
            if self.nav.current_document() != doc {
                return;
            }
        }
        if self.focus_current_search_match() {
            if let Some(from) = from {
                self.nav.begin_jump(from);
            }
            self.navigate_to_current_match();
        }
    }

    /// Projects search results onto the loaded document without reading another
    /// file or changing the viewport. Navigation uses the same source mapping.
    fn refresh_visible_search_matches(&mut self) {
        self.highlight
            .refresh_visible_matches(self.nav.current_document(), self.page.render_view());
        self.focus_current_search_match();
        if self.highlight.options().scope == SearchScope::CurrentDocument {
            self.highlight.emit_results(&mut self.effects);
        }
    }

    fn focus_current_search_match(&mut self) -> bool {
        let Some(hit) = self
            .highlight
            .current_book_match()
            .filter(|hit| hit.doc == self.nav.current_document())
        else {
            return false;
        };
        let occurrence = match &hit.target {
            BookSearchTarget::LocalOccurrence(occurrence) => Some(*occurrence),
            BookSearchTarget::SourceCfi(cfi) => self
                .nav
                .cfi
                .to_internal(cfi)
                .and_then(|cfi| crate::cfi::parse_cfi(&self.page, &cfi))
                .and_then(|(doc, glyph)| (doc == self.nav.current_document()).then_some(glyph))
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
        self.highlight.emit_match_signal(&mut self.effects);
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

mod load;
