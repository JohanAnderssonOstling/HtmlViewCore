//! Painting one laid-out page.
//!
//! Everything here reads: a document, the frame currently laid out, column
//! geometry, palettes, decoded images and the overlay state. Nothing calls back
//! into the renderer, which is why painting can be driven from a bare
//! [`PagePainter`] and exercised with a recording painter in tests.
//!
//! The overlay state types live here too -- what to highlight and how to draw it
//! are the same concern.

use html::render::Painter;
use html::resources::ImagePipeline;
use html_view_doc::{DocQuery, TextGeometry, scaled_image_size};
use html_view_select::SelectionView;
use html_view_types::{
    AnnotationStyle, BookSearchMatch, BookSearchTarget, InteractionPalette, ReaderPaintPalette,
    RenderState, RendererAnnotation, RendererEvent, RendererHost, SearchOptions, SearchScope,
    VisibleLinePositions,
};
use kurbo::{Point, Rect};
use peniko::Color;

#[derive(Default)]
pub struct AnnotationOverlayState {
    source: Vec<RendererAnnotation>,
    visible: Vec<(String, u32, u32, AnnotationStyle, [u8; 4])>,
}

#[derive(Default)]
pub struct MediaOverlayHighlightState {
    target_href: Option<String>,
    visible: Option<(u32, u32)>,
}

#[derive(Default)]
pub struct HighlightState {
    query: String,
    matches: Vec<(u32, u32)>,
    book_matches: Vec<BookSearchMatch>,
    current_match: usize,
    current_local_match: usize,
    search_active: bool,
    options: SearchOptions,
}

#[derive(Clone, Copy)]
enum SearchProjectionState {
    Active,
    Inactive,
}

impl HighlightState {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn options(&self) -> SearchOptions {
        self.options
    }

    pub fn is_active(&self) -> bool {
        self.search_active
    }

    pub fn matches(&self) -> &[(u32, u32)] {
        &self.matches
    }

    pub fn current_match_index(&self) -> usize {
        self.current_match
    }

    pub fn install_visible_matches(&mut self, matches: Vec<(u32, u32)>) {
        self.matches = matches;
    }

    fn projected_with_state(
        query: String,
        state: SearchProjectionState,
        options: SearchOptions,
        matches: Vec<(u32, u32)>,
        current_local_match: usize,
    ) -> Self {
        Self {
            query,
            matches,
            current_local_match,
            search_active: matches!(state, SearchProjectionState::Active),
            options,
            ..Self::default()
        }
    }

    pub fn projected_active(
        query: String,
        options: SearchOptions,
        matches: Vec<(u32, u32)>,
        current_local_match: usize,
    ) -> Self {
        Self::projected_with_state(
            query,
            SearchProjectionState::Active,
            options,
            matches,
            current_local_match,
        )
    }

    pub fn projected_inactive(
        query: String,
        options: SearchOptions,
        matches: Vec<(u32, u32)>,
        current_local_match: usize,
    ) -> Self {
        Self::projected_with_state(
            query,
            SearchProjectionState::Inactive,
            options,
            matches,
            current_local_match,
        )
    }

    /// Options for restricting a search to the document on screen.
    pub fn current_document_options(&self) -> SearchOptions {
        SearchOptions {
            scope: SearchScope::CurrentDocument,
            ..self.options
        }
    }

    pub fn activate(&mut self) -> bool {
        let changed = !self.search_active;
        self.search_active = true;
        changed
    }

    pub fn deactivate(&mut self) -> bool {
        let changed = self.search_active;
        self.search_active = false;
        changed
    }

    /// Adopts a new query, dropping every result and cursor from the old one.
    pub fn restart(&mut self, query: String, options: SearchOptions) {
        self.query = query;
        self.options = options;
        self.book_matches.clear();
        self.matches.clear();
        self.current_match = 0;
        self.current_local_match = 0;
    }

    /// Records whole-document matches and mirrors them as book matches, which
    /// is how a current-document search reports results.
    pub fn set_local_matches(&mut self, doc: usize, matches: Vec<(u32, u32)>) {
        self.book_matches = matches
            .iter()
            .enumerate()
            .map(|(occurrence, _)| BookSearchMatch {
                doc,
                target: BookSearchTarget::LocalOccurrence(occurrence),
                excerpt: self.query.clone(),
            })
            .collect();
        self.matches = matches;
    }

    pub fn set_results(
        &mut self,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) {
        self.restart(query, options);
        self.book_matches = results;
    }

    /// Appends a streamed batch. Returns whether this batch made the result set
    /// non-empty, which is when the caller should jump to the first match.
    pub fn append_results(
        &mut self,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
    ) -> bool {
        if self.query != query || self.options != options {
            self.restart(query, options);
        }
        let was_empty = self.book_matches.is_empty();
        self.book_matches.extend(results);
        was_empty && !self.book_matches.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.book_matches.is_empty()
    }

    pub fn next_match(&mut self) -> bool {
        let count = self.book_matches.len();
        if count == 0 {
            return false;
        }
        self.current_match = (self.current_match + 1) % count;
        true
    }

    pub fn previous_match(&mut self) -> bool {
        let count = self.book_matches.len();
        if count == 0 {
            return false;
        }
        self.current_match = (self.current_match + count - 1) % count;
        true
    }

