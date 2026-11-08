//! Data types shared across the reader crates.
//!
//! Everything here is state or a contract: column geometry, viewport and frame
//! bookkeeping, the command and event vocabulary, and the cache keys that decide
//! when painted output can be reused. No document querying, pagination or
//! navigation logic lives here -- those are separate crates that agree on these
//! types.

#[cfg(test)]
use html_engine_model::page_placements;
use std::fmt;
use std::mem::{Discriminant, take};
use std::sync::Arc;

use kurbo::{Point, Size};
pub use peniko::Color;

use html::document::ImageId;
#[cfg(test)]
use html::layout::LineId;
use html::pagination::{PageAnchor, PageRequest};
use html::resources::{BlockingJobSpawner, InlineJobSpawner, ResourceProvider};
use html::revisions::DocumentRevisions;
use html::text::FontFace;

pub use html::render::OverlayStyle as AnnotationStyle;

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
    pub image: ImageId,
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
    /// The passage being read aloud.
    pub reading: Color,
}

impl Default for InteractionPalette {
    fn default() -> Self {
        Self {
            selection: Color::rgba8(151, 188, 159, 112),
            search_match: Color::rgba8(232, 193, 78, 116),
            active_search_match: Color::rgba8(190, 116, 55, 164),
            annotation: Color::rgba8(236, 204, 91, 104),
            active_annotation: Color::rgba8(196, 126, 57, 148),
            reading: Color::rgba8(151, 188, 159, 56),
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
    /// Original OPF addresses aligned with the renderer's document list.
    /// Non-EPUB sources leave this unset and use synthetic document addresses.
    pub publication_cfi_bases: Option<Vec<Option<String>>>,
    pub font_size: f32,
    pub column_width: f64,
    pub scale: f64,
    /// Optional presentation cap. `None` lets the viewport determine the
    /// number of columns from the requested column width.
    pub max_column_count: Option<u8>,
    pub style_overrides: html::style::ReaderStyleOverrides,
    pub image_sizing_policy: html::layout::ImageSizingPolicy,
    pub text_composition_policy: html::layout::TextCompositionPolicy,
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
            publication_cfi_bases: None,
            font_size: 20.0,
            column_width: 600.0,
            scale: 1.0,
            max_column_count: None,
            style_overrides: Default::default(),
            image_sizing_policy: html::layout::ImageSizingPolicy::SmartStandalone,
            text_composition_policy: html::layout::TextCompositionPolicy::BookOptimized,
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
    /// A user jump, unlike SetCfiPosition which restores without history.
    NavigateToCfi(String),
    NavigateDocument(i8),
    /// One page forward for positive steps, backward for negative; zero does nothing.
    NavigatePage(i8),
    /// One line forward for positive steps, backward for negative; zero does nothing.
    NavigateLine(i8),
    SetPosition {
        doc: usize,
        glyph: Option<u32>,
    },
    SetCfiPosition(Option<String>),
    SetProgressFraction(f32),
    SetDocumentTextLengths(Vec<u64>),
    SetColumnWidth(f64),
    SetColumnViewportPlan {
        available_width: f64,
        column_width: f64,
        column_count: u8,
        column_gap: f64,
    },
    SetMaxColumnCount(Option<u8>),
    SetFontSize(f32),
    /// Rebuild text after the host registers newly available publication fonts.
    RefreshFonts,
    SetReaderStyleOverrides(html::style::ReaderStyleOverrides),
    SetImageSizingPolicy(html::layout::ImageSizingPolicy),
    /// Switches how notes are presented. This re-lays-out the document, since
    /// whether a note occupies the reading flow decides what boxes exist.
    SetNoteDisplay(NoteDisplay),
    SetTextCompositionPolicy(html::layout::TextCompositionPolicy),
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
    /// Selects a zero-based match directly, preserving next/previous navigation.
    NavigateSearchTo(usize),
    SetAnnotations(Vec<RendererAnnotation>),
    /// Emphasize an open note while retaining every overlapping hit target.
    SetActiveAnnotation(Option<String>),
    UpsertAnnotation(RendererAnnotation),
    RemoveAnnotation(String),
    SetMediaOverlayTarget(Option<String>),
    SetReadingTarget(Option<html::reading::ReadingTarget>),
    RequestVisibleText,
    CopySelection,
    CopySelectionCitation {
        title: String,
        cfi: Option<String>,
    },
}

/// A publication source address, independent of glyph indices and layout.
/// Even path steps address elements; a final odd step addresses a text chunk.
/// Element addresses also represent pages containing only images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    pub document: usize,
    pub path: Vec<usize>,
    pub utf16_offset: Option<usize>,
}

/// A single snapshot from which a host derives its chapter, UI and persistence.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadingPosition {
    pub source: SourcePosition,
    pub fraction: f32,
    pub document_fraction: f32,
    pub location: u64,
    pub total_locations: u64,
    pub document_count: usize,
}

