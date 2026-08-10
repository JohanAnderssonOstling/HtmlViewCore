//! Data types shared across the reader crates.
//!
//! Everything here is state or a contract: column geometry, viewport and frame
//! bookkeeping, the command and event vocabulary, and the cache keys that decide
//! when painted output can be reused. No document querying, pagination or
//! navigation logic lives here -- those are separate crates that agree on these
//! types.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use kurbo::{Point, Size};
pub use peniko::Color;

use html::pipeline::EarliestStage;
pub use html::resources::TocEntry;

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

/// How note bodies take part in the reading flow.
///
/// Notes are identified by EPUB structural semantics (`epub:type` tokens
/// `footnote`/`endnote`/`rearnote`) or the DPUB-ARIA roles `doc-footnote` and
/// `doc-endnote`. Markup carrying neither is not a note to the reader and is
/// unaffected by this setting.
///
/// The engine decides what *is* a note; this decides what the reader *does*
/// with one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoteDisplay {
    /// Note bodies are held out of the paginated flow and shown on demand when
    /// their reference is activated.
    #[default]
    Popup,
    /// Note bodies stay where the author placed them and read inline. For
    /// publications that already collect notes at the end of a chapter or the
    /// book, this is the authored presentation.
    AsAuthored,
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
    pub note_display: NoteDisplay,
    /// Shows the spine as one continuous run rather than a document at a time.
    /// Each spine item still begins at the top of a column, so a document
    /// boundary is always a column boundary.
    pub continuous_spine: bool,
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
            note_display: NoteDisplay::default(),
            continuous_spine: false,
            vertical_rhythm: false,
            paint_palette: Default::default(),
            interaction_palette: Default::default(),
            search_active: false,
            search_query: String::new(),
        }
    }
}

/// A note the reader has asked its host to show.
///
/// `note` is the note laid out on its own, ready to paint through the same
/// path as the page. `blocks` is its flattened text, kept for hosts that
/// present notes as plain strings.
///
/// The laid-out note is shared rather than copied: a host holds a preview for
/// as long as its popup is open and clones it freely while rendering.
#[derive(Clone)]
pub struct FootnotePreview {
    pub href: String,
    pub blocks: Vec<String>,
    pub note: std::sync::Arc<html::layout::LaidOutDocument>,
}

// A laid-out document is neither comparable nor printable, and neither is
// useful here: a preview is identified by the reference it came from.
impl fmt::Debug for FootnotePreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FootnotePreview").field("href", &self.href).field("blocks", &self.blocks).finish_non_exhaustive()
    }
}

impl PartialEq for FootnotePreview {
    fn eq(&self, other: &Self) -> bool {
        self.href == other.href && self.blocks == other.blocks
    }
}

impl Eq for FootnotePreview {}

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
    /// Switches how notes are presented. This re-lays-out the document, since
    /// whether a note occupies the reading flow decides what boxes exist.
    SetNoteDisplay(NoteDisplay),
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

    /// Width to lay a note out at before the reader hands it back to be shown.
    /// A host that presents notes in a panel narrower than the page should
    /// answer with that width; `None` lays the note out at the column width.
    ///
    /// Read once, when a note is activated. A note is not laid out again while
    /// it is on screen, so this must be the width the host will actually show
    /// it at -- notes are presented at a fixed size rather than resized.
    fn note_popup_width(&self) -> Option<f64> {
        None
    }
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

    pub fn invalidate_pipeline_from(&mut self, stage: EarliestStage) {
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

    pub fn invalidate_base(&mut self) {
        Self::next(&mut self.base_display);
    }

    pub fn invalidate_overlay(&mut self) {
        Self::next(&mut self.overlay);
    }

    pub fn invalidate_resources(&mut self) {
        Self::next(&mut self.resources);
        Self::next(&mut self.base_display);
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
pub struct LineShapeFrameKey {
    pub shape_revision: u64,
    pub layout_revision: u64,
    pub first_line: Option<usize>,
    pub last_line: Option<usize>,
}

pub struct VisibleShapedLineFragment {
    pub offset_x: f64,
    pub shaped: html::layout::ShapedLine,
}

pub enum DocAnchor {
    Start,
    End,
    Glyph(u32),
    /// A document entered part-way, at a layout offset rather than at one of
    /// its ends. Continuous reading arrives this way: the item was already on
    /// screen beside the previous page, so reading resumes below what was
    /// shown of it.
    Offset(f64),
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
    pub fn with_buffer(first_line: usize, mut positions: Vec<LineScreen>) -> Self {
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

    pub fn capacity(&self) -> usize {
        self.positions.capacity()
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, LineScreen)> + '_ {
        self.positions.iter().copied().enumerate().map(|(offset, screen)| (self.first_line + offset, screen))
    }

    /// Moves every line right by `columns`, for content paginated from its own
    /// first column and then placed after something else.
    pub fn shift_columns(&mut self, columns: i32) {
        for screen in &mut self.positions {
            screen.col_index += columns;
        }
    }

    pub fn push(&mut self, screen: LineScreen) {
        self.positions.push(screen);
    }

    pub fn get_mut(&mut self, line_index: usize) -> Option<&mut LineScreen> {
        let offset = line_index.checked_sub(self.first_line)?;
        self.positions.get_mut(offset)
    }

    pub fn clear(&mut self) {
        self.first_line = 0;
        self.positions.clear();
    }

    pub fn into_buffer(mut self) -> Vec<LineScreen> {
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

#[derive(Clone)]
pub struct ColumnLayout {
    pub col_width: f64,
    pub base_col_width: f64,
    pub col_count: f64,
    pub col_gap: f64,
    pub size: Size,
    pub scale: f64,
    pub max_col_count: Option<u8>,
}

impl ColumnLayout {
    /// Compute x-coordinate for a column given a local x offset
    #[inline]
    pub fn col_x(&self, col_index: f64, local_x: f64) -> f64 {
        self.col_gap + col_index * (self.col_width + self.col_gap) + local_x
    }

    pub fn resolve_point_at(&self, point: Point, elem_height: f64, mut render_state: RenderState, start_offset_y: f64) -> (Option<Point>, RenderState) {
        // map document point to screen columns
        let mut y = point.y + render_state.y_offset - start_offset_y;
        let mut col_index = (y / self.size.height).floor(); // initial column based on vertical position
        y -= col_index * self.size.height;

        if y + elem_height > self.size.height {
            // move to next column if element would overflow
            col_index += 1.0;
            render_state.y_offset += self.size.height - y;
            y = 0.;
        }

        // Past all columns - stop rendering
        if col_index >= self.col_count {
            // stop when past last column
            render_state.should_stop = true;
            return (None, render_state);
        }

        render_state.col_index = col_index;
        let x = self.col_x(col_index, point.x);
        let resolved = Point::new(x, y);
        (Some(resolved), render_state)
    }

    pub fn inline_metrics(&self) -> crate::InlineMetrics {
        // compute inline decoration offset within column gap
        const MIN_GAP: f64 = 20.0;
        let effective_gap = (self.col_gap - MIN_GAP).max(0.0);
        let half_gap = effective_gap / 2.0;
        crate::InlineMetrics { offset: half_gap / 2.0 }
    }

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
    pub shaped_lines: Vec<VisibleShapedLineFragment>,
    pub native_shape_failures: HashSet<usize>,
    pub line_shape_key: Option<LineShapeFrameKey>,
}

impl VisibleFrame {
    pub fn clear_backend_shaping(&mut self) {
        self.shaped_lines.clear();
        self.native_shape_failures.clear();
        self.line_shape_key = None;
    }

    pub fn clear(&mut self) {
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