    pub fn current_book_match(&self) -> Option<BookSearchMatch> {
        self.book_matches.get(self.current_match).cloned()
    }

    /// Points the in-document cursor at `occurrence`, clamped to the matches
    /// actually found in the document now on screen.
    pub fn focus_local_match(&mut self, occurrence: usize) {
        self.current_local_match = occurrence.min(self.matches.len().saturating_sub(1));
    }

    pub fn current_local_match_start(&self) -> Option<u32> {
        self.matches
            .get(self.current_local_match)
            .map(|&(start, _)| start)
    }

    pub fn emit_match_signal(&self, host: &dyn RendererHost) {
        let total = self.book_matches.len();
        let current = if total == 0 {
            0
        } else {
            self.current_match + 1
        };
        host.emit(RendererEvent::MatchInfo { current, total });
    }

    pub fn emit_results(&self, host: &dyn RendererHost) {
        host.emit(RendererEvent::SearchResults(self.book_matches.clone()));
        self.emit_match_signal(host);
    }
}

impl AnnotationOverlayState {
    pub fn source(&self) -> &[RendererAnnotation] {
        &self.source
    }

    pub fn replace(&mut self, annotations: Vec<RendererAnnotation>) {
        self.source = annotations;
    }

    pub fn upsert(&mut self, annotation: RendererAnnotation) {
        self.source.retain(|item| item.id != annotation.id);
        self.source.push(annotation);
    }

    pub fn remove(&mut self, id: &str) {
        self.source.retain(|item| item.id != id);
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// Recomputes which annotations fall in the document now on screen,
    /// preferring the recorded CFI and falling back to text matching.
    pub fn resolve(&mut self, doc: DocQuery<'_>, current_doc: usize) {
        self.visible.clear();
        for annotation in &self.source {
            let by_cfi = html_view_cfi::parse_cfi_range(doc.document(), &annotation.cfi_range)
                .filter(|(document, _, _)| *document == current_doc)
                .map(|(_, start, end)| (start, end));
            if let Some((start, end)) =
                by_cfi.or_else(|| doc.recover_annotation_range(annotation, current_doc))
            {
                self.visible.push((
                    annotation.id.clone(),
                    start,
                    end,
                    annotation.style,
                    annotation.color,
                ));
            }
        }
    }

    /// Reports the topmost annotation covering `glyph`. Later annotations win,
    /// matching paint order.
    pub fn activate_at_glyph(&self, glyph: u32, host: &dyn RendererHost) -> bool {
        let Some((id, ..)) = self
            .visible
            .iter()
            .rev()
            .find(|(_, start, end, _, _)| glyph >= *start && glyph < *end)
        else {
            return false;
        };
        host.emit(RendererEvent::AnnotationActivated { id: id.clone() });
        true
    }
}

impl MediaOverlayHighlightState {
    pub fn target_href(&self) -> Option<&str> {
        self.target_href.as_deref()
    }

    pub fn set_visible(&mut self, range: Option<(u32, u32)>) {
        self.visible = range;
    }

    pub fn set_target(&mut self, target_href: Option<String>) {
        self.target_href = target_href;
        self.visible = None;
    }

    pub fn visible(&self) -> Option<(u32, u32)> {
        self.visible
    }

    pub fn resolved(target_href: Option<String>, visible: Option<(u32, u32)>) -> Self {
        Self {
            target_href,
            visible,
        }
    }
}

fn line_paint_origin(point: Point) -> Point {
    point
}

fn project_inline_decoration_rect(
    rect: Rect,
    logical_line_point: Point,
    screen_point: Point,
    extra_x_offset: f64,
    optical_offset_x: f64,
) -> Rect {
    let x = screen_point.x + extra_x_offset + optical_offset_x + (rect.x0 - logical_line_point.x);
    let y = screen_point.y + (rect.y0 - logical_line_point.y);
    Rect::new(x, y, x + rect.width(), y + rect.height())
}

fn initial_render_state() -> RenderState {
    html_view_paginate::initial_render_state()
}

#[derive(Clone, Copy)]
enum ContentLayer {
    Flow,
    Positioned,
    IndependentPositioned,
    NegativePositioned,
    IndependentNegativePositioned,
}

impl ContentLayer {
    fn flags(self) -> (bool, bool, bool) {
        match self {
            Self::Flow => (false, false, false),
            Self::Positioned => (true, false, false),
            Self::IndependentPositioned => (true, false, true),
            Self::NegativePositioned => (true, true, false),
            Self::IndependentNegativePositioned => (true, true, true),
        }
    }

    fn matches_line(self, line: &html::layout::RenderLine) -> bool {
        let (positioned, negative, independent) = self.flags();
        line.is_in_positioned_layer() == positioned
            && line.is_in_negative_positioned_layer() == negative
            && line.is_in_independent_positioned_layer() == independent
    }

    fn for_line(line: &html::layout::RenderLine) -> Self {
        let independent = line.is_in_independent_positioned_layer();
        if line.is_in_negative_positioned_layer() {
            if independent {
                Self::IndependentNegativePositioned
            } else {
                Self::NegativePositioned
            }
        } else if line.is_in_positioned_layer() {
            if independent {
                Self::IndependentPositioned
            } else {
                Self::Positioned
            }
        } else {
            Self::Flow
        }
    }