#[derive(Clone, Debug)]
pub enum RendererEvent {
    /// The composed page changed, including a reflow at the same source anchor.
    VisiblePageChanged,
    /// A deliberate jump committed; emitted before its settled position.
    NavigationJump {
        from: String,
    },
    /// One settled page location and its progress, emitted after pagination.
    ReadingPositionChanged(ReadingPosition),
    TitleChanged(Option<String>),
    MatchInfo {
        current: usize,
        total: usize,
    },
    SearchResults(BookSearchResults),
    SearchResultsAppended(Arc<[BookSearchMatch]>),
    SearchActiveChanged(bool),
    ColumnWidthChanged(f64),
    FontSizeChanged(f32),
    ScaleChanged(f64),
    /// The selected source address; hosts request text only when an action needs it.
    SelectionFinished {
        doc: usize,
        cfi_range: String,
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
    CopyImage,
    CopyEquationLatex,
    PreviewNote,
    UpdateDocument,
}

impl fmt::Display for RendererOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CopyImage => "copy image",
            Self::CopyEquationLatex => "copy equation as LaTeX",
            Self::PreviewNote => "preview footnote",
            Self::UpdateDocument => "update document rendering",
        })
    }
}

/// Effects produced by renderer operations. The renderer only appends; the
/// embedding shell drains them after each call and performs them, so the
/// renderer never calls back into its host while an operation is running.
#[derive(Default)]
pub struct RendererEffects {
    /// Displayed content or interaction state changed.
    pub repaint: bool,
    /// Reader appearance changed and the host should restyle its chrome.
    pub restyle: bool,
    pub events: Vec<RendererEvent>,
    pub clipboard: Vec<ClipboardWrite>,
    /// Publication font declarations to load. The engine selects declared
    /// faces for shaping; once a host has registered loaded faces with its
    /// text backend it issues `RendererCommand::RefreshFonts`.
    pub font_loads: Vec<FontLoadRequest>,
}

impl RendererEffects {
    pub fn request_repaint(&mut self) {
        self.repaint = true;
    }

    pub fn request_restyle(&mut self) {
        self.restyle = true;
    }

    pub fn emit(&mut self, event: RendererEvent) {
        self.events.push(event);
    }

    pub fn is_empty(&self) -> bool {
        !self.repaint
            && !self.restyle
            && self.events.is_empty()
            && self.clipboard.is_empty()
            && self.font_loads.is_empty()
    }

    /// Moves every pending effect into `out`, preserving order and leaving
    /// this buffer empty with its capacity retained.
    pub fn drain_into(&mut self, out: &mut RendererEffects) {
        out.repaint |= take(&mut self.repaint);
        out.restyle |= take(&mut self.restyle);
        out.events.append(&mut self.events);
        out.clipboard.append(&mut self.clipboard);
        out.font_loads.append(&mut self.font_loads);
    }
}

/// An owned clipboard payload for the shell to write. Platform failures are
/// handled by the shell; renderer export failures remain renderer events.
pub enum ClipboardWrite {
    Text(String),
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    Svg(Vec<u8>),
}

/// Shared publication declarations and whether reader font roles need them.
pub struct FontLoadRequest {
    pub faces: Arc<[FontFace]>,
    pub use_publication_fonts: bool,
    pub provider: Arc<dyn ResourceProvider>,
}

