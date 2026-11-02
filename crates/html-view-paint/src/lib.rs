//! Painting one laid-out page.
//!
//! Everything here reads: a document, the frame currently laid out, column
//! geometry, palettes, decoded images and the overlay state. Nothing calls back
//! into the renderer, which is why painting can be driven from a bare
//! [`PagePainter`] and exercised with a recording painter in tests.
//!
//! The overlay state types live here too -- what to highlight and how to draw it
//! are the same concern.

use html::annotations::{AnnotationAnchor, AnnotationIndex, DocumentAnnotation};
use html::layout::ReadingTarget;
use html::render::{DocumentPainter, FragmentImagePolicy, Painter};
use html::search::{DocumentSearch, Search};
use html::selection::SelectionGeometry;
use html::targets::TextQuote;

mod projection;
use html::resources::ImagePipeline;
use html_view_doc::{DocQuery, TextGeometry};
use html_view_select::SelectionView;
use html_view_types::{
    AnnotationStyle, BookSearchMatch, BookSearchTarget, InteractionPalette, ReaderPaintPalette,
    RendererAnnotation, RendererEvent, RendererHost, SearchOptions, SearchScope,
};
use kurbo::Rect;
use peniko::Color;
use projection::PageProjection;
use std::ops::Range;

#[derive(Default)]
pub struct AnnotationOverlayState {
    source: Vec<RendererAnnotation>,
    index: AnnotationIndex<usize>,
}

#[derive(Default)]
pub enum MediaOverlayHighlightState {
    #[default]
    None,
    Href {
        href: String,
        visible: Option<Range<u32>>,
    },
    Reading(ReadingTarget),
}

#[derive(Default)]
pub struct HighlightState {
    local: DocumentSearch,
    book_matches: Vec<BookSearchMatch>,
    current_match: usize,
    search_active: bool,
    scope: SearchScope,
}

#[derive(Clone, Copy)]
enum SearchProjectionState {
    Active,
    Inactive,
}

impl HighlightState {
    pub fn query(&self) -> &str {
        self.local.query()
    }

    pub fn options(&self) -> SearchOptions {
        let options = self.local.options();
        SearchOptions {
            match_case: options.match_case,
            whole_word: options.whole_word,
            match_diacritics: options.match_diacritics,
            scope: self.scope,
        }
    }

    pub fn is_active(&self) -> bool {
        self.search_active
    }

    pub fn matches(&self) -> &[Range<u32>] {
        self.local.results().ranges()
    }

    pub fn current_match_index(&self) -> usize {
        self.current_match
    }

    pub fn refresh_visible_matches(&mut self, document: DocQuery<'_>) {
        self.local.refresh(document.view());
    }

    fn projected_with_state(
        query: String,
        state: SearchProjectionState,
        options: SearchOptions,
        matches: Vec<Range<u32>>,
        current_local_match: usize,
    ) -> Self {
        let mut local = DocumentSearch::default();
        local.configure(query, options.into());
        local.results_mut().replace(matches);
        local.results_mut().focus(current_local_match);
        Self {
            local,
            search_active: matches!(state, SearchProjectionState::Active),
            scope: options.scope,
            ..Self::default()
        }
    }

    pub fn projected_active(
        query: String,
        options: SearchOptions,
        matches: Vec<Range<u32>>,
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
        matches: Vec<Range<u32>>,
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
            ..self.options()
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
        self.local.configure(query, options.into());
        self.scope = options.scope;
        self.book_matches.clear();
        self.current_match = 0;
    }

