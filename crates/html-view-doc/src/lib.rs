//! Read-only queries over a laid-out document.
//!
//! Every method here depends on the document alone: no viewport, no frame, no
//! navigation or selection state. Holding them behind [`DocQuery`] makes that
//! independence checkable by the compiler instead of by convention, and lets
//! the renderer pass document access around as an ordinary parameter.

use kurbo::{Point, Rect};
use std::collections::HashSet;
use std::ops::Range;

use html::layout::{CaretAffinity, LaidOutDocument, RenderView};
use html_view_types::{ColumnLayout, RendererAnnotation, SourcePosition, VisibleFrame};

#[derive(Clone, Copy)]
pub struct DocQuery<'a> {
    view: RenderView<'a>,
}

impl<'a> DocQuery<'a> {
    pub fn new(
        document: &'a LaidOutDocument<'_>,
        resources: &'a html::text::GlyphResourceStore,
    ) -> Self {
        Self {
            view: document.render_view(resources),
        }
    }

    pub fn from_view(view: RenderView<'a>) -> Self {
        Self { view }
    }

    pub fn view(self) -> html::layout::RenderView<'a> {
        self.view
    }

    pub fn text(self) -> html::layout::RenderTextView<'a> {
        self.view.text()
    }

    pub fn boxes(self) -> html::layout::RenderBoxView<'a> {
        self.view.boxes()
    }

    /// The first source content on a line, including images before its text.
    /// Source order gives empty TOC markers and text the same coordinate system.
    pub fn source_position_on_line(
        self,
        document: usize,
        line: usize,
        glyph: Option<u32>,
    ) -> Option<SourcePosition> {
        let source = self
            .view()
            .addressing()
            .source_location_on_line(line, glyph)?;
        Some(SourcePosition {
            document,
            path: source.path,
            utf16_offset: source.utf16_offset,
        })
    }

    pub fn source_element_y(self, position: &SourcePosition) -> Option<f64> {
        self.view()
            .addressing()
            .source_element_y(&html::layout::SourceLocation {
                path: position.path.clone(),
                utf16_offset: position.utf16_offset,
            })
    }

    pub fn line_text_intersections(
        self,
        line_idx: usize,
        start: u32,
        end: u32,
    ) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.text()
            .line_text_fragments(line_idx)
            .into_iter()
            .flatten()
            .filter_map(move |fragment| {
                if !fragment.visible() {
                    return None;
                }
                let glyphs = fragment.glyphs();
                let fragment_start = start.max(glyphs.start);
                let fragment_end = end.min(glyphs.end);
                (start < end && fragment_start < fragment_end)
                    .then_some((fragment_start, fragment_end))
            })
    }

    pub fn best_anchor_from_glyphs(
        self,
        glyph_limit: u32,
        filter: Option<&HashSet<u16>>,
    ) -> Option<u16> {
        let mut best: Option<(u16, u32)> = None;
        for (id_idx, glyph_idx) in self.view().addressing().anchor_glyphs() {
            if filter.is_some_and(|f| !f.contains(&id_idx)) {
                continue;
            }
            let is_better = match best {
                None => true,
                Some((_, best_glyph)) => glyph_idx > best_glyph,
            };
            if glyph_idx <= glyph_limit && is_better {
                best = Some((id_idx, glyph_idx));
            }
        }
        best.map(|(id_idx, _)| id_idx)
    }

    pub fn best_anchor_from_positions(
        self,
        current_y: f64,
        filter: Option<&HashSet<u16>>,
    ) -> Option<u16> {
        let mut best: Option<(u16, f64, u32)> = None;
        for (id_idx, pos) in self.view().addressing().anchor_positions().iter() {
            if filter.is_some_and(|f| !f.contains(&id_idx)) {
                continue;
            }
            let is_better = match best {
                None => true,
                Some((_, best_y, best_order)) => {
                    pos.y() > best_y || (pos.y() == best_y && pos.order() > best_order)
                }
            };
            if pos.y() <= current_y + 0.1 && is_better {
                best = Some((id_idx, pos.y(), pos.order()));
            }
        }
        best.map(|(id_idx, _, _)| id_idx)
    }

    /// Locates an annotation whose CFI no longer resolves, by matching its
    /// quoted text against the document with the recorded prefix and suffix as
    /// disambiguators. Returns `None` for annotations belonging to another
    /// spine document.
    pub fn recover_annotation_range(
        self,
        annotation: &RendererAnnotation,
        current_doc: usize,
    ) -> Option<Range<u32>> {
        if html_view_cfi::parse_cfi_spine_only(&annotation.cfi_range)? != current_doc {
            return None;
        }
        self.text().locate_quote(
            &annotation.exact_text,
            annotation.prefix.as_deref(),
            annotation.suffix.as_deref(),
        )
    }
}