/// Platform capabilities supplied once when a renderer is created. They are
/// values the renderer holds, not callbacks into a host.
#[derive(Clone)]
pub struct RendererServices {
    /// Finite blocking or CPU work, without coupling the renderer to a thread
    /// pool, async runtime or Web Worker machinery.
    pub blocking_jobs: Arc<dyn BlockingJobSpawner>,
    /// Chapter preparation can use a stable worker so successive chapters
    /// reuse one allocator arena.
    pub chapter_preparation: Arc<dyn BlockingJobSpawner>,
    /// Wakes the shell when background resource work completes.
    pub resource_waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for RendererServices {
    /// Synchronous services for simple embedders.
    fn default() -> Self {
        let inline: Arc<dyn BlockingJobSpawner> = Arc::new(InlineJobSpawner);
        Self {
            blocking_jobs: inline.clone(),
            chapter_preparation: inline,
            resource_waker: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererRevisions {
    pub layout: u64,
    pub resources: u64,
    pub base_display: u64,
    pub overlay: u64,
}

impl Default for RendererRevisions {
    fn default() -> Self {
        Self {
            layout: 1,
            resources: 1,
            base_display: 1,
            overlay: 1,
        }
    }
}

impl RendererRevisions {
    fn next(value: &mut u64) {
        *value = value.wrapping_add(1).max(1);
    }
    /// Advance host cache counters for changed document products.
    pub fn update_document(&mut self, before: DocumentRevisions, after: DocumentRevisions) {
        if before == after {
            return;
        }
        if after.layout_changed_since(before) {
            Self::next(&mut self.layout);
        }
        if after.resources_changed_since(before) {
            Self::next(&mut self.resources);
        }
        Self::next(&mut self.base_display);
        Self::next(&mut self.overlay);
    }

    pub fn replace_document(&mut self) {
        Self::next(&mut self.layout);
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
    pub geometry: FrameGeometryCacheKey,
    pub display_revision: u64,
    pub resources_revision: u64,
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
    /// Request variant and its cursor or bit-exact document offset.
    pub page_request: (Discriminant<PageRequest>, u64),
}

impl FrameGeometryCacheKey {
    /// Page requests do not affect layout compatibility.
    pub fn same_layout(self, other: Self) -> bool {
        Self {
            page_request: other.page_request,
            ..self
        } == other
    }
}

#[derive(Clone, Copy)]
pub enum DocAnchor {
    Start,
    End,
    Glyph(u32),
}

pub use html::pagination::{LineScreen, PagePositions, RepeatedTableHeader, VisibleLinePositions};
use html::pagination::PageGeometry;

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

    /// Compose columns at the supplied viewport without changing document width.
    pub fn configure_viewport(&mut self, viewport: Size) {
        self.size = viewport / self.scale;
        self.recompute_columns();
    }

    /// Derive column count and gap from the committed width and viewport.
    pub fn recompute_columns(&mut self) {
        let raw_count = (self.size.width / self.col_width).floor();
        self.col_count = self.max_col_count.map_or(raw_count.max(1.0), |maximum| {
            raw_count.max(1.0).min(f64::from(maximum))
        });
        // Side margins belong to the host. Spread slack between multiple
        // columns; a lone column is centered by `col_x` instead.
        self.col_gap = if self.col_count > 1.0 {
            ((self.size.width - self.col_count * self.col_width) / (self.col_count - 1.0)).max(0.0)
        } else {
            0.0
        };
    }

    pub fn commit_column_width(&mut self, base_width: Option<f64>, effective_width: Option<f64>) {
        if let Some(width) = base_width {
            self.base_col_width = width;
        }
        if let Some(width) = effective_width {
            self.col_width = width;
        }
        // Width, count, and gap are one piece of viewport geometry. Keeping a
        // previous gap after a new width was committed left a wide blank band
        // between columns even though the new width could consume it.
        self.recompute_columns();
    }

    pub fn update_scale(&mut self, requested: f64) -> Option<f64> {
        let scale = requested.clamp(0.5, 3.0);
        if (scale - self.scale).abs() < f64::EPSILON {
            return None;
        }
        self.scale = scale;
        Some(scale)
    }


    pub fn page_geometry(&self) -> PageGeometry {
        PageGeometry {
            col_width: self.col_width,
            col_count: self.col_count,
            col_gap: self.col_gap,
            size: self.size,
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

#[derive(Clone, Copy, Default)]
pub struct ViewportState {
    /// Composition to prepare, or the last installed page's resolved request.
    request: PageRequest,
    /// Viewport origin; backward requests retain it until a page is installed.
    start_offset_y: f64,
}

impl ViewportState {
    /// Explicit forward pagination cursor when selected by navigation.
    pub fn start_line(self) -> Option<usize> {
        match self.request {
            PageRequest::StartingAt(cursor) => Some(cursor),
            _ => None,
        }
    }

    pub fn request(self) -> PageRequest {
        self.request
    }

    pub fn start_offset_y(self) -> f64 {
        self.start_offset_y
    }

    /// Starts a fresh forward composition whose screen begins at `offset_y`.
    pub fn begin_forward_at(&mut self, offset_y: f64) {
        *self = Self {
            request: PageRequest::StartingAtOffset(offset_y),
            start_offset_y: offset_y,
        };
    }

    /// Starts at an engine-resolved page boundary, retaining its group identity.
    pub fn begin_forward_at_anchor(&mut self, anchor: PageAnchor) {
        self.begin_forward_at(anchor.offset_y);
        self.request = anchor.cursor.map_or(
            PageRequest::StartingAtOffset(anchor.offset_y),
            PageRequest::StartingAt,
        );
    }

    /// Starts a fresh backward composition ending at the supplied document
    /// offset and, when known, line.
    pub fn begin_backward_at(&mut self, end_offset_y: f64, anchor_end_line: Option<usize>) {
        self.request = anchor_end_line.map_or(
            PageRequest::EndingAtOffset(end_offset_y),
            PageRequest::EndingAt,
        );
    }

    /// Installs the resolved composition without interpreting its direction or
    /// document boundaries. Continuation remains on the installed engine page.
    pub fn install_page(&mut self, page: &PagePositions) {
        *self = Self {
            request: page.request,
            start_offset_y: page.paint_start_offset_y,
        };
    }
}

#[derive(Default)]
pub struct VisibleFrame {
    page: VisiblePageGeometry,
    geometry_key: Option<FrameGeometryCacheKey>,
}

#[derive(Default)]
pub struct VisiblePageGeometry {
    installed: bool,
    content: PagePositions,
}

impl VisiblePageGeometry {
    pub fn start_line(&self) -> Option<usize> {
        self.installed.then_some(self.content.start_line)
    }
    pub fn end_line(&self) -> Option<usize> {
        self.content.last_line
    }
    pub fn line_positions(&self) -> &VisibleLinePositions {
        &self.content.positions
    }
    pub fn paint_start_offset_y(&self) -> f64 {
        self.content.paint_start_offset_y
    }
    pub fn offset_breaks(&self) -> &[(f64, f64)] {
        &self.content.offset_breaks
    }
    pub fn repeated_table_headers(&self) -> &[RepeatedTableHeader] {
        &self.content.repeated_table_headers
    }
    pub fn block_decoration_indices(&self) -> Option<&[usize]> {
        self.content.block_decoration_indices.as_deref()
    }
    pub fn content(&self) -> &PagePositions {
        &self.content
    }
}

impl VisibleFrame {
    pub fn page(&self) -> &VisiblePageGeometry {
        &self.page
    }
    pub fn geometry_key(&self) -> Option<FrameGeometryCacheKey> {
        self.geometry_key
    }
    pub fn set_geometry_key(&mut self, key: FrameGeometryCacheKey) {
        self.geometry_key = Some(key);
    }

    pub fn reproject_columns(&mut self, geometry: PageGeometry) {
        self.page.content.reproject_columns(geometry);
    }

    pub fn take_line_positions(&mut self) -> VisibleLinePositions {
        take(&mut self.page.content.positions)
    }

    /// Install the complete engine result without rebuilding its projection.
    pub fn install_page(&mut self, page: PagePositions) -> VisibleLinePositions {
        self.page.installed = true;
        std::mem::replace(&mut self.page.content, page).positions
    }

    pub fn clear(&mut self) {
        self.page.installed = false;
        self.page.content.clear();
        self.geometry_key = None;
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

/// Search matches retained in immutable batches. Snapshots share match strings;
/// streamed batches append without copying the already published prefix.
#[derive(Clone, Debug, Default)]
pub struct BookSearchResults {
    // Each batch carries its exclusive end in the combined result set.
    batches: Vec<(usize, Arc<[BookSearchMatch]>)>,
}

impl BookSearchResults {
    pub fn len(&self) -> usize {
        self.batches.last().map_or(0, |(end, _)| *end)
    }
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }
    pub fn clear(&mut self) {
        self.batches.clear();
    }
    pub fn get(&self, index: usize) -> Option<&BookSearchMatch> {
        let batch = self.batches.partition_point(|(end, _)| *end <= index);
        let (end, values) = self.batches.get(batch)?;
        values.get(index - (*end - values.len()))
    }
    pub fn iter(&self) -> impl Iterator<Item = &BookSearchMatch> {
        self.batches.iter().flat_map(|(_, values)| values.iter())
    }
    pub fn append(&mut self, batch: Arc<[BookSearchMatch]>) {
        if !batch.is_empty() {
            self.batches.push((self.len() + batch.len(), batch));
        }
    }
}

impl From<Vec<BookSearchMatch>> for BookSearchResults {
    fn from(matches: Vec<BookSearchMatch>) -> Self {
        let mut results = Self::default();
        results.append(matches.into());
        results
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
    fn multiple_column_slack_stays_between_columns_and_single_column_is_centered() {
        let mut layout = ColumnLayout {
            col_width: 300.0,
            base_col_width: 300.0,
            max_col_count: Some(2),
            ..Default::default()
        };
        layout.configure_viewport(Size::new(800.0, 600.0));
        assert_eq!(layout.col_count, 2.0);
        assert_eq!(layout.col_gap, 200.0);
        assert_eq!(
            page_placements::col_x(&layout.page_geometry(), 0.0, 0.0),
            0.0
        );
        assert_eq!(
            page_placements::col_x(&layout.page_geometry(), 1.0, 0.0),
            500.0
        );

        layout.max_col_count = Some(1);
        layout.recompute_columns();
        assert_eq!(layout.col_gap, 0.0);
        assert_eq!(
            page_placements::col_x(&layout.page_geometry(), 0.0, 0.0),
            250.0
        );
        assert_eq!(
            page_placements::col_x(&layout.page_geometry(), 0.0, 300.0),
            550.0
        );

        layout.max_col_count = Some(2);
        layout.col_width = 600.0;
        layout.base_col_width = 600.0;
        layout.configure_viewport(Size::new(1200.0, 600.0));
        assert_eq!(layout.col_count, 2.0);
        assert_eq!(layout.col_gap, 0.0);
    }

    #[test]
    fn resizing_a_single_column_reprojects_its_centered_lines() {
        let mut positions = page_placements::with_buffer(0, Vec::new());
        page_placements::push(
            &mut positions,
            LineId::new(0),
            LineScreen {
                point: Point::new(250.0, 12.0),
                col_index: 0,
            },
        );
        let mut frame = VisibleFrame::default();
        frame.install_page(PagePositions {
            geometry: PageGeometry {
                col_width: 300.0,
                col_count: 1.0,
                col_gap: 0.0,
                size: Size::new(800.0, 600.0),
            },
            positions,
            last_line: Some(0),
            ..PagePositions::default()
        });

        frame.reproject_columns(PageGeometry {
            col_width: 300.0,
            col_count: 1.0,
            col_gap: 0.0,
            size: Size::new(1000.0, 600.0),
        });
        let (_, line) = frame.page().line_positions().iter().next().unwrap();
        assert_eq!(line.point, Point::new(350.0, 12.0));
    }

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
    fn forward_composition_clears_transient_backward_state() {
        let mut viewport = ViewportState::default();
        viewport.begin_backward_at(2.0, Some(4));

        viewport.begin_forward_at(12.0);

        assert!(matches!(
            viewport.request(),
            PageRequest::StartingAtOffset(offset) if offset == 12.0
        ));
        assert_eq!(viewport.start_offset_y(), 12.0);
    }

    #[test]
    fn backward_composition_installs_its_end_anchor() {
        let mut viewport = ViewportState::default();

        viewport.begin_backward_at(24.0, Some(7));

        assert!(matches!(
            viewport.request(),
            PageRequest::EndingAt(7)
        ));
        let page = PagePositions {
            request: PageRequest::EndingAt(7),
            paint_start_offset_y: 8.0,
            next_start_offset_y: 24.0,
            ..PagePositions::default()
        };
        viewport.install_page(&page);
        assert_eq!(viewport.start_offset_y(), 8.0);
        assert!(matches!(
            viewport.request(),
            PageRequest::EndingAt(7)
        ));
    }

    #[test]
    fn cached_forward_page_installs_its_composition() {
        let mut viewport = ViewportState::default();

        let page = PagePositions {
            request: PageRequest::StartingAtOffset(10.0),
            paint_start_offset_y: 10.0,
            reached_end: true,
            next_start_offset_y: 30.0,
            ..PagePositions::default()
        };
        viewport.install_page(&page);

        assert!(matches!(
            viewport.request(),
            PageRequest::StartingAtOffset(offset) if offset == 10.0
        ));
        assert_eq!(viewport.start_offset_y(), 10.0);
    }

    #[test]
    fn visible_frame_installs_page_positions_as_one_transition() {
        let mut frame = VisibleFrame::default();
        let mut previous_positions = VisibleLinePositions::default();
        page_placements::push(
            &mut previous_positions,
            LineId::new(0),
            LineScreen {
                point: Point::new(1.0, 2.0),
                col_index: 0,
            },
        );
        frame.install_page(PagePositions {
            positions: previous_positions,
            last_line: Some(0),
            ..PagePositions::default()
        });
        let mut positions = page_placements::with_buffer(4, Vec::new());
        page_placements::push(
            &mut positions,
            LineId::new(4),
            LineScreen {
                point: Point::new(3.0, 4.0),
                col_index: 1,
            },
        );
        let page = PagePositions {
            request: PageRequest::StartingAt(4),
            geometry: PageGeometry::default(),
            paint_start_offset_y: 12.0,
            start_line: 4,
            last_line: Some(7),
            positions,
            offset_breaks: vec![(10.0, 20.0)],
            repeated_table_headers: vec![RepeatedTableHeader {
                source_left: 1.0,
                source_right: 2.0,
                source_top: 3.0,
                source_bottom: 4.0,
                col_index: 1,
            }],
            block_decoration_indices: Some(vec![2, 5]),
            reached_end: false,
            next_start_offset_y: 50.0,
        };

        let previous = frame.install_page(page);

        assert_eq!(previous.iter().count(), 1);
        assert_eq!(frame.page().start_line(), Some(4));
        assert_eq!(frame.page().end_line(), Some(7));
        assert_eq!(frame.page().paint_start_offset_y(), 12.0);
        assert!(page_placements::at_cursor(frame.page().line_positions(), 4).is_some());
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
    fn resource_invalidation_keeps_interaction_overlay_reusable() {
        let mut revisions = RendererRevisions::default();
        let overlay = revisions.overlay;
        revisions.invalidate_resources();
        assert_ne!(revisions.resources, 1);
        assert_ne!(revisions.base_display, 1);
        assert_eq!(revisions.overlay, overlay);
    }
}

impl From<SearchOptions> for html::text::TextSearchOptions {
    fn from(options: SearchOptions) -> Self {
        Self {
            match_case: options.match_case,
            whole_word: options.whole_word,
            match_diacritics: options.match_diacritics,
        }
    }
}