    fn matches_decoration(self, decoration: &html::layout::RenderDecoration) -> bool {
        let (positioned, negative, independent) = self.flags();
        decoration.is_in_positioned_layer() == positioned
            && decoration.is_in_negative_positioned_layer() == negative
            && decoration.is_in_independent_positioned_layer() == independent
    }
}

#[derive(Clone, Copy)]
enum PaintPhase {
    Background,
    Foreground,
}

#[derive(Clone, Copy)]
struct PaintPass {
    phase: PaintPhase,
    layer: ContentLayer,
}

impl PaintPass {
    fn background(layer: ContentLayer) -> Self {
        Self {
            phase: PaintPhase::Background,
            layer,
        }
    }

    fn foreground(layer: ContentLayer) -> Self {
        Self {
            phase: PaintPhase::Foreground,
            layer,
        }
    }

    fn is_foreground(self) -> bool {
        matches!(self.phase, PaintPhase::Foreground)
    }
}

struct DecorationSegment<'a> {
    rect: &'a Rect,
    start: f64,
    end: f64,
    y_offset: f64,
    page_start_y: f64,
    decoration: &'a html::layout::RenderDecoration,
}

/// Everything needed to paint the current page.
#[derive(Clone, Copy)]
pub struct PagePainter<'a> {
    pub geo: TextGeometry<'a>,
    pub images: &'a ImagePipeline,
    pub reader_palette: ReaderPaintPalette,
    pub interaction_palette: InteractionPalette,
    pub highlight: &'a HighlightState,
    pub annotations: &'a AnnotationOverlayState,
    pub media_overlay: &'a MediaOverlayHighlightState,
    pub selection: SelectionView<'a>,
}

impl<'a> PagePainter<'a> {
    pub fn paint_frame(&self, cx: &mut impl Painter) {
        self.paint_base_before_overlay(cx);
        self.paint_interaction_overlay(cx);
        self.paint_base_after_overlay(cx);
    }

    pub fn paint_base_before_overlay(&self, cx: &mut impl Painter) {
        let background = self
            .reader_palette
            .background
            .or_else(|| self.geo.doc.view().canvas_background_color());
        if let Some(rgba) = background {
            let [r, g, b, a] = rgba.to_be_bytes();
            cx.fill_rect(
                kurbo::Rect::new(
                    0.0,
                    0.0,
                    self.geo.layout.size.width.max(0.0),
                    self.geo.layout.size.height.max(0.0),
                ),
                peniko::Color::rgba8(r, g, b, a),
            );
        }
        let positions = self
            .geo
            .frame
            .page()
            .uses_cached_inline_positions()
            .then_some(self.geo.frame.page().line_positions());
        // Negative stacking contexts precede every in-flow background. They
        // cannot share the later generic positioned pass with z-index:auto/0
        // because doing so puts negative images above ordinary content.
        for layer in [
            ContentLayer::NegativePositioned,
            ContentLayer::IndependentNegativePositioned,
        ] {
            self.paint_decorations(
                cx,
                self.geo.frame.page().paint_start_offset_y(),
                self.geo.frame.page().offset_breaks(),
                positions,
                PaintPass::background(layer),
            );
            self.paint_repeated_table_header_decorations(cx, PaintPass::background(layer));
            let mut negative_state = initial_render_state();
            if let (Some(first_line), Some(end_line)) = (
                self.geo.frame.page().start_line(),
                self.geo.frame.page().end_line(),
            ) {
                negative_state = self.paint_base_lines_from_positions(
                    cx,
                    first_line,
                    end_line,
                    self.geo.frame.page().line_positions(),
                    negative_state,
                    layer,
                );
            }
            let _ = self.paint_repeated_table_header_lines(cx, layer, negative_state);
            self.paint_decorations(
                cx,
                self.geo.frame.page().paint_start_offset_y(),
                self.geo.frame.page().offset_breaks(),
                positions,
                PaintPass::foreground(layer),
            );
            self.paint_repeated_table_header_decorations(cx, PaintPass::foreground(layer));
        }

        let flow = ContentLayer::Flow;
        self.paint_decorations(
            cx,
            self.geo.frame.page().paint_start_offset_y(),
            self.geo.frame.page().offset_breaks(),
            positions,
            PaintPass::background(flow),
        );
        self.paint_repeated_table_header_decorations(cx, PaintPass::background(flow));
        self.paint_remaining_base(cx);
    }

