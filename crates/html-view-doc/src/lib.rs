//! Read-only queries over a laid-out document.
//!
//! Every method here depends on the document alone: no viewport, no frame, no
//! navigation or selection state. Holding them behind [`DocQuery`] makes that
//! independence checkable by the compiler instead of by convention, and lets
//! the renderer pass document access around as an ordinary parameter.

#[cfg(test)]
use html::layout::LineId;
use kurbo::{Point, Rect, Vec2};
use std::ops::Range;

use html::geometry::{ContentHit, Geometry, LinePlacement, WordHit, WordHitPolicy};
use html::layout::{CaretAffinity, LaidOutDocument, RenderLine, RenderView};
use html::render::PaintFragment;
use html::targets::LinkTarget;
use html_view_types::{ColumnLayout, VisibleFrame};

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
}

/// Where glyphs land on screen for the page currently laid out.
///
/// Screen positions depend on the document, current line placements,
/// and the column geometry -- and on nothing else. Overlay
/// painting, hit testing and selection all need this, so it lives with the
/// document queries rather than with any one of them.
#[derive(Clone, Copy)]
pub struct TextGeometry<'a> {
    pub doc: DocQuery<'a>,
    pub frame: &'a VisibleFrame,
    pub layout: &'a ColumnLayout,
}

struct BoxProjectionSpan {
    column: i32,
    translation: Vec2,
    top: f64,
    bottom: f64,
}

impl<'a> TextGeometry<'a> {
    pub fn new(doc: DocQuery<'a>, frame: &'a VisibleFrame, layout: &'a ColumnLayout) -> Self {
        Self { doc, frame, layout }
    }

