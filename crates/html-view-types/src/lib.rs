//! Data types shared across the reader crates.
//!
//! Everything here is state or a contract: column geometry, viewport and frame
//! bookkeeping, the command and event vocabulary, and the cache keys that decide
//! when painted output can be reused. No document querying, pagination or
//! navigation logic lives here -- those are separate crates that agree on these
//! types.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
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

/// A glyph together with the spine document whose glyph registry owns it.
/// Raw glyph indices are deliberately not sufficient for screen-wide input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentGlyph {
    pub doc: usize,
    pub glyph: u32,
}

/// An image together with the spine document whose resource table owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentImage {
    pub doc: usize,
    pub image: u32,
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
            vertical_rhythm: false,
            paint_palette: Default::default(),
            interaction_palette: Default::default(),
            search_active: false,
            search_query: String::new(),
        }
    }
}

/// Metadata telling a host where to present the renderer-owned note surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupAnchorCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Viewport-local overlay placement for a footnote popup. `corner` names the
/// popup corner that the host places exactly at `point`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootnotePopupAnchor {
    pub point: Point,
    pub corner: PopupAnchorCorner,
}

impl FootnotePopupAnchor {
    pub fn at_click(point: Point, viewport: Size) -> Self {
        let opens_right = point.x <= viewport.width / 2.0;
        let opens_down = point.y <= viewport.height / 2.0;
        let corner = match (opens_right, opens_down) {
            (true, true) => PopupAnchorCorner::TopLeft,
            (false, true) => PopupAnchorCorner::TopRight,
            (true, false) => PopupAnchorCorner::BottomLeft,
            (false, false) => PopupAnchorCorner::BottomRight,
        };
        Self { point, corner }
    }

