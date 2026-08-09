use std::collections::HashSet;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kurbo::{Point, Size};
pub use peniko::Color;

use crate::document_cache::{DocumentCache, FootnoteDocumentIndex};
use crate::navigation::{LocationHistory, NavSignals, TocAnchors};
use crate::prepared_page::PreparedPageCache;
use html::engine::Engine;
use html::layout::GlyphShaper;
pub use html::layout::{DocumentTocNode, TextDecorationLines, UsedBorderRadii};
use html::pipeline::{EarliestStage, PipelineInputs};
pub use html::pipeline::{ImageSizingPolicy, RootFontSize, TextCompositionPolicy};
use html::resources::ImagePipeline;
pub use html::resources::{FileSystemProvider, ResourceMetadata, ResourceProvider, TocEntry};

pub use html_view_cfi as cfi;
mod doc_query;
mod document_cache;
mod navigation;
#[cfg(test)]
mod test_support;

#[path = "renderer/navigate.rs"]
mod navigate;
mod prepared_page;

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
pub use html::render::{Painter, RecordingPainter};

mod session;
pub use session::{FramePainter, PointerDownOptions, PointerMoveOutcome, PreparedFrame, RendererSession};

const DEFAULT_DOCUMENT_CACHE_CAPACITY: usize = 3;

fn visible_text(source: &str) -> String {
    html::pipeline::plain_text_from_fragment(source)
}