    pub fn document_geometry(&self) -> Geometry<'a> {
        Geometry::new(self.doc.view())
    }

    /// Projects document geometry into page columns and, when requested,
    /// replicated table headers. Painting and interaction share this mapping.
    pub fn project_rect(
        &self,
        source: Rect,
        line: Option<usize>,
        include_repeated_headers: bool,
        visit: &mut dyn FnMut(PaintFragment),
    ) {
        if let Some(index) = line {
            if let (Some(screen), Some(line)) = (
                self.frame.page().line_positions().get(index),
                self.doc.text().line(index),
            ) {
                visit(PaintFragment {
                    source,
                    destination: source.origin() + (screen.point - line.point()),
                });
            }
        } else {
            let height = self.layout.size.height;
            if height <= 0.0 {
                return;
            }
            let breaks = self.frame.page().offset_breaks();
            let start = self.frame.page().paint_start_offset_y();
            let mut top = source.y0;
            let mut index = breaks.partition_point(|(at, _)| *at <= top);
            let mut offset = index.checked_sub(1).map_or(0.0, |index| breaks[index].1);
            while top < source.y1 {
                let bottom = breaks
                    .get(index)
                    .map_or(source.y1, |(at, _)| source.y1.min(*at));
                let from = top + offset - start;
                let to = bottom + offset - start;
                let first = (from / height).floor().max(0.0) as i32;
                let last = ((to - 0.0001) / height)
                    .floor()
                    .min(self.layout.col_count - 1.0) as i32;
                for column in first..=last {
                    let column_top = f64::from(column) * height;
                    let y0 = from.max(column_top);
                    let y1 = to.min(column_top + height);
                    if y1 <= y0 {
                        continue;
                    }
                    let clipped = Rect::new(
                        source.x0,
                        y0 - offset + start,
                        source.x1,
                        y1 - offset + start,
                    );
                    visit(PaintFragment {
                        source: clipped,
                        destination: Point::new(
                            self.layout.col_x(f64::from(column), source.x0),
                            y0 - column_top,
                        ),
                    });
                }
                top = bottom;
                if let Some((at, next)) = breaks.get(index) {
                    if *at <= top {
                        offset = *next;
                        index += 1;
                    }
                }
            }
        }
        if include_repeated_headers {
            self.project_repeated_headers(source, visit);
        }
    }

    /// Projects a box through its content and enclosing flow fragments.
    /// Independent positioned boxes retain separate page membership.
    pub fn project_box_rect(
        &self,
        source: Rect,
        owner: usize,
        include_repeated_headers: bool,
        visit: &mut dyn FnMut(PaintFragment),
    ) {
        let height = self.layout.size.height;
        if height <= 0.0 {
            return;
        }
        if self.frame.page().line_positions().is_empty() {
            self.project_rect(source, None, include_repeated_headers, visit);
            return;
        }
        let boxes = self.doc.boxes();
        let mut distances = vec![usize::MAX; self.layout.col_count.ceil().max(0.0) as usize];
        let mut candidates = Vec::new();
        for (index, screen) in self.frame.page().line_positions().iter() {
            let Some(nearest) = distances.get_mut(screen.col_index as usize) else {
                continue;
            };
            let Some(distance) = boxes.fragmentation_distance(owner, index) else {
                continue;
            };
            let Some(line) = self.doc.text().line(index) else {
                continue;
            };
            let top = line.point().y - screen.point.y;
            if source.y1 <= top || source.y0 >= top + height {
                continue;
            }
            *nearest = (*nearest).min(distance);
            candidates.push((line.point(), screen, distance));
        }
        let mut spans: Vec<BoxProjectionSpan> = Vec::new();
        for (point, screen, distance) in candidates {
            // Prefer the box's own content in each column, then the nearest
            // enclosing fragment. This also carries tall cells through
            // columns in which only another cell has text.
            if distance != distances[screen.col_index as usize] {
                continue;
            }
            let translation = screen.point - point;
            let mut top = -translation.y;
            let bottom = height - translation.y;
            if let Some(previous) = spans.last_mut()
                && previous.column == screen.col_index
            {
                if (previous.translation - translation).hypot() < 1e-6 {
                    continue;
                }
                // Keep two offsets within one column from painting the same
                // source band twice across a keep or rhythm adjustment.
                previous.bottom = previous.bottom.min(point.y);
                top = top.max(point.y);
            }
            spans.push(BoxProjectionSpan {
                column: screen.col_index,
                translation,
                top,
                bottom,
            });
        }
        for span in spans {
            let clipped = source.intersect(Rect::new(
                source.x0,
                span.top,
                source.x1,
                span.bottom.max(span.top),
            ));
            if clipped.width() > 0.0 && clipped.height() > 0.0 {
                visit(PaintFragment {
                    source: clipped,
                    destination: clipped.origin() + span.translation,
                });
            }
        }
        if include_repeated_headers {
            self.project_repeated_headers(source, visit);
        }
    }

    fn project_repeated_headers(&self, source: Rect, visit: &mut dyn FnMut(PaintFragment)) {
        for header in self.frame.page().repeated_table_headers() {
            let clipped = source.intersect(Rect::new(
                header.source_left,
                header.source_top,
                header.source_right,
                header.source_bottom,
            ));
            if clipped.width() > 0.0 && clipped.height() > 0.0 {
                visit(PaintFragment {
                    source: clipped,
                    destination: Point::new(
                        self.layout.col_x(header.col_index as f64, clipped.x0),
                        clipped.y0 - header.source_top,
                    ),
                });
            }
        }
    }

    pub fn placements(&self) -> impl Iterator<Item = LinePlacement> + '_ {
        self.frame
            .page()
            .line_positions()
            .iter()
            .map(|(line, screen)| LinePlacement {
                line,
                origin: screen.point,
            })
    }

    // Column selection is presentation policy. The engine handles document
    // hit testing once the adapter has selected the candidate placements.
    fn column_placements(&self, point: Point) -> impl Iterator<Item = LinePlacement> + '_ {
        let count = self.layout.col_count as i32;
        let column = ((point.x / (self.layout.col_width + self.layout.col_gap)).floor() as i32)
            .clamp(0, (count - 1).max(0));
        self.frame
            .page()
            .line_positions()
            .iter()
            .filter(move |(_, screen)| count > 0 && screen.col_index == column)
            .map(|(line, screen)| LinePlacement {
                line,
                origin: screen.point,
            })
    }

    pub fn line_selection_rects(
        &self,
        line_idx: usize,
        range: Range<u32>,
        screen: Point,
    ) -> impl Iterator<Item = Rect> + '_ {
        self.document_geometry().placed_selection_rects(
            LinePlacement {
                line: line_idx,
                origin: screen,
            },
            range,
        )
    }

    pub fn glyph_x_in_line(&self, line: &RenderLine, glyph: u32) -> f64 {
        self.document_geometry()
            .caret_x(line.index(), glyph, CaretAffinity::Leading)
            .unwrap_or(0.0)
    }

    pub fn glyph_x_in_line_trailing(&self, line: &RenderLine, glyph: u32) -> f64 {
        self.document_geometry()
            .caret_x(line.index(), glyph, CaretAffinity::Trailing)
            .unwrap_or(0.0)
    }

    pub fn glyph_x_span_in_line(&self, line: &RenderLine, start: u32, end: u32) -> (f64, f64) {
        self.document_geometry()
            .caret_span(line.index(), start..end)
            .unwrap_or((0.0, 0.0))
    }

    pub fn glyph_range_bounds(&self, start: u32, end: u32) -> Option<Rect> {
        self.document_geometry()
            .selection_bounds_lines(start..end, self.placements())
    }

    pub fn selection_endpoints(&self, start: u32, end: u32) -> (Option<Point>, Option<Point>) {
        self.document_geometry()
            .selection_endpoints_lines(start..end, self.placements())
    }

    pub fn hit_test_glyph(&self, point: Point) -> Option<u32> {
        self.document_geometry()
            .hit_test_lines(point, self.column_placements(point))
            .map(|hit| hit.glyph)
    }

    pub fn word_at(&self, point: Point, policy: WordHitPolicy) -> Option<Range<u32>> {
        self.document_geometry()
            .word_at_lines(point, self.column_placements(point), policy)
    }

    pub fn word_hit_at(&self, point: Point, policy: WordHitPolicy) -> Option<WordHit> {
        self.document_geometry()
            .word_hit_at_lines(point, self.column_placements(point), policy)
    }

    pub fn hit_test_image(&self, point: Point) -> Option<u32> {
        self.document_geometry()
            .image_at_lines(point, self.column_placements(point))
    }

    pub fn for_each_projected_box_rect(&self, box_idx: usize, mut visit: impl FnMut(Rect)) {
        let boxes = self.doc.boxes();
        let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
            return;
        };
        let rect = Rect::new(
            point.x,
            point.y,
            point.x + size.width,
            point.y + size.height,
        );
        if rect.width() <= 0.0 || rect.height() <= 0.0 || self.layout.size.height <= 0.0 {
            return;
        }
        self.project_box_rect(rect, box_idx, true, &mut |fragment| {
            visit(Rect::from_origin_size(
                fragment.destination,
                fragment.source.size(),
            ));
        });
    }

    pub fn link_at(&self, point: Point) -> Option<LinkTarget<'a>> {
        self.document_geometry()
            .link_at_lines(point, self.column_placements(point))
    }

    pub fn content_at(&self, point: Point) -> Option<ContentHit<'a>> {
        self.document_geometry()
            .content_at_lines(point, self.column_placements(point), |index| {
                let mut contains = false;
                self.for_each_projected_box_rect(index, |rect| contains |= rect.contains(point));
                contains
            })
    }
}

