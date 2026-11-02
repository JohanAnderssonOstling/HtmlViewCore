//! Text and table selection.
//!
//! Selection depends on the document, the page currently laid out and its own
//! state -- nothing else in the renderer. Queries go through [`SelectionView`];
//! the two state types own their own transitions. Nothing here touches the
//! clipboard: the copy methods return the payload and the caller decides what
//! to do with it.

use html::export::{DocumentExport, ExportFormat, ExportTarget};
use html::geometry::{Geometry, WordHit};
use html::layout::RenderTable;
use html::render::Painter;
use html::selection::{Selection, SelectionGeometry, SelectionState as DocumentSelectionState};
use html_view_doc::TextGeometry;
use kurbo::{Point, Rect};
use peniko::Color;
use std::ops::{Deref, DerefMut};

pub use html::selection::{
    SelectionInteraction, SelectionMode, TableCellRange, TableHit,
    TableSelectionState, table_selection_markdown,
};

#[derive(Clone, Default)]
pub struct SelectionState {
    logical: DocumentSelectionState,
    link_cursor_active: bool,
    pending_link_glyph: Option<u32>,
}

impl Deref for SelectionState {
    type Target = DocumentSelectionState;
    fn deref(&self) -> &Self::Target {
        &self.logical
    }
}
impl DerefMut for SelectionState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.logical
    }
}
impl SelectionState {
    pub fn begin_at_word(&mut self, hit: WordHit, mode: SelectionMode) {
        self.pending_link_glyph = Some(hit.caret);
        self.logical.begin_at_word(hit, mode);
    }
    pub fn update_word_interaction(
        &mut self,
        hit: Option<WordHit>,
        mode: SelectionMode,
    ) -> bool {
        let changed = self.logical.update_word_interaction(hit, mode);
        if self.logical.range().is_some() {
            self.pending_link_glyph = None;
        }
        changed
    }
    pub fn finish_interaction(&mut self) -> Option<u32> {
        self.logical.finish_interaction();
        let interaction = self.logical.interaction();
        let click = self.pending_link_glyph.take();
        click.filter(|glyph| {
            interaction.anchor() == Some(*glyph) && interaction.active() == Some(*glyph)
        })
    }
    pub fn cancel_interaction(&mut self) {
        self.logical.cancel_interaction();
        self.pending_link_glyph = None;
    }
    pub fn select_range(&mut self, anchor: u32, active: u32, mode: SelectionMode) {
        self.logical.select_range(anchor, active, mode);
        self.pending_link_glyph = None;
    }
    pub fn link_cursor_active(&self) -> bool {
        self.link_cursor_active
    }
    pub fn activate_link_cursor(&mut self) -> bool {
        !std::mem::replace(&mut self.link_cursor_active, true)
    }
    pub fn deactivate_link_cursor(&mut self) -> bool {
        std::mem::replace(&mut self.link_cursor_active, false)
    }
}

/// A selection together with everything needed to answer questions about it.
#[derive(Clone, Copy)]
pub struct SelectionView<'a> {
    pub geo: TextGeometry<'a>,
    pub state: &'a SelectionState,
    pub table: &'a TableSelectionState,
}

impl<'a> SelectionView<'a> {
    pub fn new(
        geo: TextGeometry<'a>,
        state: &'a SelectionState,
        table: &'a TableSelectionState,
    ) -> Self {
        Self { geo, state, table }
    }

    pub fn range(&self) -> Option<(u32, u32)> {
        self.state.range().map(|range| (range.start, range.end))
    }

    /// Returns whether a point intersects the selection as it is painted.
    pub fn contains_point(&self, position: Point) -> bool {
        let Some((start, end)) = self.range() else {
            return false;
        };
        let plan = self.document_selection(start, end).geometry_plan();
        self.geo.document_geometry().selection_contains_lines(
            &plan,
            position,
            self.geo.placements(),
            |index| {
                let mut hit = false;
                self.geo
                    .for_each_projected_box_rect(index, |rect| hit |= rect.contains(position));
                hit
            },
        )
    }

    pub fn geometry_plan(&self, start: u32, end: u32) -> SelectionGeometry {
        self.document_selection(start, end).geometry_plan()
    }