fn visible_text_length(source: &str) -> u64 {
    visible_text(source).chars().filter(|character| !character.is_whitespace()).count() as u64
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
        let Ok(source) = provider.read_string(uri) else { continue };
        let text = visible_text(&source);
        let chars: Vec<char> = text.chars().collect();
        for (occurrence, (start, end)) in highlight::find_text_matches(&text, query, options).into_iter().enumerate() {
            if cancelled() || results.len() >= MAX_RESULTS {
                break;
            }
            let excerpt_start = start.saturating_sub(42);
            let excerpt_end = (end + 70).min(chars.len());
            let excerpt = chars[excerpt_start..excerpt_end].iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
            results.push(BookSearchMatch { doc, occurrence, excerpt });
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
    document_uris.iter().map(|uri| provider.read_string(uri).map(|source| visible_text_length(&source)).unwrap_or(1).max(1)).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationStyle {
    Highlight,
    Underline,
    Squiggly,
    Strikethrough,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererAnnotation {
    pub id: String,
    pub cfi_range: String,
    pub exact_text: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub style: AnnotationStyle,
    pub color: [u8; 4],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderPaintPalette {
    pub foreground: Option<u32>,
    pub background: Option<u32>,
}

/// Renderer interaction colors expressed in the same renderer-neutral color
/// type used by the painting contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InteractionPalette {
    pub selection: Color,
    pub search_match: Color,
    pub active_search_match: Color,
    pub annotation: Color,
    pub active_annotation: Color,
}

impl Default for InteractionPalette {
    fn default() -> Self {
        Self {
            selection: Color::rgba8(151, 188, 159, 112),
            search_match: Color::rgba8(232, 193, 78, 116),
            active_search_match: Color::rgba8(190, 116, 55, 164),
            annotation: Color::rgba8(236, 204, 91, 104),
            active_annotation: Color::rgba8(196, 126, 57, 148),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RendererInitialConfig {
    pub font_size: f32,
    pub column_width: f64,
    pub scale: f64,
    /// Optional presentation cap. `None` lets the viewport determine the
    /// number of columns from the requested column width.
    pub max_column_count: Option<u8>,
    pub style_overrides: html::pipeline::ReaderStyleOverrides,
    pub image_sizing_policy: html::pipeline::ImageSizingPolicy,
    pub text_composition_policy: html::pipeline::TextCompositionPolicy,
    /// Opt-in pagination-only spacing refinement. This never changes DOM/CSS
    /// layout; it only distributes bounded page slack over existing block gaps.
    pub vertical_rhythm: bool,
    pub paint_palette: ReaderPaintPalette,
    pub interaction_palette: InteractionPalette,
    pub search_active: bool,
    pub search_query: String,
}

impl Default for RendererInitialConfig {
    fn default() -> Self {
        Self {
            font_size: 20.0,
            column_width: 600.0,
            scale: 1.0,
            max_column_count: None,
            style_overrides: Default::default(),
            image_sizing_policy: html::pipeline::ImageSizingPolicy::SmartStandalone,
            text_composition_policy: html::pipeline::TextCompositionPolicy::BookOptimized,
            vertical_rhythm: false,
            paint_palette: Default::default(),
            interaction_palette: Default::default(),
            search_active: false,
            search_query: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FootnotePreview {
    pub href: String,
    pub blocks: Vec<String>,
}

#[derive(Default)]
struct AnnotationOverlayState {
    source: Vec<RendererAnnotation>,
    visible: Vec<(String, u32, u32, AnnotationStyle, [u8; 4])>,
}

#[derive(Default)]
struct MediaOverlayHighlightState {
    target_href: Option<String>,
    visible: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub enum RendererCommand {
    NavigateToHref(String),
    NavigateHistory(i8),
    NavigateDocument(i8),
    SetPosition { doc: usize, glyph: Option<u32> },
    SetCfiPosition(Option<String>),
    SetProgressFraction(f32),
    SetDocumentTextLengths(Vec<u64>),
    SetColumnWidth(f64),
    SetMaxColumnCount(Option<u8>),
    SetFontSize(f32),
    SetReaderStyleOverrides(html::pipeline::ReaderStyleOverrides),
    SetImageSizingPolicy(html::pipeline::ImageSizingPolicy),
    SetTextCompositionPolicy(html::pipeline::TextCompositionPolicy),
    SetReaderPaintPalette(ReaderPaintPalette),
    SetScale(f64),
    SetSearchActive(bool),
    SetSearchQuery(String),
    SetSearch { query: String, options: SearchOptions },
    SetSearchResults { query: String, options: SearchOptions, results: Vec<BookSearchMatch> },
    AppendSearchResults { query: String, options: SearchOptions, results: Vec<BookSearchMatch> },
    NavigateSearch(i8),
    SetAnnotations(Vec<RendererAnnotation>),
    UpsertAnnotation(RendererAnnotation),
    RemoveAnnotation(String),
    SetMediaOverlayTarget(Option<String>),
    RequestVisibleText,
    CopySelection,
    CopySelectionCitation { title: String, cfi: Option<String> },
}

#[derive(Clone, Debug)]
pub enum RendererEvent {
    PositionChanged { doc: usize, glyph: Option<u32> },
    CfiChanged(Option<String>),
    NavAnchorChanged { doc: usize, anchor: Option<String> },
    TitleChanged(Option<String>),
    TocChanged(Vec<TocEntry>),
    MatchInfo { current: usize, total: usize },
    SearchResults(Vec<BookSearchMatch>),
    HistoryAvailability { back: bool, forward: bool },
    SearchActiveChanged(bool),
    ColumnWidthChanged(f64),
    FontSizeChanged(f32),
    ScaleChanged(f64),
    ReadingProgress { fraction: f32, location: u64, total_locations: u64, doc: usize, doc_count: usize },
    SelectionFinished { doc: usize, cfi_range: String, exact_text: String, prefix: Option<String>, suffix: Option<String> },
    AnnotationActivated { id: String },
    FootnoteOpened(FootnotePreview),
    SpeakableText(String),
    ImageOpened { uri: String, bytes: Vec<u8> },
    OperationFailed { operation: RendererOperation, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererOperation {
    CopySelection,
    CopyImage,
    UpdateDocument,
}

impl fmt::Display for RendererOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CopySelection => "copy selection",
            Self::CopyImage => "copy image",
            Self::UpdateDocument => "update document rendering",
        })
    }
}

pub trait RendererHost {
    fn request_repaint(&self);
    fn request_style(&self);
    fn schedule(&self, delay: Duration, f: Box<dyn FnOnce() + Send>);
    fn schedule_repaint(&self, delay: Duration);
    fn set_clipboard(&self, text: &str) -> Result<(), String>;
    fn set_clipboard_image(&self, width: usize, height: usize, rgba: Vec<u8>) -> Result<(), String>;
    fn set_clipboard_svg(&self, _bytes: Vec<u8>) -> Result<(), String> {
        Err("copying SVG images is not supported by this renderer host".to_owned())
    }
    fn emit(&self, event: RendererEvent);
}

#[derive(Clone, Copy, PartialEq)]
pub enum PaintDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererRevisions {
    pub document: u64,
    pub style: u64,
    pub shape: u64,
    pub layout: u64,
    pub base_display: u64,
    pub overlay: u64,
    pub resources: u64,
}

impl Default for RendererRevisions {
    fn default() -> Self {
        Self { document: 1, style: 1, shape: 1, layout: 1, base_display: 1, overlay: 1, resources: 1 }
    }
}

impl RendererRevisions {
    fn next(value: &mut u64) {
        *value = value.wrapping_add(1).max(1);
    }

    fn invalidate_pipeline_from(&mut self, stage: EarliestStage) {
        match stage {
            EarliestStage::Parse => {
                Self::next(&mut self.document);
                Self::next(&mut self.style);
                Self::next(&mut self.shape);
                Self::next(&mut self.layout);
            }
            EarliestStage::Style | EarliestStage::Prepare => {
                Self::next(&mut self.style);
                Self::next(&mut self.shape);
                Self::next(&mut self.layout);
            }
            EarliestStage::Shape => {
                Self::next(&mut self.shape);
                Self::next(&mut self.layout);
            }
            EarliestStage::Layout => Self::next(&mut self.layout),
            EarliestStage::Paint => {}
            EarliestStage::None => return,
        }
        Self::next(&mut self.base_display);
        Self::next(&mut self.overlay);
    }

    fn invalidate_base(&mut self) {
        Self::next(&mut self.base_display);
    }

    fn invalidate_overlay(&mut self) {
        Self::next(&mut self.overlay);
    }

    fn invalidate_resources(&mut self) {
        Self::next(&mut self.resources);
        Self::next(&mut self.base_display);
    }
}

#[cfg(test)]
mod renderer_revision_tests {
    use super::*;

    #[test]
    fn overlay_invalidation_does_not_expire_base_display() {
        let mut revisions = RendererRevisions::default();
        let base = revisions.base_display;
        let layout = revisions.layout;
        revisions.invalidate_overlay();
        assert_eq!(revisions.base_display, base);
        assert_eq!(revisions.layout, layout);
        assert_ne!(revisions.overlay, 1);
    }

    #[test]
    fn pipeline_invalidation_expires_only_downstream_products() {
        let mut revisions = RendererRevisions::default();
        revisions.invalidate_pipeline_from(EarliestStage::Shape);
        assert_eq!(revisions.document, 1);
        assert_eq!(revisions.style, 1);
        assert_ne!(revisions.shape, 1);
        assert_ne!(revisions.layout, 1);
        assert_ne!(revisions.base_display, 1);
        assert_ne!(revisions.overlay, 1);
    }

    #[test]
    fn resource_invalidation_keeps_interaction_overlay_reusable() {
        let mut revisions = RendererRevisions::default();
        let overlay = revisions.overlay;
        revisions.invalidate_resources();
        assert_ne!(revisions.resources, 1);
        assert_ne!(revisions.base_display, 1);
        assert_eq!(revisions.overlay, overlay);
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct DisplayCacheKey {
    pub layout_revision: u64,
    pub display_revision: u64,
    pub resources_revision: u64,
    pub viewport_width_bits: u64,
    pub viewport_height_bits: u64,
    pub column_width_bits: u64,
    pub column_gap_bits: u64,
    pub column_count: u8,
    pub scale_bits: u64,
    pub page_offset_bits: u64,
    pub direction: u8,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct FrameDisplayCacheKeys {
    pub base: DisplayCacheKey,
    pub overlay: DisplayCacheKey,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct FrameGeometryCacheKey {
    pub layout_revision: u64,
    pub viewport_width_bits: u64,
    pub viewport_height_bits: u64,
    pub column_width_bits: u64,
    pub column_gap_bits: u64,
    pub column_count: u8,
    pub scale_bits: u64,
    pub page_offset_bits: u64,
    pub direction: u8,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct LineShapeFrameKey {
    shape_revision: u64,
    layout_revision: u64,
    first_line: Option<usize>,
    last_line: Option<usize>,
}

struct VisibleShapedLineFragment {
    offset_x: f64,
    shaped: html::layout::ShapedLine,
}

pub enum DocAnchor {
    Start,
    End,
    Glyph(u32),
}

pub struct RenderState {
    pub y_offset: f64,
    pub col_index: f64,
    pub should_stop: bool,
}

#[derive(Clone, Copy)]
pub struct InlineMetrics {
    pub offset: f64,
}

#[derive(Clone, Copy)]
pub struct LineScreen {
    pub point: Point,
    pub col_index: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct RepeatedTableHeader {
    pub source_left: f64,
    pub source_right: f64,
    pub source_top: f64,
    pub source_bottom: f64,
    pub col_index: i32,
}

pub struct PagePositions {
    pub start_line: usize,
    pub last_line: Option<usize>,
    pub positions: VisibleLinePositions,
    pub offset_breaks: Vec<(f64, f64)>,
    pub repeated_table_headers: Vec<RepeatedTableHeader>,
    pub reached_end: bool,
    pub next_start_offset_y: f64,
}

/// Screen positions for the contiguous range of lines painted in one frame.
///
/// Keeping only the visible range avoids allocating and clearing an
/// `Option<LineScreen>` for every line in a document on every frame.
#[derive(Default)]
pub struct VisibleLinePositions {
    first_line: usize,
    positions: Vec<LineScreen>,
}

impl VisibleLinePositions {
    fn with_buffer(first_line: usize, mut positions: Vec<LineScreen>) -> Self {
        positions.clear();
        Self { first_line, positions }
    }

    pub fn first_line(&self) -> usize {
        self.first_line
    }

    pub fn last_line(&self) -> Option<usize> {
        (!self.positions.is_empty()).then(|| self.first_line + self.positions.len() - 1)
    }

    pub fn get(&self, line_index: usize) -> Option<LineScreen> {
        let offset = line_index.checked_sub(self.first_line)?;
        self.positions.get(offset).copied()
    }

    pub fn contains(&self, line_index: usize) -> bool {
        self.get(line_index).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, LineScreen)> + '_ {
        self.positions.iter().copied().enumerate().map(|(offset, screen)| (self.first_line + offset, screen))
    }

    fn push(&mut self, screen: LineScreen) {
        self.positions.push(screen);
    }

    fn get_mut(&mut self, line_index: usize) -> Option<&mut LineScreen> {
        let offset = line_index.checked_sub(self.first_line)?;
        self.positions.get_mut(offset)
    }

    fn clear(&mut self) {
        self.first_line = 0;
        self.positions.clear();
    }

    fn into_buffer(mut self) -> Vec<LineScreen> {
        self.positions.clear();
        self.positions
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatState {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

pub struct ColumnLayout {
    pub col_width: f64,
    pub base_col_width: f64,
    pub col_count: f64,
    pub col_gap: f64,
    pub size: Size,
    pub scale: f64,
    pub max_col_count: Option<u8>,
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self { col_width: 600.0, base_col_width: 600.0, col_count: 0.0, col_gap: 0.0, size: Size::ZERO, scale: 1.0, max_col_count: None }
    }
}

pub struct ViewportState {
    pub direction: PaintDirection,
    pub start_offset_y: f64,
    pub end_offset_y: f64,
    pub back_start_offset_y: f64,
    pub back_anchor_end_line: Option<usize>,
    pub reached_end: bool,
    pub next_start_offset_y: f64,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self { direction: PaintDirection::Forward, start_offset_y: 0.0, end_offset_y: 0.0, back_start_offset_y: 0.0, back_anchor_end_line: None, reached_end: false, next_start_offset_y: 0.0 }
    }
}

#[derive(Default)]
pub struct VisibleFrame {
    pub current_page_start_line: Option<usize>,
    pub current_page_end_line: Option<usize>,
    pub last_line_positions: VisibleLinePositions,
    pub paint_start_offset_y: f64,
    pub use_cached_inline_positions: bool,
    pub offset_breaks: Vec<(f64, f64)>,
    pub repeated_table_headers: Vec<RepeatedTableHeader>,
    pub geometry_key: Option<FrameGeometryCacheKey>,
    shaped_lines: Vec<VisibleShapedLineFragment>,
    native_shape_failures: HashSet<usize>,
    line_shape_key: Option<LineShapeFrameKey>,
}

impl VisibleFrame {
    fn clear_backend_shaping(&mut self) {
        self.shaped_lines.clear();
        self.native_shape_failures.clear();
        self.line_shape_key = None;
    }

    fn clear(&mut self) {
        self.current_page_start_line = None;
        self.current_page_end_line = None;
        self.last_line_positions.clear();
        self.paint_start_offset_y = 0.0;
        self.use_cached_inline_positions = false;
        self.offset_breaks.clear();
        self.repeated_table_headers.clear();
        self.geometry_key = None;
        self.clear_backend_shaping();
    }
}

#[derive(Default)]
struct FrameScratch {
    line_position_buffers: Vec<Vec<LineScreen>>,
    desired_images: HashSet<u32>,
    line_shape: LineShapeScratch,
}

#[derive(Default)]
struct LineShapeScratch {
    source_runs: Vec<(Range<u32>, usize)>,
    source_runs_shape_revision: Option<u64>,
    #[cfg(test)]
    source_run_rebuilds: usize,
    text: String,
    byte_offsets: Vec<usize>,
    extra_advances: Vec<f32>,
    advance_overrides: Vec<Option<f32>>,
    baseline_shifts: Vec<f32>,
    paints: Vec<bool>,
    placements: Vec<html::layout::CharacterPlacement>,
}

impl FrameScratch {
    fn take_line_position_buffer(&mut self, visible: &mut VisibleLinePositions) -> Vec<LineScreen> {
        self.line_position_buffers.pop().unwrap_or_else(|| std::mem::take(visible).into_buffer())
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

        let capacity = positions.positions.capacity();
        let mut frame = VisibleFrame { last_line_positions: positions, ..VisibleFrame::default() };
        let mut scratch = FrameScratch::default();
        frame.clear();
        let reused = scratch.take_line_position_buffer(&mut frame.last_line_positions);
        assert!(reused.capacity() >= capacity);
    }
}

#[derive(Default)]
pub struct SelectionState {
    pub is_selecting: bool,
    pub selection_anchor: Option<u32>,
    pub selection_active: Option<u32>,
    pub pending_link_glyph: Option<u32>,
    pub selected_text: Option<String>,
    pub selected_text_markdown: Option<String>,
    pub link_cursor_active: bool,
    pub semantic_selection: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TableCellRange {
    table_box: usize,
    row_start: usize,
    row_end: usize,
    column_start: usize,
    column_end: usize,
}

#[derive(Default)]
pub(crate) struct TableSelectionState {
    range: Option<TableCellRange>,
    anchor: Option<(usize, usize, usize, usize, usize)>,
    dragging: bool,
}

#[derive(Default)]
pub struct HighlightState {
    pub query: String,
    pub matches: Vec<(u32, u32)>,
    pub book_matches: Vec<BookSearchMatch>,
    pub current_match: usize,
    pub current_local_match: usize,
    pub search_active: bool,
    pub options: SearchOptions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchScope {
    CurrentDocument,
    #[default]
    WholeBook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_diacritics: bool,
    pub scope: SearchScope,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { match_case: false, whole_word: false, match_diacritics: false, scope: SearchScope::WholeBook }
    }
}

#[derive(Clone, Debug)]
pub struct BookSearchMatch {
    pub doc: usize,
    pub occurrence: usize,
    pub excerpt: String,
}

pub struct NavigationState {
    pub document_uris: Vec<String>,
    pub document_text_lengths: Vec<u64>,
    pub current_doc_index: usize,
    pub nav_anchor_glyph: Option<u32>,
    pub pending_nav_anchor_update: bool,
    pub signals: NavSignals,
    pub history: LocationHistory,
    pub toc_anchors: TocAnchors,
}

pub(crate) struct RendererCore {
    host: Rc<dyn RendererHost>,
    document: LaidOutDocument,
    pipeline_session: Engine,
    pipeline_inputs: PipelineInputs,
    image_pipeline: ImagePipeline,
    provider: Arc<dyn ResourceProvider>,
    root_font_size: f32,
    layout: ColumnLayout,
    viewport: ViewportState,
    document_cache: DocumentCache,
    prepared_pages: PreparedPageCache,
    frame: VisibleFrame,
    frame_scratch: FrameScratch,
    revisions: RendererRevisions,
    selection: SelectionState,
    table_selection: TableSelectionState,
    highlight: HighlightState,
    annotations: AnnotationOverlayState,
    media_overlay: MediaOverlayHighlightState,
    nav: NavigationState,
    origin: Point,
    interaction_palette: InteractionPalette,
    reader_palette: ReaderPaintPalette,
    vertical_rhythm: bool,
}

impl RendererCore {
    pub fn revisions(&self) -> RendererRevisions {
        self.revisions
    }

    pub fn has_pending_resources(&self) -> bool {
        self.image_pipeline.has_pending()
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
        let page_offset = match self.viewport.direction {
            PaintDirection::Forward => self.viewport.start_offset_y,
            PaintDirection::Backward => self.viewport.end_offset_y,
        };
        FrameGeometryCacheKey {
            layout_revision: self.revisions.layout,
            viewport_width_bits: self.layout.size.width.to_bits(),
            viewport_height_bits: self.layout.size.height.to_bits(),
            column_width_bits: self.layout.col_width.to_bits(),
            column_gap_bits: self.layout.col_gap.to_bits(),
            column_count: self.layout.col_count.max(0.0).min(255.0) as u8,
            scale_bits: self.layout.scale.to_bits(),
            page_offset_bits: page_offset.to_bits(),
            direction: match self.viewport.direction {
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

    fn request_resource_repaint(&mut self) {
        self.revisions.invalidate_resources();
        self.host.request_repaint();
    }

    pub fn document(&self) -> &LaidOutDocument {
        &self.document
    }

    pub fn root_font_size(&self) -> f32 {
        self.root_font_size
    }

    pub fn preferred_column_width(&self) -> f64 {
        self.layout.base_col_width
    }

    pub fn scale(&self) -> f64 {
        self.layout.scale
    }

    pub fn search_active(&self) -> bool {
        self.highlight.search_active
    }

    pub fn link_cursor_active(&self) -> bool {
        self.selection.link_cursor_active
    }

    pub fn origin(&self) -> Point {
        self.origin
    }

    pub fn set_origin(&mut self, origin: Point) {
        self.origin = origin;
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette) {
        if self.interaction_palette == palette {
            return;
        }
        self.interaction_palette = palette;
        self.request_overlay_repaint();
    }

    pub fn to_layout_point(&self, position: Point) -> Point {
        let scale = self.layout.scale;
        let dx = self.origin.x * (scale - 1.0);
        let dy = self.origin.y * (scale - 1.0);
        Point::new((position.x + dx) / scale, (position.y + dy) / scale)
    }

    pub fn set_link_cursor_active(&mut self, active: bool) {
        if self.selection.link_cursor_active != active {
            self.selection.link_cursor_active = active;
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
        let image_pipeline = ImagePipeline::new(Arc::new(image_resources), provider.clone());
        let document_text_lengths = vec![1; document_uris.len()];
        let document_cache = DocumentCache::new(DEFAULT_DOCUMENT_CACHE_CAPACITY, current_doc_index, FootnoteDocumentIndex::parse(&loaded.inputs.source));
        let column_width = config.column_width.clamp(200.0, 1200.0).round();
        let layout = ColumnLayout { col_width: column_width, base_col_width: column_width, scale: config.scale.clamp(0.5, 3.0), max_col_count: config.max_column_count.map(|count| count.max(1)), ..Default::default() };
        let mut highlight = HighlightState::default();
        highlight.search_active = config.search_active;
        highlight.query = config.search_query;
        Self {
            host,
            document,
            pipeline_session: loaded.session,
            pipeline_inputs: loaded.inputs,
            image_pipeline,
            provider,
            root_font_size,
            layout,
            viewport: ViewportState::default(),
            document_cache,
            prepared_pages: PreparedPageCache::new(),
            frame: VisibleFrame::default(),
            frame_scratch: FrameScratch::default(),
            revisions: RendererRevisions::default(),
            selection: SelectionState::default(),
            table_selection: TableSelectionState::default(),
            highlight,
            annotations: AnnotationOverlayState::default(),
            media_overlay: MediaOverlayHighlightState::default(),
            nav: NavigationState {
                document_uris,
                document_text_lengths,
                current_doc_index,
                nav_anchor_glyph: None,
                pending_nav_anchor_update: false,
                signals: Default::default(),
                history: Default::default(),
                toc_anchors: Default::default(),
            },
            origin: Point::ZERO,
            interaction_palette: config.interaction_palette,
            reader_palette: config.paint_palette,
            vertical_rhythm: config.vertical_rhythm,
        }
    }

    fn next_pipeline_inputs_for_layout(&self, viewport_width: f64) -> PipelineInputs {
        let mut next = self.pipeline_inputs.clone();
        next.layout = html::pipeline::LayoutConstraints {
            viewport_width,
            viewport_height: (self.layout.size.height.is_finite() && self.layout.size.height >= 0.0).then_some(self.layout.size.height),
            line_height: self.pipeline_inputs.layout.line_height,
            image_sizing_policy: self.pipeline_inputs.layout.image_sizing_policy,
            text_composition_policy: self.pipeline_inputs.layout.text_composition_policy,
        };
        let viewport_height = (self.layout.size.height.is_finite() && self.layout.size.height > 0.0).then_some(self.layout.size.height);
        next.style_environment.media = html::pipeline::MediaEnvironment::screen(viewport_width, viewport_height).expect("effective column dimensions must be finite and positive");
        next
    }

    fn next_pipeline_inputs_for_style(&self, root_font_size: f32) -> PipelineInputs {
        let mut next = self.pipeline_inputs.clone();
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
        // Before the first frame, `current_glyph_position` resolves to the
        // document's first glyph even when decoration or replaced content
        // precedes it. Relayout at the true document start must preserve the
        // zero offset instead of manufacturing a glyph anchor that scrolls
        // past that leading content.
        let first_glyph = self.document.render_view().text().line(0).map(|line| line.start());
        let anchor_is_document_start = match (self.nav.nav_anchor_glyph, first_glyph) {
            (None, _) => true,
            (Some(anchor), Some(first)) => anchor == first,
            (Some(_), None) => false,
        };
        let preserve_document_start = anchor_is_document_start && matches!(self.viewport.direction, PaintDirection::Forward) && self.viewport.start_offset_y <= 0.1 && self.frame.current_page_start_line.is_none_or(|line| line == 0);
        let anchor_glyph = if preserve_document_start { None } else { self.nav.nav_anchor_glyph.or_else(|| self.current_glyph_position()) };
        let anchor_cfi = if preserve_document_start { None } else { self.current_cfi() };

        let update = self.pipeline_session.update(requested_inputs.clone(), glyph_shaper)?;

        let document = self.pipeline_session.document().cloned().ok_or_else(|| html::pipeline::PipelineError("pipeline update completed without a laid-out document".to_owned()))?;
        self.document = document;
        self.pipeline_inputs = requested_inputs;
        self.root_font_size = self.pipeline_inputs.style_environment.root_font_size as f32;
        if update.stage == EarliestStage::Parse {
            self.document_cache.insert_footnotes(self.nav.current_doc_index, FootnoteDocumentIndex::parse(&self.pipeline_inputs.source));
        }
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

        self.host.request_repaint();
        Ok(())
    }

    pub fn emit_state_snapshot(&mut self) {
        self.host.emit(RendererEvent::TitleChanged(self.document.render_view().title().map(str::to_owned)));
        self.host.emit(RendererEvent::TocChanged(Self::build_document_toc_entries_for(&self.document)));
        self.nav.history.emit_availability(self.host.as_ref());
        self.host.emit(RendererEvent::SearchActiveChanged(self.highlight.search_active));
        self.update_match_signal();
        self.host.emit(RendererEvent::ColumnWidthChanged(self.layout.base_col_width));
        self.host.emit(RendererEvent::FontSizeChanged(self.root_font_size));
        self.host.emit(RendererEvent::ScaleChanged(self.layout.scale));
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
                if lengths.len() == self.nav.document_uris.len() && lengths.iter().all(|length| *length > 0) {
                    self.nav.document_text_lengths = lengths;
                    self.nav.signals.progress.invalidate();
                    self.update_nav_signal();
                }
            }
            RendererCommand::SetColumnWidth(width) => self.set_col_width(glyph_shaper, width),
            RendererCommand::SetMaxColumnCount(count) => {
                let count = count.map(|count| count.max(1));
                if self.layout.max_col_count != count {
                    self.layout.max_col_count = count;
                    self.host.request_repaint();
                }
            }
            RendererCommand::SetFontSize(size) => self.set_root_font_size(glyph_shaper, size),
            RendererCommand::SetReaderStyleOverrides(overrides) => {
                let mut inputs = self.pipeline_inputs.clone();
                if inputs.reader_overrides != overrides {
                    inputs.reader_overrides = overrides;
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetImageSizingPolicy(policy) => {
                let mut inputs = self.pipeline_inputs.clone();
                if inputs.layout.image_sizing_policy != policy {
                    inputs.layout.image_sizing_policy = policy;
                    if let Err(error) = self.apply_pipeline_inputs(inputs, glyph_shaper) {
                        self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
                    }
                }
            }
            RendererCommand::SetTextCompositionPolicy(policy) => {
                let mut inputs = self.pipeline_inputs.clone();
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
            RendererCommand::SetSearchActive(active) => self.set_search_active(active),
            RendererCommand::SetSearchQuery(query) => self.set_search_query(&query),
            RendererCommand::SetSearch { query, options } => self.set_search(glyph_shaper, &query, options),
            RendererCommand::SetSearchResults { query, options, results } => self.set_search_results(glyph_shaper, query, options, results),
            RendererCommand::AppendSearchResults { query, options, results } => self.append_search_results(glyph_shaper, query, options, results),
            RendererCommand::NavigateSearch(step) if step > 0 => self.next_match(glyph_shaper),
            RendererCommand::NavigateSearch(_) => self.prev_match(glyph_shaper),
            RendererCommand::SetAnnotations(annotations) => {
                self.annotations.source = annotations;
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::UpsertAnnotation(annotation) => {
                self.annotations.source.retain(|item| item.id != annotation.id);
                self.annotations.source.push(annotation);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::RemoveAnnotation(id) => {
                self.annotations.source.retain(|item| item.id != id);
                self.resolve_visible_annotations();
                self.request_overlay_repaint();
            }
            RendererCommand::SetMediaOverlayTarget(target_href) => {
                self.media_overlay.target_href = target_href;
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
                let text = self.selection.selected_text.as_deref().unwrap_or_default();
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
        if document != self.nav.current_doc_index {
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

    pub(crate) fn set_search_active(&mut self, active: bool) {
        if !self.highlight.set_active(active) {
            return;
        }
        self.host.emit(RendererEvent::SearchActiveChanged(active));
        if !active {
            self.set_search_query("");
        } else {
            self.request_overlay_repaint();
        }
    }

    pub fn document_toc(&self) -> Vec<TocEntry> {
        Self::build_document_toc_entries_for(&self.document)
    }

    pub fn set_toc_anchor_strings_by_doc(&mut self, anchors_by_doc: Vec<Vec<String>>) {
        let document_count = self.nav.document_uris.len();
        self.nav.toc_anchors.set_strings_by_doc(anchors_by_doc, document_count);
        self.update_toc_anchor_filter();
    }

    pub fn set_search_query(&mut self, query: &str) {
        let options = self.highlight.current_document_options();
        self.highlight.restart(query.to_owned(), options);
        let matches = self.doc().find_matches_with_options(query, options);
        let empty = matches.is_empty();
        self.highlight.set_local_matches(self.nav.current_doc_index, matches);
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
            self.highlight.set_local_matches(self.nav.current_doc_index, matches);
        } else {
            let results = search_publication(self.provider.as_ref(), &self.nav.document_uris, query, options);
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
        let Some(hit) = self.highlight.current_book_match() else { return };
        if hit.doc != self.nav.current_doc_index {
            self.set_position(glyph_shaper, hit.doc, None);
        }
        self.highlight.matches = self.doc().find_matches_with_options(&self.highlight.query, self.highlight.options);
        self.highlight.focus_local_match(hit.occurrence);
        self.navigate_to_current_match();
    }

    fn step_match(&mut self, glyph_shaper: &mut impl GlyphShaper, forward: bool) {
        if !self.highlight.step(forward) {
            return;
        }
        self.update_match_signal();
        self.navigate_to_book_match(glyph_shaper);
    }

    pub fn next_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        self.step_match(glyph_shaper, true);
    }

    pub fn prev_match(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        self.step_match(glyph_shaper, false);
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

#[path = "renderer/paginate.rs"]
mod paginate;

#[path = "renderer/vertical_rhythm.rs"]
mod vertical_rhythm;

#[path = "renderer/semantic_keeps.rs"]
mod semantic_keeps;

#[path = "renderer/nav.rs"]
mod nav;

#[path = "renderer/interaction.rs"]
mod interaction;

pub use html_view_load as load;
