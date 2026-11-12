//! Painting one laid-out page.
//!
//! Everything here reads: a document, the frame currently laid out, column
//! geometry, palettes, decoded images and the overlay state. Nothing calls back
//! into the renderer, which is why painting can be driven from a bare
//! [`PagePainter`] and exercised with a recording painter in tests.
//!
//! Overlay state chooses which source ranges and appearances to supply. The
//! engine projects those ranges and draws the requested styles.

use html::reading::ReadingTarget;
use html::render::{DocumentPainter, OverlayPaintScratch, Painter, SceneImagePolicy, TextOverlay};
use html::search::DocumentSearch;
use html::selection::{SelectionPaintScratch, SelectionPaintStyle};
use html::targets::TextQuote;

use html::resources::DecodedImages;
use html::layout::RenderView;
use html::pagination::PageView;
use html::selection::SelectionController;
use html_view_types::{
    AnnotationStyle, BookSearchMatch, BookSearchResults, BookSearchTarget, InteractionPalette,
    ReaderPaintPalette, RendererAnnotation, RendererEvent, RendererEffects, SearchOptions,
    SearchScope,
};
use peniko::Color;
use std::ops::Range;
use std::sync::Arc;

#[derive(Default)]
pub struct AnnotationOverlayState {
    source: Vec<RendererAnnotation>,
    resolved: Vec<ResolvedAnnotation>,
    painted: Vec<ResolvedAnnotation>,
    active_id: Option<String>,
}