    fn document_selection(&self, start: u32, end: u32) -> Selection<'a> {
        let selection = Selection::new(self.geo.doc.view(), start..end);
        if self.state.interaction().is_semantic() {
            selection.with_semantics()
        } else {
            selection
        }
    }

    pub fn table_hit(&self, position: Point) -> Option<TableHit> {
        Geometry::new(self.geo.doc.view()).table_at_where(|box_idx| {
            let mut hit = false;
            self.geo.for_each_projected_box_rect(box_idx, |rect| {
                hit |= rect.contains(position);
            });
            hit
        })
    }

    pub fn table_at(&self, position: Point) -> bool {
        self.table_hit(position).is_some()
    }

    pub fn table_selection_at(&self, position: Point) -> bool {
        self.table_hit(position)
            .is_some_and(|hit| self.table.covers(hit))
    }

    pub fn copy_table_at(&self, position: Point) -> Option<String> {
        self.export_table_at(position, ExportFormat::Markdown)
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> Option<String> {
        self.export_table_at(position, ExportFormat::Html)
    }

    pub fn copy_table_styled_html_at(&self, position: Point) -> Option<String> {
        self.export_table_at(position, ExportFormat::AuthoredHtml)
    }

    fn export_table_at(&self, position: Point, format: ExportFormat) -> Option<String> {
        let hit = self.table_hit(position)?;
        DocumentExport::new(self.geo.doc.view())
            .request(ExportTarget::Table(hit.table_box), format)
            .ok()??
            .into_text()
    }

    pub fn copy_table_selection(&self) -> Option<String> {
        let range = self.table.range()?;
        DocumentExport::new(self.geo.doc.view())
            .request(ExportTarget::TableCells(range), ExportFormat::Markdown)
            .ok()??
            .into_text()
    }

    pub fn paint_table_selection(&self, painter: &mut impl Painter, plan: &SelectionGeometry) {
        if let Some(range) = self.table.range()
            && let Some(table) = self.geo.doc.boxes().table(range.table_box)
        {
            self.paint_bordered_table_cell_range(painter, &table, range);
        }
        for &box_idx in &plan.cell_boxes {
            self.geo.for_each_projected_box_rect(box_idx, |rect| {
                painter.fill_rect(rect, Color::rgba8(72, 133, 237, 54));
            });
        }
    }

    fn paint_bordered_table_cell_range(
        &self,
        painter: &mut impl Painter,
        table: &RenderTable,
        range: TableCellRange,
    ) {
        let fill = Color::rgba8(72, 133, 237, 54);
        let border = Color::rgba8(45, 102, 201, 210);
        for cell in range.cells(table) {
            self.geo
                .for_each_projected_box_rect(cell.box_idx(), |rect| {
                    painter.fill_rect(rect, fill);
                    let width = 1.5;
                    painter.fill_rect(
                        Rect::new(rect.x0, rect.y0, rect.x1, rect.y0 + width),
                        border,
                    );
                    painter.fill_rect(
                        Rect::new(rect.x0, rect.y1 - width, rect.x1, rect.y1),
                        border,
                    );
                    painter.fill_rect(
                        Rect::new(rect.x0, rect.y0 + width, rect.x0 + width, rect.y1 - width),
                        border,
                    );
                    painter.fill_rect(
                        Rect::new(rect.x1 - width, rect.y0 + width, rect.x1, rect.y1 - width),
                        border,
                    );
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use html::geometry::WordHit;
    use kurbo::Point;
    use html::text::{GlyphRequest, ShapedGlyph};

    use super::{SelectionMode, SelectionState, TableCellRange, table_selection_markdown};
    use html::layout::LayoutConstraints;
    use html::layout::{
        FontSlant, GlyphId, GlyphMetric, GlyphResourceGeneration, GlyphResourceStore, GlyphShaper,
    };
    use html::pipeline::DocumentFactory;
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        generation: GlyphResourceGeneration,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceGeneration)>,
    }

    impl GlyphShaper for TestGlyphShaper {
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

    fn word_hit(caret: u32, word: std::ops::Range<u32>) -> WordHit {
        WordHit { caret, word, position: Point::new(caret as f64 * 10.0, 0.0) }
    }

    #[test]
    fn pointer_drag_can_cross_words_without_changing_caret_and_ignores_jitter() {
        let mut selection = SelectionState::default();
        selection.begin_at_word(word_hit(1, 0..1), SelectionMode::Plain);
        let jitter = WordHit { position: Point::new(12.0, 0.0), ..word_hit(1, 0..1) };
        assert!(!selection.update_word_interaction(Some(jitter), SelectionMode::Plain));
        assert_eq!(selection.finish_interaction(), Some(1));

        selection.begin_at_word(word_hit(1, 0..1), SelectionMode::Plain);
        let next_word = WordHit { position: Point::new(30.0, 0.0), ..word_hit(1, 1..2) };
        assert!(selection.update_word_interaction(Some(next_word), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(0..2));
        assert_eq!(selection.finish_interaction(), None);

        selection.begin_at_word(word_hit(1, 0..1), SelectionMode::Plain);
        let same_word = WordHit { position: Point::new(30.0, 0.0), ..word_hit(1, 0..1) };
        assert!(selection.update_word_interaction(Some(same_word), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(0..1));
    }

    #[test]
    fn word_drag_preserves_clicks_and_snaps_in_both_directions() {
        let mut selection = SelectionState::default();
        selection.begin_at_word(word_hit(12, 10..15), SelectionMode::Plain);
        assert!(!selection.update_word_interaction(Some(word_hit(12, 10..15)), SelectionMode::Plain));
        assert_eq!(selection.range(), None);
        assert_eq!(selection.finish_interaction(), Some(12));

        selection.begin_at_word(word_hit(12, 10..15), SelectionMode::Plain);
        assert!(selection.update_word_interaction(Some(word_hit(13, 10..15)), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(10..15));
        assert!(!selection.update_word_interaction(Some(word_hit(14, 10..15)), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(10..15));
        assert!(selection.update_word_interaction(Some(word_hit(22, 20..25)), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(10..25));
        assert!(selection.update_word_interaction(Some(word_hit(3, 0..5)), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(0..15));
        assert!(selection.update_word_interaction(Some(word_hit(12, 10..15)), SelectionMode::Plain));
        assert_eq!(selection.range(), Some(10..15));
        assert!(selection.update_word_interaction(None, SelectionMode::Semantic));
        assert_eq!(selection.range(), Some(10..15));
        assert!(selection.interaction().is_semantic());
        assert!(!selection.update_word_interaction(None, SelectionMode::Semantic));
        assert!(!selection.update_word_interaction(Some(word_hit(14, 10..15)), SelectionMode::Plain));
        assert_eq!(selection.finish_interaction(), None);
    }

    #[test]
    fn selection_interaction_distinguishes_a_click_from_a_drag() {
        let mut selection = SelectionState::default();
        selection.begin_at_word(word_hit(12, 10..15), SelectionMode::Plain);

        assert_eq!(selection.finish_interaction(), Some(12));

        selection.begin_at_word(word_hit(12, 10..15), SelectionMode::Plain);
        assert!(selection.update_word_interaction(Some(word_hit(18, 17..22)), SelectionMode::Semantic));
        assert!(selection.interaction().is_semantic());
        assert_eq!(selection.finish_interaction(), None);
        assert_eq!(selection.range(), Some(10..22));
    }

    #[test]
    fn selection_update_outside_text_preserves_the_range_and_promotes_its_mode() {
        let mut selection = SelectionState::default();
        selection.begin_at_word(word_hit(7, 5..10), SelectionMode::Plain);

        assert!(!selection.update_word_interaction(None, SelectionMode::Semantic));
        assert!(selection.interaction().is_semantic());
        assert_eq!(selection.interaction().active(), Some(7));
        assert_eq!(selection.finish_interaction(), Some(7));
    }

    #[test]
    fn markdown_expands_spans_escapes_pipes_and_preserves_data_rows() {
        let mut factory = DocumentFactory::new();
        let prepared =
            factory.parse_with_new_pipeline("<table class='ledger' style='color:red' onclick='unsafe()'><tr><th colspan='2'>A | B</th></tr><tr><td rowspan='2'>One</td><td>Two &amp; more</td></tr><tr><td>Three</td></tr></table>", None);
        let mut shaper = TestGlyphShaper::default();
        let mut shaped_resources = GlyphResourceStore::default();
        let shaped = prepared.shape(&mut shaper, &mut shaped_resources).unwrap();
        let document = shaped
            .layout(
                LayoutConstraints::new(500.0, 20.0).unwrap(),
                &shaped_resources,
            )
            .expect("test layout succeeds");
        let boxes = document.render_view(&shaped_resources).boxes();
        let table = (0..boxes.len())
            .find(|&index| boxes.is_table(index))
            .and_then(|index| boxes.table(index))
            .unwrap();
        assert_eq!(
            table.markdown().as_deref(),
            Some("| A \\| B |  |\n| --- | --- |\n| One | Two & more |\n|  | Three |")
        );
        assert_eq!(
            table.unstyled_html().as_deref(),
            Some(
                "<table><tr><th colspan=\"2\">A | B</th></tr><tr><td rowspan=\"2\">One</td><td>Two &amp; more</td></tr><tr><td>Three</td></tr></table>"
            )
        );
        assert!(table.authored_html().contains("class=\"ledger\""));
        assert!(table.authored_html().contains("style=\"color:red\""));
        assert!(!table.authored_html().contains("onclick"));

        let range = TableCellRange {
            table_box: table.box_idx(),
            row_start: 1,
            row_end: 2,
            column_start: 1,
            column_end: 1,
        };
        assert_eq!(
            table_selection_markdown(&table, range).as_deref(),
            Some("|  |\n| --- |\n| Two & more |\n| Three |")
        );
    }
}