/// Where glyphs land on screen for the page currently laid out.
///
/// Screen positions depend on the document, the shaped lines held by the
/// current frame, and the column geometry -- and on nothing else. Overlay
/// painting, hit testing and selection all need this, so it lives with the
/// document queries rather than with any one of them.
#[derive(Clone, Copy)]
pub struct TextGeometry<'a> {
    pub doc: DocQuery<'a>,
    pub frame: &'a VisibleFrame,
    pub layout: &'a ColumnLayout,
}

impl<'a> TextGeometry<'a> {
    pub fn new(doc: DocQuery<'a>, frame: &'a VisibleFrame, layout: &'a ColumnLayout) -> Self {
        Self { doc, frame, layout }
    }

    /// Project the engine's visible selection geometry into a displayed line.
    pub fn line_selection_rects(
        &self,
        line_idx: usize,
        range: Range<u32>,
        screen: Point,
    ) -> impl Iterator<Item = Rect> + '_ {
        let origin = self
            .doc
            .text()
            .line(line_idx)
            .map_or(Point::ZERO, |line| line.point());
        let delta = screen - origin;
        self.doc
            .view()
            .geometry()
            .line_selection_rects(line_idx, range, self.frame.shaping().shaped_lines())
            .map(move |rect| rect + delta)
    }

    pub fn glyph_x_in_line(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
        self.doc
            .view()
            .geometry()
            .caret_x(
                line.index(),
                glyph_idx,
                CaretAffinity::Leading,
                self.frame.shaping().shaped_lines(),
            )
            .unwrap_or(0.0)
    }

    pub fn glyph_x_in_line_trailing(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
        self.doc
            .view()
            .geometry()
            .caret_x(
                line.index(),
                glyph_idx,
                CaretAffinity::Trailing,
                self.frame.shaping().shaped_lines(),
            )
            .unwrap_or(0.0)
    }

    pub fn glyph_x_span_in_line(
        &self,
        line: &html::layout::RenderLine,
        start: u32,
        end: u32,
    ) -> (f64, f64) {
        self.doc
            .view()
            .geometry()
            .caret_span(
                line.index(),
                start..end,
                self.frame.shaping().shaped_lines(),
            )
            .unwrap_or((0.0, 0.0))
    }

    /// The rectangle a glyph range occupies on the page now, in screen
    /// coordinates.
    ///
    /// The inverse of [`Self::hit_test_glyph`], and computed the same way the
    /// selection is painted: over the visible lines the range crosses, taking
    /// each line's painted horizontal span. A caller that has to keep out of
    /// the reader's way — a note panel placed beside the passage it annotates —
    /// needs the box rather than the click that made it, because a selection
    /// spanning four lines is nowhere near the point where the mouse came up.
    ///
    /// `None` when no part of the range is on this page: it was scrolled past,
    /// or it lives in another column that is not currently shown.
    pub fn glyph_range_bounds(&self, start: u32, end: u32) -> Option<Rect> {
        if start >= end {
            return None;
        }
        let mut bounds: Option<Rect> = None;
        for (line_idx, screen) in self.frame.page().line_positions().iter() {
            for rect in self.line_selection_rects(line_idx, start..end, screen.point) {
                bounds = Some(bounds.map_or(rect, |existing| existing.union(rect)));
            }
        }
        bounds.filter(|rect| rect.width() > 0.0 || rect.height() > 0.0)
    }

    /// Caret positions at the two ends, rather than the bounding box of a
    /// multiline selection. Only boundaries on the current page are returned.
    pub fn selection_endpoints(&self, start: u32, end: u32) -> (Option<Point>, Option<Point>) {
        let mut leading = None;
        let mut trailing = None;
        if start >= end {
            return (leading, trailing);
        }
        for (line_idx, screen) in self.frame.page().line_positions().iter() {
            let Some(line) = self.doc.text().line(line_idx) else {
                continue;
            };
            if self
                .doc
                .line_text_intersections(line_idx, start, start + 1)
                .next()
                .is_some()
            {
                leading = Some(Point::new(
                    screen.point.x + self.glyph_x_in_line(&line, start),
                    screen.point.y + line.height(),
                ));
            }
            if self
                .doc
                .line_text_intersections(line_idx, end - 1, end)
                .next()
                .is_some()
            {
                trailing = Some(Point::new(
                    screen.point.x + self.glyph_x_in_line_trailing(&line, end),
                    screen.point.y + line.height(),
                ));
            }
        }
        (leading, trailing)
    }

    pub fn hit_test_glyph(&self, pos: Point) -> Option<u32> {
        // find nearest glyph index for a screen position
        let lines = self.doc.text().lines();
        if self.frame.page().line_positions().is_empty()
            || lines.is_empty()
            || self.layout.col_count <= 0.0
        {
            return None;
        }

        let max_col = self.layout.col_count as i32 - 1;
        let target_col = ((pos.x / (self.layout.col_width + self.layout.col_gap)).floor() as i32)
            .clamp(0, max_col); // hit-test only within column

        let mut best_idx: Option<usize> = None;
        let mut best_vertical_dist = f64::INFINITY;
        let mut best_horizontal_dist = f64::INFINITY;
        for (idx, screen) in self.frame.page().line_positions().iter() {
            if screen.col_index != target_col {
                continue;
            }
            let Some(line) = lines.get(idx) else {
                continue;
            };
            let y0 = screen.point.y;
            let y1 = y0 + line.height();
            let vertical_dist = if pos.y < y0 {
                y0 - pos.y
            } else if pos.y > y1 {
                pos.y - y1
            } else {
                0.0
            };

            // Multiple independent line boxes can occupy the same vertical
            // band, most commonly in adjacent table cells. Resolve those
            // candidates by their painted horizontal text span instead of
            // letting the first cell in source order capture every click.
            let glyphs = line.glyphs();
            if glyphs.is_empty() {
                continue;
            }
            let (x0, x1) = self.glyph_x_span_in_line(&line, glyphs.start, glyphs.end);
            let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
            let horizontal_dist = if pos.x < x0 {
                x0 - pos.x
            } else if pos.x > x1 {
                pos.x - x1
            } else {
                0.0
            };
            if vertical_dist < best_vertical_dist
                || (vertical_dist == best_vertical_dist && horizontal_dist < best_horizontal_dist)
            {
                best_vertical_dist = vertical_dist;
                best_horizontal_dist = horizontal_dist;
                best_idx = Some(idx);
            }
        }

        let line_idx = best_idx?;
        let screen = self.frame.page().line_positions().get(line_idx)?;
        let rel_x = pos.x - screen.point.x;
        self.doc.view().geometry().hit_test_line(
            line_idx,
            rel_x,
            self.frame.shaping().shaped_lines(),
        )
    }

    pub fn hit_test_image(&self, pos: Point) -> Option<u32> {
        if self.frame.page().line_positions().is_empty() || self.layout.col_count <= 0.0 {
            return None;
        }

        let max_col = self.layout.col_count as i32 - 1;
        let target_col = ((pos.x / (self.layout.col_width + self.layout.col_gap)).floor() as i32)
            .clamp(0, max_col);

        let fragments = self.doc.view().fragments();
        for (line_idx, screen) in self.frame.page().line_positions().iter() {
            if screen.col_index != target_col || fragments.images_for_line(line_idx).is_empty() {
                continue;
            }
            for frag in fragments.images_for_line(line_idx).iter() {
                let size = frag.size();
                let offset = frag.offset();
                let (width, height) = (size.width, size.height);
                let x0 = screen.point.x + offset.x;
                let y0 = screen.point.y + offset.y;
                let x1 = x0 + width;
                let y1 = y0 + height;
                if pos.x >= x0 && pos.x <= x1 && pos.y >= y0 && pos.y <= y1 {
                    return Some(frag.image_idx());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use html::layout::{
        ContentRole, FontSlant, GlyphId, GlyphMetric, GlyphResourceGeneration, GlyphResourceStore,
        GlyphShaper, LayoutConstraints,
    };
    use html::pipeline::DocumentFactory;
    use html::text::find_text_matches;
    use html_view_types::{SearchOptions as Opts, SearchScope};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        generation: GlyphResourceGeneration,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceGeneration)>,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.generation = GlyphResourceGeneration::default();
        }

        fn resource_generation(&self) -> GlyphResourceGeneration {
            self.generation
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::layout::ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.generation.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.generation = glyph_store;
            }
        }

        fn shape_glyph(
            &mut self,
            ch: char,
            font_size: f32,
            _weight: u16,
            _slant: FontSlant,
            _color: u32,
            _family: Option<&str>,
            resources: &mut GlyphResourceStore,
        ) -> Result<GlyphId, html::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(
                ch,
                font_size * 0.5,
                font_size * 0.75,
                font_size * 0.25,
                font_size * 0.75,
            )
            .map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = resources.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout(html: &str, resources: &mut GlyphResourceStore) -> LaidOutDocument<'static> {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut shaper, resources)
            .expect("the test shaper registers every glyph")
            .layout(
                LayoutConstraints::new(600.0, 16.0).expect("constraints must be valid"),
                resources,
            )
            .expect("test layout succeeds")
    }

    fn options() -> Opts {
        Opts {
            match_case: false,
            whole_word: false,
            match_diacritics: false,
            scope: SearchScope::WholeBook,
        }
    }

    /// The glyph index of the first occurrence of `wanted`.
    fn glyph_of(doc: DocQuery<'_>, wanted: char) -> u32 {
        let text = doc.text();
        (0..text.glyph_count() as u32)
            .find(|index| {
                text.glyph_at(*index as usize)
                    .and_then(|glyph| text.glyph_metric(glyph))
                    .is_some_and(|metric| metric.ch() == wanted)
            })
            .expect("character must be present")
    }

    #[test]
    fn unavailable_line_addresses_remain_unavailable() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>Text</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(doc.source_position_on_line(0, usize::MAX, Some(0)), None);
        assert_eq!(doc.source_position_on_line(0, 0, Some(u32::MAX)), None);
    }

    // -- text extraction ----------------------------------------------------

    #[test]
    fn selection_text_spans_block_boundaries_with_newlines() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p><p>beta</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        let (plain, _) = doc
            .view()
            .export()
            .selection_text(0..doc.text().glyph_count() as u32);

        let plain = plain.expect("a non-empty selection yields text");
        assert!(plain.contains("alpha") && plain.contains("beta"));
        assert!(
            plain.contains('\n'),
            "separate blocks must not run together: {plain:?}"
        );
    }

    #[test]
    fn selection_markdown_marks_emphasis_and_headings() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<h2>Title</h2><p><em>soft</em> and <strong>hard</strong></p>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        let (_, markdown) = doc
            .view()
            .export()
            .selection_text(0..doc.text().glyph_count() as u32);

        let markdown = markdown.expect("a non-empty selection yields markdown");
        assert!(
            markdown.starts_with("## "),
            "h2 becomes a level-two heading: {markdown:?}"
        );
        assert!(
            markdown.contains("Title"),
            "the heading text survives: {markdown:?}"
        );
        assert!(
            markdown.contains("*soft*"),
            "em becomes emphasis: {markdown:?}"
        );
        assert!(
            markdown.contains("**hard**"),
            "strong becomes bold: {markdown:?}"
        );
    }

    #[test]
    fn touch_words_span_inline_runs_preserve_unicode_and_stop_at_blocks() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<p>ca<em>fe\u{301}</em> can't 中文</p><p>next</p>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        for (character, expected) in [
            ('f', "cafe\u{301}"),
            ('\u{301}', "cafe\u{301}"),
            ('\'', "can't"),
            ('n', "can't"),
        ] {
            let range = doc.text().word_range_at(glyph_of(doc, character)).unwrap();
            assert_eq!(
                doc.view()
                    .export()
                    .selection_text(range)
                    .0
                    .unwrap_or_default(),
                expected
            );
        }
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p><p>beta</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(
            doc.view()
                .export()
                .selection_text(doc.text().word_range_at(glyph_of(doc, 'a')).unwrap())
                .0
                .unwrap_or_default(),
            "alpha"
        );
        assert_eq!(
            doc.view()
                .export()
                .selection_text(doc.text().word_range_at(glyph_of(doc, 'b')).unwrap())
                .0
                .unwrap_or_default(),
            "beta"
        );
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>one two</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert!(doc.text().word_range_at(glyph_of(doc, ' ')).is_none());
    }

    #[test]
    fn an_empty_range_yields_no_text() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(doc.view().export().selection_text(3..3), (None, None));
    }

    #[test]
    fn code_spans_use_delimiters_longer_than_their_contents() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p><code>a`b</code></p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        let (_, markdown) = doc
            .view()
            .export()
            .selection_text(0..doc.text().glyph_count() as u32);
        assert_eq!(markdown.as_deref(), Some("``a`b``"));
    }

    // -- box tree -----------------------------------------------------------

    #[test]
    fn ancestor_lookup_finds_the_nearest_match_and_stops_at_the_root() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<table><tr><td><p id='cell'>x</p></td></tr></table>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        let boxes = doc.boxes();
        let paragraph = (0..boxes.len())
            .find(|&i| boxes.attribute(i, "id") == Some("cell"))
            .expect("the paragraph is in the box tree");

        let table = doc
            .view()
            .semantics()
            .nearest(paragraph, ContentRole::Table)
            .expect("the paragraph sits in a table");
        assert!(doc.view().semantics().contains(table, paragraph));
        assert!(
            doc.view()
                .semantics()
                .nearest(paragraph, ContentRole::TableCell)
                .is_some()
        );
        assert!(
            doc.view()
                .semantics()
                .nearest(paragraph, ContentRole::Quote)
                .is_none(),
            "a tag that is not an ancestor must not match"
        );
    }

    #[test]
    fn a_box_descends_from_itself() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert!(doc.view().semantics().contains(0, 0));
    }

    #[test]
    fn heading_prefixes_cover_every_level() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<h1>a</h1><h3>b</h3><p>c</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        let boxes = doc.boxes();
        let tagged = |want: &str| {
            (0..boxes.len())
                .find(|&i| boxes.tag(i).is_some_and(|t| t.eq_ignore_ascii_case(want)))
                .expect("tag present")
        };

        assert_eq!(
            doc.view()
                .export()
                .markdown_for_box(tagged("h1"))
                .as_deref(),
            Some("# a")
        );
        assert_eq!(
            doc.view()
                .export()
                .markdown_for_box(tagged("h3"))
                .as_deref(),
            Some("### b")
        );
        assert_eq!(
            doc.view().export().markdown_for_box(tagged("p")).as_deref(),
            Some("c")
        );
    }

    // -- line lookup --------------------------------------------------------

    #[test]
    fn start_and_end_line_lookup_agree_on_a_single_line_document() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(doc.view().geometry().start_line_at(0.0, 0.1), 0);
        assert_eq!(doc.text().line_index_for_glyph(0), Some(0));
    }

    #[test]
    fn line_lookup_on_an_empty_document_does_not_panic() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(doc.view().geometry().start_line_at(0.0, 0.1), 0);
        assert_eq!(doc.view().geometry().end_line_at(0.0, 0.1), None);
        assert_eq!(doc.text().line_index_for_glyph(0), None);
    }

    #[test]
    fn start_line_advances_past_earlier_lines() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<p>alpha</p><p>beta</p><p>gamma</p>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        let last = doc.text().lines().len() - 1;
        let last_y = doc.text().line(last).expect("last line").point().y;
        assert_eq!(
            doc.view().geometry().start_line_at(last_y, 0.1),
            last,
            "an offset at the last line's top selects it"
        );
        assert_eq!(doc.view().geometry().start_line_at(0.0, 0.1), 0);
    }

    // -- search -------------------------------------------------------------

    #[test]
    fn matching_is_case_and_diacritic_insensitive_by_default() {
        assert_eq!(
            find_text_matches("Café CAFE cafeteria", "cafe", options().into()).len(),
            3
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE cafeteria",
                "cafe",
                (Opts {
                    whole_word: true,
                    ..options()
                })
                .into()
            )
            .len(),
            2
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE",
                "cafe",
                (Opts {
                    match_case: true,
                    ..options()
                })
                .into()
            )
            .len(),
            0
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE",
                "cafe",
                (Opts {
                    match_diacritics: true,
                    ..options()
                })
                .into()
            )
            .len(),
            1
        );
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(find_text_matches("alpha", "", options().into()).is_empty());
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha</p>", &mut document_resources);
        assert!(
            DocQuery::new(&document, &document_resources)
                .text()
                .find_matches("", options().into())
                .is_empty()
        );
    }

    #[test]
    fn document_matching_returns_glyph_ranges_covering_the_query() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>alpha beta alpha</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        let matches = doc.text().find_matches("alpha", options().into());

        assert_eq!(matches.len(), 2, "both occurrences are found");
        for range in matches {
            let (start, end) = (range.start, range.end);
            assert_eq!(end - start, 5, "each match spans the query length");
            let range = start..end;
            assert_eq!(
                doc.view()
                    .export()
                    .selection_text(range)
                    .0
                    .unwrap_or_default()
                    .to_lowercase(),
                "alpha"
            );
        }
    }

    #[test]
    fn list_markers_are_excluded_from_search() {
        // Browsers exclude generated ::marker content from find-in-page.
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<ol><li>item</li></ol>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert!(
            doc.text().find_matches("1", options().into()).is_empty(),
            "the generated list marker must not be searchable"
        );
    }

    // -- anchors ------------------------------------------------------------

    #[test]
    fn anchor_lookup_finds_the_nearest_preceding_id() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<p id='first'>alpha</p><p id='second'>beta</p>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        let beta = glyph_of(doc, 'b');

        let anchor = doc
            .best_anchor_from_glyphs(beta, None)
            .expect("an anchor precedes the second paragraph");
        assert_eq!(doc.view().string(anchor), "second");
    }

    #[test]
    fn anchor_lookup_respects_a_filter() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<p id='first'>alpha</p><p id='second'>beta</p>",
            &mut document_resources,
        );
        let doc = DocQuery::new(&document, &document_resources);
        let beta = glyph_of(doc, 'b');
        let only_first: HashSet<u16> = [doc.view().lookup_string("first").expect("id is interned")]
            .into_iter()
            .collect();

        let anchor = doc
            .best_anchor_from_glyphs(beta, Some(&only_first))
            .expect("the filtered anchor still resolves");
        assert_eq!(
            doc.view().string(anchor),
            "first",
            "a filter restricts the answer to listed anchors"
        );
    }

    #[test]
    fn glyph_range_for_a_missing_anchor_is_none() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p id='here'>alpha</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert!(
            doc.view()
                .addressing()
                .glyph_range_for_anchor("absent")
                .is_none()
        );
    }
}