struct ResolvedAnnotation {
    source_index: usize,
    range: Range<u32>,
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
    book_matches: BookSearchResults,
    current_match: usize,
    search_active: bool,
    scope: SearchScope,
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
        self.local.ranges()
    }

    pub fn refresh_visible_matches(&mut self, doc: usize, document: RenderView<'_>) {
        if self.scope == SearchScope::CurrentDocument {
            self.set_local_matches(doc, document);
        } else {
            self.local.refresh(document);
        }
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
    pub fn set_local_matches(&mut self, doc: usize, document: RenderView<'_>) {
        self.local.refresh(document);
        self.book_matches = self
            .local
            .ranges()
            .iter()
            .enumerate()
            .map(|(occurrence, range)| {
                let excerpt = document
                    .export()
                    .excerpt(range.clone(), 42, 70)
                    .unwrap_or_else(|| self.query().to_owned());
                BookSearchMatch {
                    doc,
                    target: BookSearchTarget::LocalOccurrence(occurrence),
                    excerpt,
                }
            })
            .collect::<Vec<_>>()
            .into();
        self.current_match = self
            .current_match
            .min(self.book_matches.len().saturating_sub(1));
    }

    pub fn set_results(&mut self, results: Vec<BookSearchMatch>) {
        self.book_matches = results.into();
    }

    /// Append a streamed batch, recomputing local ranges only for a new query.
    pub fn append_results(
        &mut self,
        query: String,
        options: SearchOptions,
        results: Vec<BookSearchMatch>,
        document: RenderView<'_>,
        effects: &mut RendererEffects,
    ) {
        if self.query() != query || self.options() != options {
            self.restart(query, options);
            self.local.refresh(document);
            effects.emit(RendererEvent::SearchResults(self.book_matches.clone()));
        }
        let batch: Arc<[BookSearchMatch]> = results.into();
        self.book_matches.append(batch.clone());
        if !batch.is_empty() {
            effects.emit(RendererEvent::SearchResultsAppended(batch));
        }
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

    pub fn current_book_match(&self) -> Option<&BookSearchMatch> {
        self.book_matches.get(self.current_match)
    }

    /// Points the in-document cursor at `occurrence`, clamped to the matches
    /// actually found in the document now on screen.
    pub fn focus_local_match(&mut self, occurrence: usize) {
        self.local.focus(occurrence);
    }

    pub fn current_local_match_start(&self) -> Option<u32> {
        self.local.current().map(|(_, range)| range.start)
    }

    pub fn emit_match_signal(&self, effects: &mut RendererEffects) {
        let total = self.book_matches.len();
        let current = if total == 0 {
            0
        } else {
            self.current_match + 1
        };
        effects.emit(RendererEvent::MatchInfo { current, total });
    }

    pub fn emit_results(&self, effects: &mut RendererEffects) {
        effects.emit(RendererEvent::SearchResults(self.book_matches.clone()));
        self.emit_match_signal(effects);
    }
}

impl AnnotationOverlayState {
    pub fn source(&self) -> &[RendererAnnotation] {
        &self.source
    }

    pub fn replace(&mut self, annotations: Vec<RendererAnnotation>) {
        self.source = annotations;
        self.resolved.clear();
        self.painted.clear();
    }

    pub fn upsert(&mut self, annotation: RendererAnnotation) {
        self.source.retain(|item| item.id != annotation.id);
        self.source.push(annotation);
        self.resolved.clear();
        self.painted.clear();
    }

    pub fn remove(&mut self, id: &str) {
        self.source.retain(|item| item.id != id);
        self.resolved.clear();
        self.painted.clear();
    }

    pub fn resolve_with_cfi(
        &mut self,
        doc: RenderView<'_>,
        current_doc: usize,
        cfi: &html_view_cfi::PublicationCfi,
    ) {
        let targets = doc.addressing();
        let entries = self
            .source
            .iter()
            .enumerate()
            .filter_map(|(source_index, annotation)| {
                let internal = cfi.to_internal(&annotation.cfi_range)?;
                if html_view_cfi::parse_cfi_spine_only(&internal) != Some(current_doc) {
                    return None;
                }
                let preferred = html_view_cfi::parse_cfi_range(doc, &internal)
                    .filter(|(document, _, _)| *document == current_doc)
                    .map(|(_, start, end)| start..end);
                let quote = TextQuote {
                    exact: &annotation.exact_text,
                    prefix: annotation.prefix.as_deref(),
                    suffix: annotation.suffix.as_deref(),
                };
                let range = targets.resolve_range(preferred, quote)?;
                Some(ResolvedAnnotation {
                    source_index,
                    range,
                })
            });
        self.resolved.clear();
        self.resolved.extend(entries);
        self.rebuild_paint();
    }

    pub fn set_active(&mut self, id: Option<String>) {
        self.active_id = id;
        self.rebuild_paint();
    }

    /// Keep all hit targets, but paint each overlapping group with one stable
    /// appearance. The open note takes precedence only across its own range.
    fn rebuild_paint(&mut self) {
        self.painted.clear();
        let mut entries = self.resolved.iter().collect::<Vec<_>>();
        entries.sort_by_key(|entry| (entry.range.start, entry.range.end));
        let mut start = 0;
        while start < entries.len() {
            let mut end = start + 1;
            let mut range = entries[start].range.clone();
            while end < entries.len() && entries[end].range.start < range.end {
                range.end = range.end.max(entries[end].range.end);
                end += 1;
            }
            let group = &entries[start..end];
            let source_index = group.iter().map(|entry| entry.source_index).min().unwrap();
            let active = group.iter().find(|entry| {
                Some(self.source[entry.source_index].id.as_str()) == self.active_id.as_deref()
            });
            if let Some(active) = active {
                if range.start < active.range.start {
                    self.painted.push(ResolvedAnnotation {
                        source_index,
                        range: range.start..active.range.start,
                    });
                }
                self.painted.push(ResolvedAnnotation {
                    source_index: active.source_index,
                    range: active.range.clone(),
                });
                if active.range.end < range.end {
                    self.painted.push(ResolvedAnnotation {
                        source_index,
                        range: active.range.end..range.end,
                    });
                }
            } else {
                self.painted.push(ResolvedAnnotation {
                    source_index,
                    range,
                });
            }
            start = end;
        }
    }

    pub fn overlapping_ids(&self, id: &str) -> Vec<String> {
        let Some(range) = self.glyph_range(id) else {
            return Vec::new();
        };
        self.resolved
            .iter()
            .filter(|entry| entry.range.start < range.end && range.start < entry.range.end)
            .map(|entry| self.source[entry.source_index].id.clone())
            .collect()
    }

    /// The glyph range an annotation occupies in the document on screen, if it
    /// is one of the annotations resolved onto this page.
    pub fn glyph_range(&self, id: &str) -> Option<Range<u32>> {
        self.resolved
            .iter()
            .find(|entry| self.source[entry.source_index].id == id)
            .map(|entry| entry.range.clone())
    }

    /// Whether a resolved annotation covers the clicked glyph.
    pub fn contains_glyph(&self, glyph: u32) -> bool {
        self.resolved.iter().any(|entry| entry.range.contains(&glyph))
    }

    /// Activate the topmost annotation, matching paint order.
    pub fn activate_at_glyph(&self, glyph: u32, effects: &mut RendererEffects) -> bool {
        let Some(entry) = self
            .resolved
            .iter()
            .rev()
            .find(|entry| entry.range.contains(&glyph))
        else {
            return false;
        };
        effects.emit(RendererEvent::AnnotationActivated {
            id: self.source[entry.source_index].id.clone(),
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
    pub geo: PageView<'a>,
    pub scale: f64,
    pub images: DecodedImages<'a>,
    pub reader_palette: ReaderPaintPalette,
    pub interaction_palette: InteractionPalette,
    pub highlight: &'a HighlightState,
    pub annotations: &'a AnnotationOverlayState,
    pub media_overlay: &'a MediaOverlayHighlightState,
    pub selection: &'a SelectionController,
}

/// Reusable output and projection buffers for interaction overlays.
#[derive(Default)]
pub struct OverlayScratch {
    ranges: OverlayPaintScratch,
    selection: SelectionPaintScratch,
}

impl<'a> PagePainter<'a> {
    pub fn paint_frame(&self, cx: &mut impl Painter, scratch: &mut OverlayScratch) {
        self.paint_base(cx);
        self.paint_interaction_overlay(cx, scratch);
    }

    pub fn paint_base(&self, cx: &mut impl Painter) {
        let mut painter = DocumentPainter::new(self.geo.doc).with_images(self.images);
        painter.foreground = self.reader_palette.foreground;
        painter.background = self.reader_palette.background;
        painter.scale = self.scale;
        painter.image_policy = SceneImagePolicy::Placeholder(0xe6e6e6ff);
        painter.paint(&self.geo, cx);
    }

    pub fn paint_interaction_overlay(&self, cx: &mut impl Painter, scratch: &mut OverlayScratch) {
        let OverlayScratch {
            ranges,
            selection: selection_scratch,
        } = scratch;
        let current_match = self.highlight.local.current().map(|(index, _)| index);
        let search = self
            .highlight
            .matches()
            .iter()
            .enumerate()
            .map(|(index, range)| TextOverlay {
                range: range.clone(),
                style: AnnotationStyle::Highlight,
                color: if Some(index) == current_match {
                    self.interaction_palette.active_search_match
                } else {
                    self.interaction_palette.search_match
                },
            });
        let reading = self
            .media_overlay
            .ranges()
            .iter()
            .cloned()
            .map(|range| TextOverlay {
                range,
                style: AnnotationStyle::Highlight,
                color: self.interaction_palette.reading,
            });
        let annotations = self.annotations.painted.iter().map(|entry| {
            let annotation = &self.annotations.source[entry.source_index];
            let rgba = annotation.color;
            TextOverlay {
                range: entry.range.clone(),
                style: annotation.style,
                color: Color::rgba8(rgba[0], rgba[1], rgba[2], rgba[3]),
            }
        });
        let active = self
            .annotations
            .active_id
            .as_deref()
            .and_then(|id| self.annotations.glyph_range(id))
            .map(|range| TextOverlay {
                range,
                style: AnnotationStyle::Underline,
                color: self.interaction_palette.active_annotation,
            });
        self.geo.paint_overlays(
            search.chain(reading).chain(annotations).chain(active),
            cx,
            ranges,
        );
        self.geo.paint_selection(
            self.selection,
            SelectionPaintStyle {
                selection: self.interaction_palette.selection,
                hover_foreground: self.reader_palette.foreground,
            },
            cx,
            selection_scratch,
        );
    }
}