    pub fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        let (Some(first_line), Some(end_line)) = (
            self.geo.frame.page().start_line(),
            self.geo.frame.page().end_line(),
        ) else {
            return;
        };
        self.paint_overlay_lines_from_positions(
            cx,
            first_line,
            end_line,
            self.geo.frame.page().line_positions(),
        );
        self.selection.paint_table_selection(cx);
    }

    /// Base content is emitted as one coherent CSS paint stream before the
    /// interaction overlay. Keeping a nominal trailing layer preserves the
    /// backend cache interface without allowing document content to obscure
    /// selection, annotation, or media overlays.
    pub fn paint_base_after_overlay(&self, _cx: &mut impl Painter) -> RenderState {
        initial_render_state()
    }

    pub fn paint_remaining_base(&self, cx: &mut impl Painter) -> RenderState {
        let mut render_state = initial_render_state();
        if let (Some(first_line), Some(end_line)) = (
            self.geo.frame.page().start_line(),
            self.geo.frame.page().end_line(),
        ) {
            render_state = self.paint_base_lines_from_positions(
                cx,
                first_line,
                end_line,
                self.geo.frame.page().line_positions(),
                render_state,
                ContentLayer::Flow,
            );
        }
        render_state = self.paint_repeated_table_header_lines(cx, ContentLayer::Flow, render_state);
        let positions = self
            .geo
            .frame
            .page()
            .uses_cached_inline_positions()
            .then_some(self.geo.frame.page().line_positions());
        self.paint_decorations(
            cx,
            self.geo.frame.page().paint_start_offset_y(),
            self.geo.frame.page().offset_breaks(),
            positions,
            PaintPass::foreground(ContentLayer::Flow),
        );
        self.paint_repeated_table_header_decorations(cx, PaintPass::foreground(ContentLayer::Flow));
        for layer in [
            ContentLayer::Positioned,
            ContentLayer::IndependentPositioned,
        ] {
            self.paint_decorations(
                cx,
                self.geo.frame.page().paint_start_offset_y(),
                self.geo.frame.page().offset_breaks(),
                positions,
                PaintPass::background(layer),
            );
            self.paint_repeated_table_header_decorations(cx, PaintPass::background(layer));
            if let (Some(first_line), Some(end_line)) = (
                self.geo.frame.page().start_line(),
                self.geo.frame.page().end_line(),
            ) {
                render_state = self.paint_base_lines_from_positions(
                    cx,
                    first_line,
                    end_line,
                    self.geo.frame.page().line_positions(),
                    render_state,
                    layer,
                );
            }
            render_state = self.paint_repeated_table_header_lines(cx, layer, render_state);
            self.paint_decorations(
                cx,
                self.geo.frame.page().paint_start_offset_y(),
                self.geo.frame.page().offset_breaks(),
                positions,
                PaintPass::foreground(layer),
            );
            self.paint_repeated_table_header_decorations(cx, PaintPass::foreground(layer));
        }
        render_state
    }

    pub fn paint_overlay_lines_from_positions(
        &self,
        cx: &mut impl Painter,
        first_line: usize,
        end_line: usize,
        positions: &VisibleLinePositions,
    ) {
        let lines = self.geo.doc.text().lines();
        for line_idx in first_line..=end_line {
            if let (Some(screen), Some(line)) = (positions.get(line_idx), lines.get(line_idx)) {
                self.paint_highlights_for_line(cx, &line, screen.point);
                self.paint_media_overlay_for_line(cx, &line, screen.point);
                self.paint_annotations_for_line(cx, line_idx, &line, screen.point);
                self.paint_selection_for_line(cx, line_idx, &line, screen.point);
            }
        }
    }

    fn paint_base_lines_from_positions(
        &self,
        cx: &mut impl Painter,
        first_line: usize,
        end_line: usize,
        positions: &VisibleLinePositions,
        mut render_state: RenderState,
        layer: ContentLayer,
    ) -> RenderState {
        let prepared_order = positions.paint_order_indices();
        let document_order = self.geo.doc.text().paint_order_indices();
        let paint_order = if prepared_order.is_empty() {
            document_order
        } else {
            prepared_order
        };
        for line_idx in paint_order
            .iter()
            .filter_map(|&index| usize::try_from(index).ok())
            .filter(|&index| (first_line..=end_line).contains(&index))
        {
            if let Some(screen) = positions.get(line_idx)
                && self
                    .geo
                    .doc
                    .view()
                    .text()
                    .line(line_idx)
                    .is_some_and(|line| layer.matches_line(&line))
            {
                render_state = self.draw_line_at(cx, line_idx, screen.point, render_state);
            }
        }
        render_state
    }

    pub fn paint_selection_for_line(
        &self,
        cx: &mut impl Painter,
        line_idx: usize,
        line: &html::layout::RenderLine,
        screen_point: Point,
    ) {
        // draw selection highlight for a line
        let Some((sel_start, sel_end)) = self.selection.range() else {
            return;
        };
        let semantic_cells = self
            .selection
            .semantic_cell_glyph_ranges(sel_start, sel_end);
        let mut selected_ranges = self
            .geo
            .doc
            .line_text_intersections(line_idx, sel_start, sel_end)
            .map(|(start, end)| start..end)
            .collect::<Vec<_>>();
        for promoted in self
            .selection
            .semantic_promoted_glyph_ranges(sel_start, sel_end)
        {
            selected_ranges.extend(
                self.geo
                    .doc
                    .line_text_intersections(line_idx, promoted.start, promoted.end)
                    .map(|(start, end)| start..end),
            );
        }
        selected_ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<std::ops::Range<u32>> = Vec::new();
        for range in selected_ranges {
            if let Some(previous) = merged.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        for selected in merged {
            let mut fragments = vec![selected];
            for cell in &semantic_cells {
                let mut next = Vec::new();
                for fragment in fragments {
                    if cell.end <= fragment.start || cell.start >= fragment.end {
                        next.push(fragment);
                    } else {
                        if fragment.start < cell.start {
                            next.push(fragment.start..cell.start.min(fragment.end));
                        }
                        if cell.end < fragment.end {
                            next.push(cell.end.max(fragment.start)..fragment.end);
                        }
                    }
                }
                fragments = next;
            }
            for fragment in fragments {
                let (range_x0, range_x1) =
                    self.geo
                        .glyph_x_span_in_line(line, fragment.start, fragment.end);
                let x0 = screen_point.x + range_x0;
                let x1 = screen_point.x + range_x1;
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
                self.fill_selection_rect(cx, x0, x1, screen_point.y, line.height());
            }
        }
    }

    pub fn fill_selection_rect(
        &self,
        cx: &mut impl Painter,
        x0: f64,
        x1: f64,
        y: f64,
        height: f64,
    ) {
        // paint selection highlight for a horizontal span
        if x1 <= x0 {
            return;
        }
        let highlight = self.interaction_palette.selection;
        let rect = Rect::new(x0, y, x1, y + height);
        cx.fill_rect(rect, highlight);
    }

    fn paint_decorations(
        &self,
        cx: &mut impl Painter,
        start_offset_y: f64,
        breaks: &[(f64, f64)],
        line_positions: Option<&VisibleLinePositions>,
        pass: PaintPass,
    ) {
        // draw background decorations across columns
        let page_start = start_offset_y;
        let page_end = start_offset_y + self.geo.layout.col_count * self.geo.layout.size.height; // visible doc y end for this page
        let inline_metrics = self.geo.layout.inline_metrics();
        let lines = self.geo.doc.text().lines();
        let fragments = self.geo.doc.view().fragments();
        let decorations = match self.geo.frame.page().block_decoration_indices() {
            Some(indexes) => fragments.decorations_by_indices(indexes),
            None => fragments.block_decorations(),
        };
        for decoration in decorations.iter() {
            if decoration.is_foreground() != pass.is_foreground() {
                continue;
            }
            if !pass.layer.matches_decoration(&decoration) {
                continue;
            }
            if decoration.line_idx().is_some() {
                // Line-owned backgrounds/borders are replayed immediately
                // around that line's content so overlapping inline and atomic
                // inline boxes preserve CSS paint order.
                continue;
            }
            let mut rect = decoration.rect();
            if let Some(clip) = decoration.overflow_clip() {
                let clip_rect = clip.rect();
                let x0 = if clip.clips_x() {
                    rect.x0.max(clip_rect.x0)
                } else {
                    rect.x0
                };
                let y0 = if clip.clips_y() {
                    rect.y0.max(clip_rect.y0)
                } else {
                    rect.y0
                };
                let x1 = if clip.clips_x() {
                    rect.x1.min(clip_rect.x1)
                } else {
                    rect.x1
                };
                let y1 = if clip.clips_y() {
                    rect.y1.min(clip_rect.y1)
                } else {
                    rect.y1
                };
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                rect = Rect::new(x0, y0, x1, y1);
            }
            if rect.y1 <= page_start || rect.y0 >= page_end {
                continue;
            }
            if decoration.is_inline() {
                if let Some(line_idx) = decoration.line_idx().or_else(|| {
                    self.geo
                        .doc
                        .find_line_index_for_decoration(rect.y0, rect.y1)
                }) {
                    if let Some(screen) =
                        line_positions.and_then(|positions| positions.get(line_idx))
                    {
                        let Some(line) = lines.get(line_idx) else {
                            continue;
                        };
                        self.paint_inline_decoration_at(
                            cx,
                            &rect,
                            &line,
                            screen.point,
                            0.0,
                            &decoration,
                        );
                    } else if let Some(line) = self.geo.doc.text().line(line_idx) {
                        let render_state = initial_render_state();
                        let (point, _) = self.geo.layout.resolve_point_at(
                            line.point(),
                            line.height(),
                            render_state,
                            start_offset_y,
                        );
                        if let Some(point) = point {
                            self.paint_inline_decoration_at(
                                cx,
                                &rect,
                                &line,
                                point,
                                inline_metrics.offset,
                                &decoration,
                            );
                        }
                    }
                }
                continue;
            }
            let mut seg_start = rect.y0;
            let mut break_idx = 0;
            let mut current_offset = 0.0;
            while break_idx < breaks.len() && breaks[break_idx].0 <= seg_start {
                current_offset = breaks[break_idx].1;
                break_idx += 1;
            }

            while seg_start < rect.y1 {
                let next_break = breaks.get(break_idx).map(|b| b.0).unwrap_or(rect.y1);
                let seg_end = rect.y1.min(next_break);
                self.paint_decoration_segment(
                    cx,
                    DecorationSegment {
                        rect: &rect,
                        start: seg_start,
                        end: seg_end,
                        y_offset: current_offset,
                        page_start_y: start_offset_y,
                        decoration: &decoration,
                    },
                );
                seg_start = seg_end;
                if break_idx < breaks.len() && breaks[break_idx].0 <= seg_start {
                    current_offset = breaks[break_idx].1;
                    break_idx += 1;
                }
            }
        }
    }

    fn paint_decorations_for_line(
        &self,
        cx: &mut impl Painter,
        line_idx: usize,
        screen_point: Point,
        pass: PaintPass,
    ) {
        let Some(line) = self.geo.doc.text().line(line_idx) else {
            return;
        };
        for decoration in self
            .geo
            .doc
            .view()
            .fragments()
            .decorations_for_line(line_idx)
            .iter()
        {
            if decoration.is_foreground() != pass.is_foreground()
                || !pass.layer.matches_decoration(&decoration)
            {
                continue;
            }
            self.paint_line_decoration(cx, &line, screen_point, &decoration);
        }
    }

    fn paint_decoration_segment(&self, cx: &mut impl Painter, segment: DecorationSegment<'_>) {
        let DecorationSegment {
            rect,
            start: seg_start,
            end: seg_end,
            y_offset,
            page_start_y: start_offset_y,
            decoration,
        } = segment;
        // split decoration across columns and draw
        if seg_end <= seg_start {
            return;
        }

        let col_height = self.geo.layout.size.height;
        let y0p = seg_start + y_offset - start_offset_y; // segment y in page space
        let y1p = seg_end + y_offset - start_offset_y;
        if y1p <= 0.0 {
            return;
        }
        let eps = 0.0001;
        let mut first_col = (y0p / col_height).floor() as i32;
        let mut last_col = ((y1p - eps) / col_height).floor() as i32;
        if first_col < 0 {
            first_col = 0;
        }
        let max_col = self.geo.layout.col_count as i32 - 1;
        if last_col > max_col {
            last_col = max_col;
        }
        if last_col < first_col {
            return;
        }

        for col in first_col..=last_col {
            let col_top = (col as f64) * col_height;
            let col_bottom = col_top + col_height;
            let y0 = y0p.max(col_top);
            let y1 = y1p.min(col_bottom);
            if y1 <= y0 {
                continue;
            }
            let x = self.geo.layout.col_x(col as f64, rect.x0);
            let y = y0 - col_top;
            let height = y1 - y0;
            let split_rect = kurbo::Rect::new(x, y, x + rect.width(), y + height);
            let radii = decoration.radii().map(|mut radii| {
                if seg_start > rect.y0 {
                    radii.top_left = (0.0, 0.0);
                    radii.top_right = (0.0, 0.0);
                }
                if seg_end < rect.y1 {
                    radii.bottom_left = (0.0, 0.0);
                    radii.bottom_right = (0.0, 0.0);
                }
                radii
            });
            // Column splitting changes only the physical corner exposure. The
            // semantic fragment still owns its CSS appearance.
            html::render::paint_projected_decoration(cx, decoration, split_rect, radii);
        }
    }

    pub fn paint_inline_decoration_at(
        &self,
        cx: &mut impl Painter,
        rect: &kurbo::Rect,
        line: &html::layout::RenderLine,
        screen_point: Point,
        x_offset: f64,
        decoration: &html::layout::RenderDecoration,
    ) {
        // Inline backgrounds, borders, and text decorations share the same
        // paint-only optical shift as their glyphs. The source rect remains in
        // logical layout coordinates, so pagination and overflow geometry do
        // not change.
        let split_rect = project_inline_decoration_rect(
            *rect,
            line.point(),
            screen_point,
            x_offset,
            line.optical_offset_x(),
        );
        html::render::paint_resolved_decoration(cx, decoration, split_rect);
    }

    pub fn paint_line_decoration(
        &self,
        cx: &mut impl Painter,
        line: &html::layout::RenderLine,
        screen_point: Point,
        decoration: &html::layout::RenderDecoration,
    ) {
        let mut rect = decoration.rect();
        if let Some(clip) = decoration.overflow_clip() {
            let clip_rect = clip.rect();
            let x0 = if clip.clips_x() {
                rect.x0.max(clip_rect.x0)
            } else {
                rect.x0
            };
            let y0 = if clip.clips_y() {
                rect.y0.max(clip_rect.y0)
            } else {
                rect.y0
            };
            let x1 = if clip.clips_x() {
                rect.x1.min(clip_rect.x1)
            } else {
                rect.x1
            };
            let y1 = if clip.clips_y() {
                rect.y1.min(clip_rect.y1)
            } else {
                rect.y1
            };
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            rect = Rect::new(x0, y0, x1, y1);
        }
        self.paint_inline_decoration_at(cx, &rect, line, screen_point, 0.0, decoration);
    }

    fn paint_repeated_table_header_decorations(&self, cx: &mut impl Painter, pass: PaintPass) {
        for header in self.geo.frame.page().repeated_table_headers() {
            for decoration in self.geo.doc.view().fragments().decorations().iter() {
                if decoration.is_foreground() != pass.is_foreground()
                    || !pass.layer.matches_decoration(&decoration)
                {
                    continue;
                }
                let mut rect = decoration.rect();
                if let Some(clip) = decoration.overflow_clip() {
                    let clip_rect = clip.rect();
                    rect = Rect::new(
                        if clip.clips_x() {
                            rect.x0.max(clip_rect.x0)
                        } else {
                            rect.x0
                        },
                        if clip.clips_y() {
                            rect.y0.max(clip_rect.y0)
                        } else {
                            rect.y0
                        },
                        if clip.clips_x() {
                            rect.x1.min(clip_rect.x1)
                        } else {
                            rect.x1
                        },
                        if clip.clips_y() {
                            rect.y1.min(clip_rect.y1)
                        } else {
                            rect.y1
                        },
                    );
                }
                let source_x0 = rect.x0.max(header.source_left);
                let source_x1 = rect.x1.min(header.source_right);
                let source_y0 = rect.y0.max(header.source_top);
                let source_y1 = rect.y1.min(header.source_bottom);
                if source_x1 <= source_x0 || source_y1 <= source_y0 {
                    continue;
                }
                let x = self.geo.layout.col_x(header.col_index as f64, source_x0);
                let y = source_y0 - header.source_top;
                let target = Rect::new(x, y, x + source_x1 - source_x0, y + source_y1 - source_y0);
                let radii = decoration.radii().map(|mut radii| {
                    if source_x0 > rect.x0 {
                        radii.top_left = (0.0, 0.0);
                        radii.bottom_left = (0.0, 0.0);
                    }
                    if source_x1 < rect.x1 {
                        radii.top_right = (0.0, 0.0);
                        radii.bottom_right = (0.0, 0.0);
                    }
                    if source_y0 > rect.y0 {
                        radii.top_left = (0.0, 0.0);
                        radii.top_right = (0.0, 0.0);
                    }
                    if source_y1 < rect.y1 {
                        radii.bottom_left = (0.0, 0.0);
                        radii.bottom_right = (0.0, 0.0);
                    }
                    radii
                });
                html::render::paint_projected_decoration(cx, &decoration, target, radii);
            }
        }
    }

    fn paint_repeated_table_header_lines(
        &self,
        cx: &mut impl Painter,
        layer: ContentLayer,
        mut render_state: RenderState,
    ) -> RenderState {
        let lines = self.geo.doc.text().lines();
        for header in self.geo.frame.page().repeated_table_headers() {
            for (line_idx, line) in lines.iter().enumerate() {
                if line.point().x < header.source_left - 0.01
                    || line.point().x >= header.source_right + 0.01
                    || line.point().y < header.source_top - 0.01
                    || line.point().y >= header.source_bottom - 0.01
                    || !layer.matches_line(&line)
                {
                    continue;
                }
                let point = Point::new(
                    self.geo
                        .layout
                        .col_x(header.col_index as f64, line.point().x),
                    line.point().y - header.source_top,
                );
                render_state = self.draw_line_at(cx, line_idx, point, render_state);
            }
        }
        render_state
    }

    pub fn draw_line_at(
        &self,
        cx: &mut impl Painter,
        line_idx: usize,
        point: Point,
        render_state: RenderState,
    ) -> RenderState {
        // Preserve the layout-space line origin for decorations and replaced
        // content. Text painters snap only the baseline axis where required;
        // moving the whole line would detach backgrounds from equivalent block
        // geometry by as much as half a device pixel.
        let point = line_paint_origin(point);
        let clip = self
            .geo
            .doc
            .text()
            .line_overflow_clip(line_idx)
            .and_then(|clip| {
                let line_point = self.geo.doc.text().line(line_idx)?.point();
                let delta = point - line_point;
                let rect = clip.rect() + delta;
                Some(Rect::new(
                    if clip.clips_x() { rect.x0 } else { 0.0 },
                    if clip.clips_y() { rect.y0 } else { 0.0 },
                    if clip.clips_x() {
                        rect.x1
                    } else {
                        self.geo.layout.size.width
                    },
                    if clip.clips_y() {
                        rect.y1
                    } else {
                        self.geo.layout.size.height
                    },
                ))
            });
        if let Some(clip) = clip {
            cx.push_clip(clip);
        }
        let layer = self
            .geo
            .doc
            .text()
            .line(line_idx)
            .map(|line| ContentLayer::for_line(&line))
            .unwrap_or(ContentLayer::Flow);
        let color = self.reader_palette.foreground.or_else(|| {
            self.geo
                .doc
                .text()
                .line(line_idx)
                .and_then(|line| line.paint_color())
        });
        self.paint_decorations_for_line(cx, line_idx, point, PaintPass::background(layer));
        self.draw_images_for_line(cx, line_idx, point);
        if !html::render::paint_line_text_runs(
            self.geo.doc.document(),
            line_idx,
            point,
            self.geo.layout.scale,
            color,
            cx,
        ) {
            html::render::paint_line_glyphs(
                self.geo.doc.document(),
                line_idx,
                point,
                self.geo.layout.scale,
                color,
                cx,
            );
        }
        self.paint_decorations_for_line(cx, line_idx, point, PaintPass::foreground(layer));
        if clip.is_some() {
            cx.pop_clip();
        }
        render_state
    }

    pub fn draw_images_for_line(&self, cx: &mut impl Painter, line_idx: usize, point: Point) {
        let images = self.geo.doc.view().fragments().images_for_line(line_idx);
        if images.is_empty() {
            return;
        }
        let placeholder = Color::rgba8(230, 230, 230, 255);
        for frag in images.iter() {
            self.draw_image_fragment_with_placeholder(cx, &frag, point, placeholder);
        }
    }

    pub fn draw_image_fragment_with_placeholder(
        &self,
        cx: &mut impl Painter,
        frag: &html::layout::RenderImageFragment,
        point: Point,
        placeholder: Color,
    ) {
        let (width, height) = scaled_image_size(frag);
        let offset = frag.offset();
        let rect = Rect::new(
            point.x + offset.x,
            point.y + offset.y,
            point.x + offset.x + width,
            point.y + offset.y + height,
        );
        if let Some(decoded) = self.images.get_decoded(frag.image_idx()) {
            match decoded {
                html::resources::DecodedImage::Raster { image, hash, .. } => {
                    cx.draw_image(image, hash, rect)
                }
                html::resources::DecodedImage::Svg {
                    bytes,
                    hash,
                    width,
                    height,
                } => cx.draw_svg(bytes, hash, (*width, *height), rect),
            }
        } else {
            cx.fill_rect(rect, placeholder);
        }
    }

    /// Paint highlight rects for all matches that overlap `line`.
    /// Called for every visible line during `paint_lines_from_positions`.
    pub fn paint_highlights_for_line(
        &self,
        cx: &mut impl Painter,
        line: &html::layout::RenderLine,
        screen_point: Point,
    ) {
        if self.highlight.matches.is_empty() {
            return;
        }

        // Binary-search for the first match whose end is past the line's start glyph
        let first = self
            .highlight
            .matches
            .partition_point(|&(_, end)| end <= line.start());

        for (i, &(match_start, match_end)) in self.highlight.matches[first..].iter().enumerate() {
            if match_start >= line.end() {
                break;
            }
            let is_current = first + i == self.highlight.current_local_match;
            let color = if is_current {
                self.interaction_palette.active_search_match
            } else {
                self.interaction_palette.search_match
            };

            for (start, end) in
                self.geo
                    .doc
                    .line_text_intersections(line.index(), match_start, match_end)
            {
                let (range_x0, range_x1) = self.geo.glyph_x_span_in_line(line, start, end);
                let x0 = screen_point.x + range_x0;
                let x1 = screen_point.x + range_x1;
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };

                if x1 > x0 {
                    let rect = Rect::new(x0, screen_point.y, x1, screen_point.y + line.height());
                    cx.fill_rect(rect, color);
                }
            }
        }
    }

    pub fn paint_annotations_for_line(
        &self,
        painter: &mut impl Painter,
        line_idx: usize,
        line: &html::layout::RenderLine,
        screen: Point,
    ) {
        for (_, range_start, range_end, style, rgba) in &self.annotations.visible {
            for (start, end) in
                self.geo
                    .doc
                    .line_text_intersections(line_idx, *range_start, *range_end)
            {
                let (x0, x1) = self.geo.glyph_x_span_in_line(line, start, end);
                let (x0, x1) = (screen.x + x0, screen.x + x1);
                if x1 <= x0 {
                    continue;
                }
                let color = Color::rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
                let thickness = 2.0;
                let rect = match style {
                    AnnotationStyle::Highlight => {
                        Rect::new(x0, screen.y, x1, screen.y + line.height())
                    }
                    AnnotationStyle::Underline | AnnotationStyle::Squiggly => Rect::new(
                        x0,
                        screen.y + line.height() - thickness,
                        x1,
                        screen.y + line.height(),
                    ),
                    AnnotationStyle::Strikethrough => Rect::new(
                        x0,
                        screen.y + line.height() / 2.0,
                        x1,
                        screen.y + line.height() / 2.0 + thickness,
                    ),
                };
                painter.fill_rect(rect, color);
            }
        }
    }

    pub fn paint_media_overlay_for_line(
        &self,
        painter: &mut impl Painter,
        line: &html::layout::RenderLine,
        screen: Point,
    ) {
        let Some((range_start, range_end)) = self.media_overlay.visible else {
            return;
        };
        for (start, end) in
            self.geo
                .doc
                .line_text_intersections(line.index(), range_start, range_end)
        {
            let (x0, x1) = self.geo.glyph_x_span_in_line(line, start, end);
            let (x0, x1) = (screen.x + x0, screen.x + x1);
            if x1 > x0 {
                painter.fill_rect(
                    Rect::new(x0, screen.y, x1, screen.y + line.height()),
                    Color::rgba8(82, 139, 255, 54),
                );
            }
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn line_origin_preserves_fractional_layout_position() {
        let origin = line_paint_origin(Point::new(31.609_375, 7.4));

        assert_eq!(origin.x, 31.609_375);
        assert_eq!(origin.y, 7.4);
    }

    #[test]
    fn inline_decorations_follow_the_glyph_optical_offset_without_changing_size() {
        let source = Rect::new(20.0, 30.0, 80.0, 50.0);
        let projected = project_inline_decoration_rect(
            source,
            Point::new(10.0, 20.0),
            Point::new(100.0, 200.0),
            3.0,
            -4.5,
        );

        assert_eq!(projected, Rect::new(108.5, 210.0, 168.5, 230.0));
        assert_eq!(projected.size(), source.size());
    }
}