    /// Converts the touching-corner contract into the popup's top-left
    /// viewport origin after the host knows the popup's final size.
    pub fn popup_origin(self, popup: Size) -> Point {
        match self.corner {
            PopupAnchorCorner::TopLeft => self.point,
            PopupAnchorCorner::TopRight => Point::new(self.point.x - popup.width, self.point.y),
            PopupAnchorCorner::BottomLeft => Point::new(self.point.x, self.point.y - popup.height),
            PopupAnchorCorner::BottomRight => {
                Point::new(self.point.x - popup.width, self.point.y - popup.height)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FootnotePreview {
    pub href: String,
    /// Present for pointer activation and absent for programmatic previews.
    pub anchor: Option<FootnotePopupAnchor>,
}

#[derive(Clone, Debug)]
pub enum RendererCommand {
    NavigateToHref(String),
    NavigateHistory(i8),
    NavigateDocument(i8),
    SetPosition {
        doc: usize,
        glyph: Option<u32>,
    },
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
    ActivateSearch,
    DeactivateSearch,
    SetSearchQuery(String),
    SetSearch {
        query: String,
        options: SearchOptions,
    },
    SetSearchResults {
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    },
    AppendSearchResults {
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    },
    NavigateSearch(i8),
    SetAnnotations(Vec<RendererAnnotation>),
    UpsertAnnotation(RendererAnnotation),
    RemoveAnnotation(String),
    SetMediaOverlayTarget(Option<String>),
    RequestVisibleText,
    CopySelection,
    CopySelectionCitation {
        title: String,
        cfi: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum RendererEvent {
    PositionChanged {
        doc: usize,
        glyph: Option<u32>,
    },
    CfiChanged(Option<String>),
    NavAnchorChanged {
        doc: usize,
        anchor: Option<String>,
    },
    TitleChanged(Option<String>),
    TocChanged(Vec<TocEntry>),
    MatchInfo {
        current: usize,
        total: usize,
    },
    SearchResults(Vec<BookSearchMatch>),
    HistoryAvailability {
        back: bool,
        forward: bool,
    },
    SearchActiveChanged(bool),
    ColumnWidthChanged(f64),
    FontSizeChanged(f32),
    ScaleChanged(f64),
    ReadingProgress {
        fraction: f32,
        location: u64,
        total_locations: u64,
        doc: usize,
        doc_count: usize,
    },
    SelectionFinished {
        doc: usize,
        cfi_range: String,
        exact_text: String,
        prefix: Option<String>,
        suffix: Option<String>,
    },
    AnnotationActivated {
        id: String,
    },
    FootnoteOpened(FootnotePreview),
    SpeakableText(String),
    ImageOpened {
        uri: String,
        bytes: Vec<u8>,
    },
    OperationFailed {
        operation: RendererOperation,
        message: String,
    },
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
    fn resource_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>>;
    fn set_clipboard(&self, text: &str) -> Result<(), String>;
    fn set_clipboard_image(&self, width: usize, height: usize, rgba: Vec<u8>)
    -> Result<(), String>;
    fn set_clipboard_svg(&self, _bytes: Vec<u8>) -> Result<(), String> {
        Err("copying SVG images is not supported by this renderer host".to_owned())
    }
    fn emit(&self, event: RendererEvent);

    /// Names the spine item whose glyphs are about to be shaped or painted.
    ///
    /// A glyph id indexes the registry of the document it was shaped from, so
    /// ids only mean something alongside the document they came from. A host
    /// that shows one document at a time normally needs only one registry.
    /// Popup notes may belong to another spine item, so hosts can still keep
    /// document-qualified registries and switch to the requested one.
    ///
    /// Called before every stretch of work that belongs to a single document,
    /// so a host may treat it as the current one until told otherwise.
    fn set_glyph_document(&self, _doc: usize) {}

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
        Self {
            document: 1,
            style: 1,
            shape: 1,
            layout: 1,
            base_display: 1,
            overlay: 1,
            resources: 1,
        }
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
    pub block_decoration_indices: Vec<usize>,
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
    paint_order_indices: Vec<u32>,
}

impl VisibleLinePositions {
    pub fn with_buffer(first_line: usize, mut positions: Vec<LineScreen>) -> Self {
        positions.clear();
        Self {
            first_line,
            positions,
            paint_order_indices: Vec::new(),
        }
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
        self.positions
            .iter()
            .copied()
            .enumerate()
            .map(|(offset, screen)| (self.first_line + offset, screen))
    }

    /// Visible spatial line indexes in CSS paint order. Pagination prepares
    /// this once so every paint layer can traverse only the current page.
    pub fn paint_order_indices(&self) -> &[u32] {
        &self.paint_order_indices
    }

    pub fn set_paint_order_indices(&mut self, indices: Vec<u32>) {
        self.paint_order_indices = indices;
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
        self.paint_order_indices.clear();
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnGeometry {
    pub width: f64,
    pub count: f64,
    pub gap: f64,
    pub viewport: Size,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnViewportPlan {
    requested_column_width: f64,
    column_width_changed: bool,
}

impl ColumnViewportPlan {
    pub fn requested_column_width(self) -> f64 {
        self.requested_column_width
    }

    pub fn column_width_changed(self) -> bool {
        self.column_width_changed
    }
}

impl ColumnLayout {
    pub fn geometry(&self) -> ColumnGeometry {
        ColumnGeometry {
            width: self.col_width,
            count: self.col_count,
            gap: self.col_gap,
            viewport: self.size,
            scale: self.scale,
        }
    }

    /// Starts a viewport reconfiguration in layout coordinates and reports
    /// whether accepting its effective width requires a document relayout.
    pub fn begin_viewport_reconfiguration(&mut self, viewport: Size) -> ColumnViewportPlan {
        self.size = viewport;
        self.size /= self.scale;
        let effective = if self.size.width > 0.0 {
            self.base_col_width.min(self.size.width)
        } else {
            self.base_col_width
        };
        let changed = (effective - self.col_width).abs() > 24.0;
        ColumnViewportPlan {
            requested_column_width: if changed { effective } else { self.col_width },
            column_width_changed: changed,
        }
    }

    /// Finishes viewport reconfiguration after the caller has accepted or
    /// rejected the width-dependent document relayout.
    pub fn finish_viewport_reconfiguration(&mut self, accepted_width: Option<f64>) {
        const MIN_GAP: f64 = 20.0;
        if let Some(width) = accepted_width {
            self.col_width = width;
        }
        let raw_count = ((self.size.width + MIN_GAP) / (self.col_width + MIN_GAP)).floor();
        self.col_count = self.max_col_count.map_or(raw_count.max(1.0), |maximum| {
            raw_count.max(1.0).min(f64::from(maximum))
        });
        self.col_gap =
            ((self.size.width - self.col_count * self.col_width) / (self.col_count + 1.0)).max(0.0);
    }

    pub fn commit_column_width(&mut self, base_width: Option<f64>, effective_width: Option<f64>) {
        if let Some(width) = base_width {
            self.base_col_width = width;
        }
        if let Some(width) = effective_width {
            self.col_width = width;
        }
    }

    pub fn update_scale(&mut self, requested: f64) -> Option<f64> {
        let scale = requested.clamp(0.5, 3.0);
        if (scale - self.scale).abs() < f64::EPSILON {
            return None;
        }
        self.scale = scale;
        Some(scale)
    }

    /// Produces geometry for a temporary document slice without changing the
    /// reader's persistent column preference.
    pub fn for_column_count(&self, count: usize) -> Self {
        let mut layout = self.clone();
        layout.col_count = count.max(1) as f64;
        layout
    }

    /// Compute x-coordinate for a column given a local x offset
    #[inline]
    pub fn col_x(&self, col_index: f64, local_x: f64) -> f64 {
        self.col_gap + col_index * (self.col_width + self.col_gap) + local_x
    }

    pub fn resolve_point_at(
        &self,
        point: Point,
        elem_height: f64,
        mut render_state: RenderState,
        start_offset_y: f64,
    ) -> (Option<Point>, RenderState) {
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
        crate::InlineMetrics {
            offset: half_gap / 2.0,
        }
    }
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self {
            col_width: 600.0,
            base_col_width: 600.0,
            col_count: 0.0,
            col_gap: 0.0,
            size: Size::ZERO,
            scale: 1.0,
            max_col_count: None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ViewportState {
    composition: ViewportComposition,
    backward: BackwardComposition,
    continuation: ViewportContinuation,
}

#[derive(Clone, Copy)]
pub struct ViewportComposition {
    direction: PaintDirection,
    start_offset_y: f64,
    end_offset_y: f64,
}

impl ViewportComposition {
    pub fn direction(self) -> PaintDirection {
        self.direction
    }

    pub fn start_offset_y(self) -> f64 {
        self.start_offset_y
    }

    pub fn end_offset_y(self) -> f64 {
        self.end_offset_y
    }

    pub fn active_offset_y(self) -> f64 {
        match self.direction {
            PaintDirection::Forward => self.start_offset_y,
            PaintDirection::Backward => self.end_offset_y,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViewportContinuation {
    MoreAt(f64),
    EndedAt(f64),
}

impl ViewportContinuation {
    pub fn reached_end(self) -> bool {
        matches!(self, Self::EndedAt(_))
    }

    pub fn next_start_offset_y(self) -> f64 {
        match self {
            Self::MoreAt(offset_y) | Self::EndedAt(offset_y) => offset_y,
        }
    }
}

#[derive(Clone, Copy)]
pub struct BackwardComposition {
    start_offset_y: f64,
    anchor_end_line: Option<usize>,
}

impl BackwardComposition {
    pub fn start_offset_y(self) -> f64 {
        self.start_offset_y
    }

    pub fn anchor_end_line(self) -> Option<usize> {
        self.anchor_end_line
    }
}

impl ViewportState {
    pub fn composition(&self) -> ViewportComposition {
        self.composition
    }

    pub fn continuation(&self) -> ViewportContinuation {
        self.continuation
    }

    pub fn backward_composition(&self) -> BackwardComposition {
        self.backward
    }

    /// Starts a fresh forward composition whose screen begins at `offset_y`.
    pub fn begin_forward_at(&mut self, offset_y: f64) {
        self.composition = ViewportComposition {
            direction: PaintDirection::Forward,
            start_offset_y: offset_y,
            end_offset_y: offset_y,
        };
        self.backward = BackwardComposition {
            start_offset_y: 0.0,
            anchor_end_line: None,
        };
        self.continuation = ViewportContinuation::MoreAt(offset_y);
    }

    /// Starts a fresh backward composition ending at the supplied document
    /// offset and, when known, line.
    pub fn begin_backward_at(&mut self, end_offset_y: f64, anchor_end_line: Option<usize>) {
        self.composition.direction = PaintDirection::Backward;
        self.composition.end_offset_y = end_offset_y;
        self.backward = BackwardComposition {
            start_offset_y: 0.0,
            anchor_end_line,
        };
        self.continuation = ViewportContinuation::MoreAt(self.continuation.next_start_offset_y());
    }

    /// Installs the viewport metadata carried by a cached forward page.
    pub fn install_forward_page(
        &mut self,
        start_offset_y: f64,
        continuation: ViewportContinuation,
    ) {
        self.begin_forward_at(start_offset_y);
        self.continuation = continuation;
    }

    /// Publishes the continuation returned by forward pagination.
    pub fn update_forward_result(&mut self, continuation: ViewportContinuation) {
        self.continuation = continuation;
    }

    /// Publishes the visible start found while composing backward. A later
    /// forward page begins where that backward composition ended.
    pub fn finish_backward_composition(&mut self, start_offset_y: f64) {
        self.composition.start_offset_y = start_offset_y;
        self.continuation = ViewportContinuation::MoreAt(self.composition.end_offset_y);
    }

    /// Resets transient output before resolving a backward page.
    pub fn begin_backward_preparation(&mut self) {
        debug_assert!(matches!(
            self.composition.direction,
            PaintDirection::Backward
        ));
        self.continuation = ViewportContinuation::MoreAt(self.continuation.next_start_offset_y());
    }

    /// Updates the candidate document offset used to compose backward.
    pub fn set_backward_composition_start(&mut self, start_offset_y: f64) {
        debug_assert!(matches!(
            self.composition.direction,
            PaintDirection::Backward
        ));
        self.backward.start_offset_y = start_offset_y;
    }
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            composition: ViewportComposition {
                direction: PaintDirection::Forward,
                start_offset_y: 0.0,
                end_offset_y: 0.0,
            },
            backward: BackwardComposition {
                start_offset_y: 0.0,
                anchor_end_line: None,
            },
            continuation: ViewportContinuation::MoreAt(0.0),
        }
    }
}

#[derive(Default)]
pub struct VisibleFrame {
    page: VisiblePageGeometry,
    geometry_key: Option<FrameGeometryCacheKey>,
    shaping: VisibleShapeCache,
}

#[derive(Default)]
pub struct VisiblePageGeometry {
    start_line: Option<usize>,
    end_line: Option<usize>,
    line_positions: VisibleLinePositions,
    paint_start_offset_y: f64,
    uses_cached_inline_positions: bool,
    offset_breaks: Vec<(f64, f64)>,
    repeated_table_headers: Vec<RepeatedTableHeader>,
    block_decoration_indices: Option<Vec<usize>>,
}

impl VisiblePageGeometry {
    pub fn start_line(&self) -> Option<usize> {
        self.start_line
    }

    pub fn end_line(&self) -> Option<usize> {
        self.end_line
    }

    pub fn line_positions(&self) -> &VisibleLinePositions {
        &self.line_positions
    }

    pub fn paint_start_offset_y(&self) -> f64 {
        self.paint_start_offset_y
    }

    pub fn uses_cached_inline_positions(&self) -> bool {
        self.uses_cached_inline_positions
    }

    pub fn offset_breaks(&self) -> &[(f64, f64)] {
        &self.offset_breaks
    }

    pub fn repeated_table_headers(&self) -> &[RepeatedTableHeader] {
        &self.repeated_table_headers
    }

    pub fn block_decoration_indices(&self) -> Option<&[usize]> {
        self.block_decoration_indices.as_deref()
    }
}

#[derive(Default)]
pub struct VisibleShapeCache {
    shaped_lines: Vec<VisibleShapedLineFragment>,
    native_failures: HashSet<usize>,
    key: Option<LineShapeFrameKey>,
}

impl VisibleShapeCache {
    pub fn shaped_lines(&self) -> &[VisibleShapedLineFragment] {
        &self.shaped_lines
    }

    pub fn native_failures(&self) -> &HashSet<usize> {
        &self.native_failures
    }

    pub fn key(&self) -> Option<LineShapeFrameKey> {
        self.key
    }
}

#[derive(Clone, Copy)]
enum InlinePositionMode {
    Flow,
    Recorded,
}

impl VisibleFrame {
    pub fn page(&self) -> &VisiblePageGeometry {
        &self.page
    }

    pub fn shaping(&self) -> &VisibleShapeCache {
        &self.shaping
    }

    pub fn geometry_key(&self) -> Option<FrameGeometryCacheKey> {
        self.geometry_key
    }

    pub fn set_geometry_key(&mut self, key: FrameGeometryCacheKey) {
        self.geometry_key = Some(key);
    }

    /// Repositions an already paginated page when only horizontal column
    /// geometry changed. Line membership, vertical positions, and semantic
    /// boundaries remain valid.
    pub fn reproject_columns(
        &mut self,
        old_column_width: f64,
        old_column_gap: f64,
        new_column_width: f64,
        new_column_gap: f64,
    ) {
        for screen in &mut self.page.line_positions.positions {
            let column = f64::from(screen.col_index);
            let old_origin = old_column_gap + column * (old_column_width + old_column_gap);
            let new_origin = new_column_gap + column * (new_column_width + new_column_gap);
            screen.point.x += new_origin - old_origin;
        }
    }

    pub fn take_line_positions(&mut self) -> VisibleLinePositions {
        std::mem::take(&mut self.page.line_positions)
    }

    /// Frames a document whose complete line geometry is visible at once.
    pub fn install_whole_document(
        &mut self,
        line_count: usize,
        positions: VisibleLinePositions,
    ) -> VisibleLinePositions {
        let previous = std::mem::replace(&mut self.page.line_positions, positions);
        self.page.start_line = Some(0);
        self.page.end_line = Some(line_count);
        self.page.paint_start_offset_y = 0.0;
        self.page.uses_cached_inline_positions = false;
        self.page.offset_breaks.clear();
        self.page.repeated_table_headers.clear();
        self.page.block_decoration_indices = None;
        previous
    }

    /// Installs an ordinary forward page whose inline geometry is resolved
    /// from document flow during painting.
    pub fn install_flow_page(
        &mut self,
        page: PagePositions,
        paint_start_offset_y: f64,
        paint_end_line: Option<usize>,
    ) -> VisibleLinePositions {
        self.install_page(
            page,
            paint_start_offset_y,
            paint_end_line,
            InlinePositionMode::Flow,
        )
    }

    /// Installs a page that has already been positioned, such as backward
    /// composition.
    pub fn install_positioned_page(
        &mut self,
        page: PagePositions,
        paint_start_offset_y: f64,
        paint_end_line: Option<usize>,
    ) -> VisibleLinePositions {
        self.install_page(
            page,
            paint_start_offset_y,
            paint_end_line,
            InlinePositionMode::Recorded,
        )
    }

    fn install_page(
        &mut self,
        page: PagePositions,
        paint_start_offset_y: f64,
        paint_end_line: Option<usize>,
        inline_positions: InlinePositionMode,
    ) -> VisibleLinePositions {
        let PagePositions {
            start_line,
            positions,
            offset_breaks,
            repeated_table_headers,
            block_decoration_indices,
            ..
        } = page;
        let previous_positions = std::mem::replace(&mut self.page.line_positions, positions);
        self.page.start_line = Some(start_line);
        self.page.end_line = paint_end_line;
        self.page.paint_start_offset_y = paint_start_offset_y;
        self.page.uses_cached_inline_positions =
            matches!(inline_positions, InlinePositionMode::Recorded);
        self.page.offset_breaks = offset_breaks;
        self.page.repeated_table_headers = repeated_table_headers;
        self.page.block_decoration_indices = Some(block_decoration_indices);
        previous_positions
    }

    pub fn clear_backend_shaping(&mut self) {
        self.shaping.shaped_lines.clear();
        self.shaping.native_failures.clear();
        self.shaping.key = None;
    }

    pub fn record_native_shape_failure(&mut self, line: usize) {
        self.shaping.native_failures.insert(line);
    }

    pub fn clear(&mut self) {
        self.page.start_line = None;
        self.page.end_line = None;
        self.page.line_positions.clear();
        self.page.paint_start_offset_y = 0.0;
        self.page.uses_cached_inline_positions = false;
        self.page.offset_breaks.clear();
        self.page.repeated_table_headers.clear();
        self.page.block_decoration_indices = None;
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
        Self {
            match_case: false,
            whole_word: false,
            match_diacritics: false,
            scope: SearchScope::WholeBook,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BookSearchMatch {
    pub doc: usize,
    pub target: BookSearchTarget,
    pub excerpt: String,
}

#[derive(Clone, Debug)]
pub enum BookSearchTarget {
    /// A durable source address produced without laying out the document.
    SourceCfi(String),
    /// Current-document search already owns its glyph matches.
    LocalOccurrence(usize),
}

#[cfg(test)]
mod renderer_revision_tests {
    use super::*;

    #[test]
    fn popup_anchor_corner_opens_into_the_roomiest_viewport_quadrant() {
        let viewport = Size::new(800.0, 600.0);
        let top_left = FootnotePopupAnchor::at_click(Point::new(100.0, 100.0), viewport);
        assert_eq!(top_left.corner, PopupAnchorCorner::TopLeft);
        assert_eq!(
            top_left.popup_origin(Size::new(240.0, 160.0)),
            top_left.point
        );
        assert_eq!(
            FootnotePopupAnchor::at_click(Point::new(700.0, 100.0), viewport).corner,
            PopupAnchorCorner::TopRight
        );
        assert_eq!(
            FootnotePopupAnchor::at_click(Point::new(100.0, 500.0), viewport).corner,
            PopupAnchorCorner::BottomLeft
        );
        assert_eq!(
            FootnotePopupAnchor::at_click(Point::new(700.0, 500.0), viewport).corner,
            PopupAnchorCorner::BottomRight
        );
    }

    #[test]
    fn forward_composition_clears_transient_backward_and_continuation_state() {
        let mut viewport = ViewportState::default();
        viewport.begin_backward_at(2.0, Some(4));
        viewport.set_backward_composition_start(3.0);
        viewport.update_forward_result(ViewportContinuation::EndedAt(5.0));

        viewport.begin_forward_at(12.0);

        let composition = viewport.composition();
        let backward = viewport.backward_composition();
        let continuation = viewport.continuation();
        assert!(matches!(composition.direction(), PaintDirection::Forward));
        assert_eq!(composition.start_offset_y(), 12.0);
        assert_eq!(composition.end_offset_y(), 12.0);
        assert_eq!(backward.start_offset_y(), 0.0);
        assert_eq!(backward.anchor_end_line(), None);
        assert!(!continuation.reached_end());
        assert_eq!(continuation.next_start_offset_y(), 12.0);
    }

    #[test]
    fn backward_composition_installs_its_end_anchor() {
        let mut viewport = ViewportState::default();

        viewport.begin_backward_at(24.0, Some(7));

        assert!(matches!(
            viewport.composition().direction(),
            PaintDirection::Backward
        ));
        assert_eq!(viewport.composition().end_offset_y(), 24.0);
        assert_eq!(viewport.backward_composition().start_offset_y(), 0.0);
        assert_eq!(viewport.backward_composition().anchor_end_line(), Some(7));
        assert!(!viewport.continuation().reached_end());

        viewport.finish_backward_composition(8.0);
        assert_eq!(viewport.composition().start_offset_y(), 8.0);
        assert_eq!(viewport.continuation().next_start_offset_y(), 24.0);
    }

    #[test]
    fn cached_forward_page_installs_its_continuation() {
        let mut viewport = ViewportState::default();

        viewport.install_forward_page(10.0, ViewportContinuation::EndedAt(30.0));

        assert!(matches!(
            viewport.composition().direction(),
            PaintDirection::Forward
        ));
        assert_eq!(viewport.composition().start_offset_y(), 10.0);
        assert_eq!(viewport.composition().end_offset_y(), 10.0);
        assert!(viewport.continuation().reached_end());
        assert_eq!(viewport.continuation().next_start_offset_y(), 30.0);
    }

    #[test]
    fn visible_frame_installs_page_positions_as_one_transition() {
        let mut frame = VisibleFrame::default();
        let mut previous_positions = VisibleLinePositions::default();
        previous_positions.push(LineScreen {
            point: Point::new(1.0, 2.0),
            col_index: 0,
        });
        frame.install_whole_document(1, previous_positions);
        let mut positions = VisibleLinePositions::with_buffer(4, Vec::new());
        positions.push(LineScreen {
            point: Point::new(3.0, 4.0),
            col_index: 1,
        });
        let page = PagePositions {
            start_line: 4,
            last_line: Some(8),
            positions,
            offset_breaks: vec![(10.0, 20.0)],
            repeated_table_headers: vec![RepeatedTableHeader {
                source_left: 1.0,
                source_right: 2.0,
                source_top: 3.0,
                source_bottom: 4.0,
                col_index: 1,
            }],
            block_decoration_indices: vec![2, 5],
            reached_end: false,
            next_start_offset_y: 50.0,
        };

        let previous = frame.install_positioned_page(page, 12.0, Some(7));

        assert_eq!(previous.len(), 1);
        assert_eq!(frame.page().start_line(), Some(4));
        assert_eq!(frame.page().end_line(), Some(7));
        assert_eq!(frame.page().paint_start_offset_y(), 12.0);
        assert!(frame.page().uses_cached_inline_positions());
        assert_eq!(frame.page().line_positions().first_line(), 4);
        assert_eq!(frame.page().offset_breaks(), vec![(10.0, 20.0)]);
        assert_eq!(frame.page().repeated_table_headers().len(), 1);
        assert_eq!(frame.page().block_decoration_indices(), Some(&[2, 5][..]));
    }

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