    /// Records whole-document matches and mirrors them as book matches, which
    /// is how a current-document search reports results.
    pub fn set_local_matches(&mut self, doc: usize, document: DocQuery<'_>) {
        self.local.refresh(document.view());
        self.book_matches = self
            .local
            .results()
            .ranges()
            .iter()
            .enumerate()
            .map(|(occurrence, range)| {
                let excerpt = Search::new(document.view())
                    .excerpt(range.clone(), 42, 70)
                    .unwrap_or_else(|| self.query().to_owned());
                BookSearchMatch {
                    doc,
                    target: BookSearchTarget::LocalOccurrence(occurrence),
                    excerpt,
                }
            })
            .collect();
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
        if self.query() != query || self.options() != options {
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

    pub fn select_match(&mut self, index: usize) -> bool {
        if index >= self.book_matches.len() {
            return false;
        }
        self.current_match = index;
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
        self.local.results_mut().focus(occurrence);
    }

    pub fn current_local_match_start(&self) -> Option<u32> {
        self.local.results().current().map(|range| range.start)
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
        self.index = AnnotationIndex::default();
    }

    pub fn upsert(&mut self, annotation: RendererAnnotation) {
        self.source.retain(|item| item.id != annotation.id);
        self.source.push(annotation);
        self.index = AnnotationIndex::default();
    }

    pub fn remove(&mut self, id: &str) {
        self.source.retain(|item| item.id != id);
        self.index = AnnotationIndex::default();
    }

    pub fn visible_len(&self) -> usize {
        self.index.resolved().len()
    }

    /// Recomputes which annotations fall in the document now on screen,
    /// preferring the recorded CFI and falling back to text matching.
    pub fn resolve(&mut self, doc: DocQuery<'_>, current_doc: usize) {
        self.resolve_with_cfi(doc, current_doc, &html_view_cfi::PublicationCfi::default());
    }

    pub fn resolve_with_cfi(
        &mut self,
        doc: DocQuery<'_>,
        current_doc: usize,
        cfi: &html_view_cfi::PublicationCfi,
    ) {
        let entries = self
            .source
            .iter()
            .enumerate()
            .filter_map(|(id, annotation)| {
                let internal = cfi.to_internal(&annotation.cfi_range)?;
                if html_view_cfi::parse_cfi_spine_only(&internal) != Some(current_doc) {
                    return None;
                }
                let preferred = html_view_cfi::parse_cfi_range(doc.view(), &internal)
                    .filter(|(document, _, _)| *document == current_doc)
                    .map(|(_, start, end)| start..end);
                Some(DocumentAnnotation {
                    id,
                    anchor: AnnotationAnchor::Range {
                        preferred,
                        quote: TextQuote {
                            exact: annotation.exact_text.clone(),
                            prefix: annotation.prefix.clone(),
                            suffix: annotation.suffix.clone(),
                        },
                    },
                })
            });
        self.index.replace(entries);
        self.index.resolve(doc.view());
    }

    /// The glyph range an annotation occupies in the document on screen, if it
    /// is one of the annotations resolved onto this page.
    pub fn glyph_range(&self, id: &str) -> Option<Range<u32>> {
        self.index
            .resolved()
            .iter()
            .find(|entry| self.source[entry.id].id == id)
            .map(|entry| entry.range.clone())
    }

    /// Reports the topmost annotation covering `glyph`. Later annotations win,
    /// matching paint order.
    pub fn activate_at_glyph(&self, glyph: u32, host: &dyn RendererHost) -> bool {
        let Some(entry) = self.index.at(glyph).next_back() else {
            return false;
        };
        host.emit(RendererEvent::AnnotationActivated {
            id: self.source[entry.id].id.clone(),
        });
        true
    }
}

impl MediaOverlayHighlightState {
    pub fn target_href(&self) -> Option<&str> {
        match self {
            Self::Href { href, .. } => Some(href),
            _ => None,
        }
    }

    pub fn set_visible(&mut self, range: Option<Range<u32>>) {
        if let Self::Href { visible, .. } = self {
            *visible = range;
        }
    }

    pub fn set_target(&mut self, target_href: Option<String>) {
        *self = target_href.map_or(Self::None, |href| Self::Href {
            href,
            visible: None,
        });
    }

    pub fn reading_target(&self) -> Option<&ReadingTarget> {
        match self {
            Self::Reading(target) => Some(target),
            _ => None,
        }
    }

    pub fn set_reading_target(&mut self, target: Option<ReadingTarget>) {
        *self = target.map_or(Self::None, Self::Reading);
    }

    pub fn visible(&self) -> Option<Range<u32>> {
        match self {
            Self::Href { visible, .. } => visible.clone(),
            _ => None,
        }
    }

    fn ranges(&self) -> &[Range<u32>] {
        match self {
            Self::Reading(target) => &target.glyph_ranges,
            Self::Href { visible, .. } => visible.as_slice(),
            Self::None => &[],
        }
    }
}

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

#[derive(Clone, Copy)]
enum OverlayRange {
    Search(bool),
    Reading,
    Annotation(usize),
    Selection,
}

impl<'a> PagePainter<'a> {
    pub fn paint_frame(&self, cx: &mut impl Painter) {
        self.paint_base(cx);
        self.paint_interaction_overlay(cx);
    }

    pub fn paint_base(&self, cx: &mut impl Painter) {
        let mut painter = DocumentPainter::new(self.geo.doc.view()).with_images(self.images);
        painter.foreground = self.reader_palette.foreground;
        painter.background = self.reader_palette.background;
        painter.scale = self.geo.layout.scale;
        painter.image_policy = FragmentImagePolicy::Placeholder(0xe6e6e6ff);
        painter.paint(&PageProjection(self.geo), cx);
    }

    pub fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        let (Some(_), Some(_)) = (
            self.geo.frame.page().start_line(),
            self.geo.frame.page().end_line(),
        ) else {
            return;
        };
        let plan = self
            .selection
            .range()
            .map_or_else(SelectionGeometry::default, |(start, end)| {
                self.selection.geometry_plan(start, end)
            });
        let mut ranges = Vec::new();
        ranges.extend(
            self.highlight
                .matches()
                .iter()
                .enumerate()
                .map(|(index, range)| {
                    (
                        OverlayRange::Search(
                            index == self.highlight.local.results().current_index(),
                        ),
                        range.clone(),
                    )
                }),
        );
        ranges.extend(
            self.media_overlay
                .ranges()
                .iter()
                .cloned()
                .map(|range| (OverlayRange::Reading, range)),
        );
        ranges.extend(
            self.annotations
                .index
                .resolved()
                .iter()
                .map(|entry| (OverlayRange::Annotation(entry.id), entry.range.clone())),
        );
        ranges.extend(
            plan.text_ranges
                .iter()
                .cloned()
                .map(|range| (OverlayRange::Selection, range)),
        );
        let geometry = self
            .geo
            .document_geometry()
            .range_geometry_lines(ranges, self.geo.placements());
        for item in geometry {
            match item.id {
                OverlayRange::Search(active) => cx.fill_rect(
                    item.bounds,
                    if active {
                        self.interaction_palette.active_search_match
                    } else {
                        self.interaction_palette.search_match
                    },
                ),
                OverlayRange::Reading => {
                    cx.fill_rect(item.bounds, self.interaction_palette.reading)
                }
                OverlayRange::Selection => {
                    cx.fill_rect(item.bounds, self.interaction_palette.selection)
                }
                OverlayRange::Annotation(index) => {
                    let annotation = &self.annotations.source[index];
                    Self::paint_annotation(cx, item.bounds, annotation.style, annotation.color);
                }
            }
        }
        self.selection.paint_table_selection(cx, &plan);
    }

    fn paint_annotation(
        painter: &mut impl Painter,
        bounds: Rect,
        style: AnnotationStyle,
        rgba: [u8; 4],
    ) {
        let color = Color::rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
        let thickness = 2.0;
        let rect = match style {
            AnnotationStyle::Highlight => bounds,
            AnnotationStyle::Squiggly => {
                for rect in squiggly_rects(bounds.x0, bounds.x1, bounds.y1) {
                    painter.fill_rect(rect, color);
                }
                return;
            }
            AnnotationStyle::Underline => {
                Rect::new(bounds.x0, bounds.y1 - thickness, bounds.x1, bounds.y1)
            }
            AnnotationStyle::Strikethrough => Rect::new(
                bounds.x0,
                (bounds.y0 + bounds.y1) / 2.0,
                bounds.x1,
                (bounds.y0 + bounds.y1) / 2.0 + thickness,
            ),
        };
        painter.fill_rect(rect, color);
    }
}

/// Subpixel strips trace a wave using the portable painter's rectangle
/// primitive. Keep the whole stroke inside the line's lower edge.
fn squiggly_rects(x0: f64, x1: f64, bottom: f64) -> impl Iterator<Item = Rect> {
    const STEP: f64 = 0.5;
    const AMPLITUDE: f64 = 1.5;
    const THICKNESS: f64 = 1.5;
    const WAVELENGTH: f64 = 6.0;
    let count = ((x1 - x0).max(0.0) / STEP).ceil() as usize;
    (0..count).map(move |index| {
        let left = x0 + index as f64 * STEP;
        let right = (left + STEP).min(x1);
        let phase = ((left + right) / 2.0 - x0) * std::f64::consts::TAU / WAVELENGTH;
        let center = bottom - AMPLITUDE - THICKNESS / 2.0 + AMPLITUDE * phase.sin();
        Rect::new(
            left,
            center - THICKNESS / 2.0,
            right,
            center + THICKNESS / 2.0,
        )
    })
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn media_highlight_switches_between_href_and_reading_without_stale_ranges() {
        let mut state = MediaOverlayHighlightState::default();
        state.set_target(Some("chapter#paragraph".into()));
        state.set_visible(Some(1..5));
        assert_eq!(state.ranges(), &[1..5]);
        let target = ReadingTarget {
            element: 4,
            glyph_ranges: vec![7..10, 15..20],
        };
        state.set_reading_target(Some(target.clone()));
        state.set_visible(None); // Re-resolving links must preserve a reading target.
        assert_eq!(state.target_href(), None);
        assert_eq!(state.ranges(), target.glyph_ranges.as_slice());
        state.set_target(Some("next#paragraph".into()));
        assert_eq!(state.reading_target(), None);
        assert!(state.ranges().is_empty());
        state.set_visible(Some(30..40));
        assert_eq!(state.ranges(), &[30..40]);
        state.set_target(None);
        assert!(state.ranges().is_empty());
    }

    #[test]
    fn squiggly_stroke_waves_and_stays_inside_its_text_range() {
        let rects: Vec<_> = squiggly_rects(10.25, 28.4, 40.0).collect();
        assert_eq!(rects.first().unwrap().x0, 10.25);
        assert_eq!(rects.last().unwrap().x1, 28.4);
        assert!(rects.iter().all(|rect| rect.y0 >= 35.5 && rect.y1 <= 40.0));
        assert!(rects.windows(2).all(|pair| pair[0].x1 == pair[1].x0));
        assert!(rects.iter().any(|rect| rect.y0 > 38.0));
        assert!(rects.iter().any(|rect| rect.y0 < 36.0));
        assert!(squiggly_rects(10.0, 10.0, 40.0).next().is_none());
    }
}