#[cfg(test)]
mod tests {
    use html::text::{GlyphRequest, ShapedGlyph};

    use super::*;
    use html::layout::{
        ContentRole, FontSlant, GlyphId, GlyphMetric, GlyphResourceGeneration, GlyphResourceStore,
        GlyphShaper, LayoutConstraints,
    };
    use html::pipeline::DocumentFactory;
    use html::targets::Targets;
    use html::text::find_text_matches;
    use html_view_types::{SearchOptions as Opts, SearchScope};
    use std::collections::{HashMap, HashSet};

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
            request: GlyphRequest<'_>,
        ) -> Result<ShapedGlyph, html::layout::ShapeError> {
            let key = (request.character, request.font_size.to_bits());
            let metric = GlyphMetric::try_new(
                request.character,
                request.font_size * 0.5,
                request.font_size * 0.75,
                request.font_size * 0.25,
            )?;
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(ShapedGlyph::from_metric(glyph, metric));
            }
            let glyph = self.glyphs.len() as GlyphId;
            self.glyphs.insert(key, glyph);
            Ok(ShapedGlyph::from_metric(glyph, metric))
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
    fn rectangles_cross_offset_breaks_and_optionally_repeat_in_headers() {
        use html_view_types::{PagePositions, RepeatedTableHeader, VisibleLinePositions};
        let mut resources = GlyphResourceStore::default();
        let document = layout("<p>text</p>", &mut resources);
        let mut frame = VisibleFrame::default();
        frame.install_flow_page(
            PagePositions {
                start_line: 0,
                last_line: None,
                positions: VisibleLinePositions::default(),
                offset_breaks: vec![(90.0, 10.0)],
                repeated_table_headers: vec![RepeatedTableHeader {
                    source_left: 0.0,
                    source_right: 50.0,
                    source_top: 80.0,
                    source_bottom: 90.0,
                    col_index: 1,
                }],
                block_decoration_indices: Vec::new(),
                reached_end: false,
                next_start_offset_y: 0.0,
            },
            0.0,
            None,
        );
        let columns = ColumnLayout {
            col_width: 100.0,
            col_gap: 10.0,
            col_count: 2.0,
            size: kurbo::Size::new(210.0, 100.0),
            ..ColumnLayout::default()
        };
        let geometry = TextGeometry::new(DocQuery::new(&document, &resources), &frame, &columns);
        let source = Rect::new(5.0, 80.0, 25.0, 120.0);
        let mut fragments = Vec::new();
        geometry.project_rect(source, None, false, &mut |fragment| {
            fragments.push((fragment.source, fragment.destination));
        });
        assert_eq!(
            fragments,
            vec![
                (Rect::new(5.0, 80.0, 25.0, 90.0), Point::new(5.0, 80.0)),
                (Rect::new(5.0, 90.0, 25.0, 120.0), Point::new(115.0, 0.0)),
            ]
        );
        let mut repeated = Vec::new();
        geometry.project_rect(source, None, true, &mut |fragment| {
            repeated.push((fragment.source, fragment.destination));
        });
        assert_eq!(&repeated[..2], fragments.as_slice());
        assert_eq!(
            repeated[2],
            (Rect::new(5.0, 80.0, 25.0, 90.0), Point::new(115.0, 0.0))
        );
    }

    #[test]
    fn line_rectangles_follow_recorded_inline_placement() {
        use html_view_types::{LineScreen, VisibleLinePositions};
        let mut resources = GlyphResourceStore::default();
        let document = layout("<p>text</p>", &mut resources);
        let doc = DocQuery::new(&document, &resources);
        let source = doc.text().line(0).unwrap().point();
        let destination = Point::new(125.0, 30.0);
        let mut positions = VisibleLinePositions::with_buffer(0, Vec::new());
        positions.push(LineId::new(0), LineScreen {
            point: destination,
            col_index: 1,
        });
        let mut frame = VisibleFrame::default();
        frame.install_whole_document(1, positions);
        let columns = ColumnLayout::default();
        let geometry = TextGeometry::new(doc, &frame, &columns);
        let rect = Rect::from_origin_size(source + (2.0, 3.0), kurbo::Size::new(10.0, 20.0));
        let mut fragments = Vec::new();
        geometry.project_rect(rect, Some(0), false, &mut |fragment| {
            fragments.push((fragment.source, fragment.destination));
        });
        assert_eq!(fragments, vec![(rect, destination + (2.0, 3.0))]);
    }

    #[test]
    fn unavailable_line_addresses_remain_unavailable() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("<p>Text</p>", &mut document_resources);
        let doc = DocQuery::new(&document, &document_resources);
        assert_eq!(
            Targets::new(doc.view()).source_location_on_line(usize::MAX, Some(0)),
            None
        );
        assert_eq!(
            Targets::new(doc.view()).source_location_on_line(0, Some(u32::MAX)),
            None
        );
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

        let anchor = Targets::new(doc.view())
            .preceding_anchor(beta)
            .expect("an anchor precedes the second paragraph");
        assert_eq!(anchor, "second");
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

        let anchor = Targets::new(doc.view())
            .preceding_anchor_where(beta, |id| {
                doc.view()
                    .lookup_string(id)
                    .is_some_and(|key| only_first.contains(&key))
            })
            .expect("the filtered anchor still resolves");
        assert_eq!(
            anchor, "first",
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
