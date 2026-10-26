//! Text and table selection.
//!
//! Selection depends on the document, the page currently laid out and its own
//! state -- nothing else in the renderer. Queries go through [`SelectionView`];
//! the two state types own their own transitions. Nothing here touches the
//! clipboard: the copy methods return the payload and the caller decides what
//! to do with it.

use html::layout::{ContentRole, RenderTable, RenderTableCell};
use html::render::Painter;
use html_view_doc::TextGeometry;
use kurbo::{Point, Rect};
use peniko::Color;
use std::collections::HashSet;
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    Plain,
    Semantic,
}

impl SelectionMode {
    pub fn is_semantic(self) -> bool {
        matches!(self, Self::Semantic)
    }

    fn promote(self, requested: Self) -> Self {
        if self.is_semantic() || requested.is_semantic() {
            Self::Semantic
        } else {
            Self::Plain
        }
    }
}

#[derive(Clone, Default)]
pub struct SelectionState {
    interaction: SelectionInteraction,
    content: SelectionContent,
    link_cursor_active: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionInteraction {
    selecting: bool,
    anchor: Option<u32>,
    active: Option<u32>,
    pending_link_glyph: Option<u32>,
    mode: SelectionMode,
}

impl SelectionInteraction {
    pub fn is_selecting(self) -> bool {
        self.selecting
    }

    pub fn anchor(self) -> Option<u32> {
        self.anchor
    }

    pub fn active(self) -> Option<u32> {
        self.active
    }

    pub fn is_semantic(self) -> bool {
        self.mode.is_semantic()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionContent {
    plain: Option<String>,
    markdown: Option<String>,
}

impl SelectionContent {
    pub fn plain(&self) -> Option<&str> {
        self.plain.as_deref()
    }

    pub fn markdown(&self) -> Option<&str> {
        self.markdown.as_deref()
    }
}

impl SelectionState {
    pub fn interaction(&self) -> SelectionInteraction {
        self.interaction
    }

    pub fn content(&self) -> &SelectionContent {
        &self.content
    }

    pub fn link_cursor_active(&self) -> bool {
        self.link_cursor_active
    }

    /// Starts a pointer selection. Until the active glyph moves, the gesture
    /// may still resolve to a link click on the same glyph.
    pub fn begin_at_glyph(&mut self, glyph: u32, mode: SelectionMode) {
        self.interaction.anchor = Some(glyph);
        self.interaction.active = Some(glyph);
        self.interaction.selecting = true;
        self.interaction.mode = mode;
        self.interaction.pending_link_glyph = Some(glyph);
    }

    /// Updates the active gesture. A pointer outside the text still promotes
    /// the selection mode, but cannot move the active glyph.
    pub fn update_interaction(&mut self, glyph: Option<u32>, mode: SelectionMode) -> bool {
        if !self.interaction.selecting {
            return false;
        }
        self.interaction.mode = self.interaction.mode.promote(mode);
        let Some(glyph) = glyph else {
            return false;
        };
        self.interaction.active = Some(glyph);
        if self.interaction.anchor != self.interaction.active {
            self.interaction.pending_link_glyph = None;
        }
        true
    }

    /// Ends the active gesture and returns the glyph when it remained a click.
    pub fn finish_interaction(&mut self) -> Option<u32> {
        self.interaction.selecting = false;
        let click = match (
            self.interaction.anchor,
            self.interaction.active,
            self.interaction.pending_link_glyph,
        ) {
            (Some(anchor), Some(active), Some(pending))
                if anchor == active && pending == anchor =>
            {
                Some(anchor)
            }
            _ => None,
        };
        self.interaction.pending_link_glyph = None;
        click
    }

    /// Cancels only the in-progress gesture, preserving the selected range and
    /// its derived copy payload.
    pub fn cancel_interaction(&mut self) {
        self.interaction.selecting = false;
        self.interaction.pending_link_glyph = None;
    }

    /// Replaces the logical range outside a pointer gesture, for example when
    /// search or another semantic command creates a selection.
    pub fn select_range(&mut self, anchor: u32, active: u32, mode: SelectionMode) {
        self.interaction.anchor = Some(anchor);
        self.interaction.active = Some(active);
        self.interaction.mode = mode;
        self.interaction.selecting = false;
        self.interaction.pending_link_glyph = None;
        self.clear_text();
    }

    pub fn install_text(&mut self, plain: Option<String>, markdown: Option<String>) {
        self.content.plain = plain;
        self.content.markdown = markdown;
    }

    pub fn clear_text(&mut self) {
        self.install_text(None, None);
    }

    /// Returns whether the cursor state changed and therefore needs restyling.
    pub fn activate_link_cursor(&mut self) -> bool {
        if self.link_cursor_active {
            return false;
        }
        self.link_cursor_active = true;
        true
    }

    pub fn deactivate_link_cursor(&mut self) -> bool {
        if !self.link_cursor_active {
            return false;
        }
        self.link_cursor_active = false;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableCellRange {
    table_box: usize,
    row_start: usize,
    row_end: usize,
    column_start: usize,
    column_end: usize,
}

#[derive(Clone, Copy)]
enum TableRangePaint {
    Fill,
    FillWithBorder,
}

#[derive(Clone, Copy)]
enum TableAnchorMode {
    Anchored,
    Dragging,
}
#[derive(Default)]
pub struct TableSelectionState {
    range: Option<TableCellRange>,
    anchor: Option<(usize, usize, usize, usize, usize)>,
    dragging: bool,
}

struct SemanticTableSelection {
    table: RenderTable,
    range: TableCellRange,
    glyph_start: u32,
    glyph_end: u32,
    cell_glyph_ranges: Vec<std::ops::Range<u32>>,
}

struct SemanticTextUnit {
    glyph_start: u32,
    glyph_end: u32,
    glyph_ranges: Vec<Range<u32>>,
    box_idx: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct TableHit {
    table_box: usize,
    row: usize,
    column: usize,
    row_end: usize,
    column_end: usize,
}


fn table_selection_markdown(table: &RenderTable, range: TableCellRange) -> Option<String> {
    table.markdown_in(range.row_start..range.row_end.saturating_add(1), range.column_start..range.column_end.saturating_add(1))
}

impl TableSelectionState {
    pub fn is_active(&self) -> bool {
        self.range.is_some()
    }

    pub(crate) fn range(&self) -> Option<TableCellRange> {
        self.range
    }

    pub fn covers(&self, hit: TableHit) -> bool {
        self.range.is_some_and(|range| range.table_box == hit.table_box)
    }

    fn anchor_to(&mut self, hit: TableHit, mode: TableAnchorMode) {
        self.anchor = Some((hit.table_box, hit.row, hit.row_end, hit.column, hit.column_end));
        self.range = Some(TableCellRange { table_box: hit.table_box, row_start: hit.row, row_end: hit.row_end, column_start: hit.column, column_end: hit.column_end });
        self.dragging = matches!(mode, TableAnchorMode::Dragging);
    }

    pub fn begin(&mut self, hit: TableHit) {
        self.anchor_to(hit, TableAnchorMode::Anchored);
    }

    /// Starts a drag from an existing selection. Returns whether `hit` was in
    /// the selected table; a drag may not start in a different one.
    pub fn begin_drag(&mut self, hit: TableHit) -> bool {
        if !self.covers(hit) {
            return false;
        }
        self.anchor_to(hit, TableAnchorMode::Dragging);
        true
    }

    /// Extends an in-progress drag to `hit`. Returns `None` when no drag is in
    /// progress, otherwise whether the selected range actually moved. A hit
    /// outside the table, or no hit at all, holds the range where it is rather
    /// than cancelling the drag.
    pub fn update_drag(&mut self, hit: Option<TableHit>) -> Option<bool> {
        if !self.dragging {
            return None;
        }
        let (Some(hit), Some((table_box, anchor_row, anchor_row_end, anchor_column, anchor_column_end))) = (hit, self.anchor) else {
            return Some(false);
        };
        if hit.table_box != table_box {
            return Some(false);
        }
        let next = TableCellRange { table_box, row_start: anchor_row.min(hit.row), row_end: anchor_row_end.max(hit.row_end), column_start: anchor_column.min(hit.column), column_end: anchor_column_end.max(hit.column_end) };
        if self.range == Some(next) {
            return Some(false);
        }
        self.range = Some(next);
        Some(true)
    }

    pub fn finish_drag(&mut self) -> bool {
        std::mem::replace(&mut self.dragging, false)
    }

    /// Drops the selection. Returns whether there was one to drop.
    pub fn clear(&mut self) -> bool {
        if self.range.take().is_none() {
            return false;
        }
        self.anchor = None;
        self.dragging = false;
        true
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
    pub fn new(geo: TextGeometry<'a>, state: &'a SelectionState, table: &'a TableSelectionState) -> Self {
        Self { geo, state, table }
    }

    pub fn range(&self) -> Option<(u32, u32)> {
        // normalize selection anchor/active into a range
        let interaction = self.state.interaction();
        let (Some(a), Some(b)) = (interaction.anchor(), interaction.active()) else {
            return None;
        };
        if a == b {
            return None;
        }
        let (start, end) = if a < b { (a, b) } else { (b, a) }; // normalize range order
        Some((start, end))
    }

    /// Returns whether a point intersects the selection as it is painted.
    pub fn contains_point(&self, position: Point) -> bool {
        let Some((selection_start, selection_end)) = self.range() else {
            return false;
        };
        if self.semantic_selection_contains_point(position, selection_start, selection_end) {
            return true;
        }
        let promoted = self.semantic_promoted_glyph_ranges(selection_start, selection_end);
        for (line_idx, screen) in self.geo.frame.page().line_positions().iter() {
            let Some(line) = self.geo.doc.text().line(line_idx) else {
                continue;
            };
            if position.y < screen.point.y || position.y > screen.point.y + line.height() {
                continue;
            }

            for range in std::iter::once(selection_start..selection_end).chain(promoted.iter().cloned()) {
                if self.geo.line_selection_rects(line_idx, range, screen.point).any(|rect| rect.contains(position)) {
                    return true;
                }
            }
        }
        false
    }

    pub fn for_each_projected_box_rect(&self, box_idx: usize, mut visit: impl FnMut(Rect)) {
        let boxes = self.geo.doc.boxes();
        let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else { return };
        let rect = Rect::new(point.x, point.y, point.x + size.width, point.y + size.height);
        if rect.width() <= 0.0 || rect.height() <= 0.0 || self.geo.layout.size.height <= 0.0 {
            return;
        }
        let mut segment_start = rect.y0;
        let mut break_index = 0;
        let mut offset = 0.0;
        while break_index < self.geo.frame.page().offset_breaks().len() && self.geo.frame.page().offset_breaks()[break_index].0 <= segment_start {
            offset = self.geo.frame.page().offset_breaks()[break_index].1;
            break_index += 1;
        }
        while segment_start < rect.y1 {
            let next_break = self.geo.frame.page().offset_breaks().get(break_index).map(|entry| entry.0).unwrap_or(rect.y1);
            let segment_end = rect.y1.min(next_break);
            let page_y0 = segment_start + offset - self.geo.frame.page().paint_start_offset_y();
            let page_y1 = segment_end + offset - self.geo.frame.page().paint_start_offset_y();
            let height = self.geo.layout.size.height;
            let first_column = (page_y0 / height).floor().max(0.0) as i32;
            let last_column = ((page_y1 - 0.0001) / height).floor().min(self.geo.layout.col_count - 1.0) as i32;
            for column in first_column..=last_column {
                let column_top = f64::from(column) * height;
                let y0 = page_y0.max(column_top);
                let y1 = page_y1.min(column_top + height);
                if y1 > y0 {
                    let x0 = self.geo.layout.col_x(f64::from(column), rect.x0);
                    visit(Rect::new(x0, y0 - column_top, x0 + rect.width(), y1 - column_top));
                }
            }
            segment_start = segment_end;
            if break_index < self.geo.frame.page().offset_breaks().len() && self.geo.frame.page().offset_breaks()[break_index].0 <= segment_start {
                offset = self.geo.frame.page().offset_breaks()[break_index].1;
                break_index += 1;
            }
        }
    }

    fn semantic_text_units(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticTextUnit> {
        if !self.state.interaction().is_semantic() || selection_start >= selection_end {
            return Vec::new();
        }
        let view = self.geo.doc.view();
        let semantics = view.semantics();
        let boxes = view.boxes();
        let mut list_items = HashSet::new();
        let mut code_blocks = HashSet::new();
        for run in view.text().text_runs() {
            let glyphs = run.glyphs();
            if glyphs.start >= selection_end || glyphs.end <= selection_start || semantics.nearest(run.box_idx(), ContentRole::Table).is_some() {
                continue;
            }
            if let Some(block) = semantics.nearest(run.box_idx(), ContentRole::CodeBlock) {
                code_blocks.insert(block);
            } else if let Some(item) = semantics.nearest(run.box_idx(), ContentRole::ListItem) {
                list_items.insert(item);
            }
        }
        let selected_list_items = list_items.clone();
        list_items.retain(|item| !boxes.ancestors(*item).any(|ancestor| selected_list_items.contains(&ancestor)));
        code_blocks.retain(|block| !boxes.ancestors(*block).any(|ancestor| list_items.contains(&ancestor)));

        let mut units = Vec::new();
        for (box_idx, is_list_item) in list_items.into_iter().map(|item| (item, true))
            .chain(code_blocks.into_iter().map(|block| (block, false)))
        {
            let ranges = semantics.text_ranges(box_idx, is_list_item);
            if let (Some(start), Some(end)) = (ranges.iter().map(|range| range.start).min(), ranges.iter().map(|range| range.end).max()) {
                // Code remains character-selectable until a drag crosses its
                // boundary. List items also highlight their generated marker.
                if !is_list_item && selection_start >= start && selection_end <= end { continue; }
                units.push(SemanticTextUnit { glyph_start: start, glyph_end: end, glyph_ranges: ranges, box_idx });
            }
        }
        units.sort_by_key(|unit| unit.glyph_start);
        units
    }

    fn semantic_table_selections(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticTableSelection> {
        if !self.state.interaction().is_semantic() || selection_start >= selection_end {
            return Vec::new();
        }
        let view = self.geo.doc.view();
        let boxes = view.boxes();
        let mut selections = Vec::new();
        for table_box in (0..boxes.len()).filter(|&index| boxes.is_table(index)) {
            let Some(table) = boxes.table(table_box) else { continue };
            let cells = table.rows().iter().flat_map(|row| row.cells()).filter_map(|cell| cell.glyphs().map(|glyphs| (cell, glyphs))).collect::<Vec<_>>();
            let touched = cells.iter().filter(|(_, glyphs)| glyphs.start < selection_end && glyphs.end > selection_start).collect::<Vec<_>>();
            if touched.is_empty() {
                continue;
            }
            let row_start = touched.iter().map(|(cell, _)| cell.row()).min().unwrap_or_default();
            let row_end = touched.iter().map(|(cell, _)| cell.row().saturating_add(cell.rowspan()).saturating_sub(1)).max().unwrap_or(row_start);
            let column_start = touched.iter().map(|(cell, _)| cell.column()).min().unwrap_or_default();
            let column_end = touched.iter().map(|(cell, _)| cell.column().saturating_add(cell.colspan()).saturating_sub(1)).max().unwrap_or(column_start);
            let range = TableCellRange { table_box, row_start, row_end, column_start, column_end };
            let cell_glyph_ranges = cells.iter().filter(|(cell, _)| cell_intersects(cell, range)).map(|(_, glyphs)| glyphs.clone()).collect::<Vec<_>>();
            let glyph_start = cell_glyph_ranges.iter().map(|range| range.start).min().unwrap_or(selection_start);
            let glyph_end = cell_glyph_ranges.iter().map(|range| range.end).max().unwrap_or(selection_end);
            selections.push(SemanticTableSelection { table, range, glyph_start, glyph_end, cell_glyph_ranges });
        }
        selections.sort_by_key(|selection| selection.glyph_start);
        selections
    }

    pub fn semantic_cell_glyph_ranges(&self, selection_start: u32, selection_end: u32) -> Vec<std::ops::Range<u32>> {
        self.semantic_table_selections(selection_start, selection_end).into_iter().flat_map(|selection| selection.cell_glyph_ranges).collect()
    }

    pub fn semantic_promoted_glyph_ranges(&self, selection_start: u32, selection_end: u32) -> Vec<Range<u32>> {
        self.semantic_text_units(selection_start, selection_end).into_iter().flat_map(|unit| unit.glyph_ranges).collect()
    }

    pub fn semantic_selection_contains_point(&self, position: Point, selection_start: u32, selection_end: u32) -> bool {
        self.semantic_table_selections(selection_start, selection_end).into_iter().any(|selection| {
            selection.table.rows().iter().flat_map(|row| row.cells()).filter(|cell| cell_intersects(cell, selection.range)).any(|cell| {
                let mut contains = false;
                self.for_each_projected_box_rect(cell.box_idx(), |rect| contains |= rect.contains(position));
                contains
            })
        })
    }

    pub fn build_semantic_selection_text(&self, selection_start: u32, selection_end: u32) -> (Option<String>, Option<String>) {
        let export = self.geo.doc.view().export();
        // Discovery is shared with highlighting; serialization happens only
        // when copying. Failed promotion leaves the ordinary selection intact.
        let mut units = self.semantic_text_units(selection_start, selection_end).into_iter().filter_map(|unit| {
            export.markdown_for_box(unit.box_idx).map(|markdown| (unit.glyph_start, unit.glyph_end, markdown))
        }).collect::<Vec<_>>();
        units.extend(self.semantic_table_selections(selection_start, selection_end).into_iter().filter_map(|selection| {
            table_selection_markdown(&selection.table, selection.range).map(|markdown| (selection.glyph_start, selection.glyph_end, markdown))
        }));
        units.sort_by_key(|unit| unit.0);
        if units.is_empty() {
            return export.selection_text(selection_start..selection_end);
        }
        let plain = export.plain_text(selection_start..selection_end);
        let mut markdown = String::new();
        let mut cursor = selection_start;
        let append = |target: &mut String, value: &str| {
            if value.is_empty() {
                return;
            }
            if !target.is_empty() && !target.ends_with('\n') {
                target.push('\n');
            }
            target.push_str(value);
        };
        for (start, end, value) in units {
            let ordinary_end = start.min(selection_end);
            if cursor < ordinary_end && let Some(value) = export.markdown(cursor..ordinary_end) {
                append(&mut markdown, &value);
            }
            append(&mut markdown, &value);
            cursor = cursor.max(end);
        }
        if cursor < selection_end && let Some(value) = export.markdown(cursor..selection_end) {
            append(&mut markdown, &value);
        }
        (plain, (!markdown.is_empty()).then_some(markdown))
    }

    pub fn table_hit(&self, position: Point) -> Option<TableHit> {
        let boxes = self.geo.doc.boxes();
        for table_box in (0..boxes.len()).filter(|&index| boxes.is_table(index)) {
            let Some(table) = boxes.table(table_box) else { continue };
            for row in table.rows() {
                for cell in row.cells() {
                    let mut hit = false;
                    self.for_each_projected_box_rect(cell.box_idx(), |rect| hit |= rect.contains(position));
                    if hit {
                        return Some(TableHit {
                            table_box,
                            row: cell.row(),
                            column: cell.column(),
                            row_end: cell.row().saturating_add(cell.rowspan()).saturating_sub(1),
                            column_end: cell.column().saturating_add(cell.colspan()).saturating_sub(1),
                        });
                    }
                }
            }
        }
        None
    }

    pub fn table_at(&self, position: Point) -> bool {
        self.table_hit(position).is_some()
    }

    pub fn table_selection_at(&self, position: Point) -> bool {
        self.table_hit(position).is_some_and(|hit| self.table.covers(hit))
    }

    pub fn table_selection_active(&self) -> bool {
        self.table.is_active()
    }

    pub fn copy_table_at(&self, position: Point) -> Option<String> {
        let hit = self.table_hit(position)?;
        let table = self.geo.doc.boxes().table(hit.table_box)?;
        table.markdown()
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> Option<String> {
        let hit = self.table_hit(position)?;
        let table = self.geo.doc.boxes().table(hit.table_box)?;
        table.unstyled_html()
    }

    pub fn copy_table_styled_html_at(&self, position: Point) -> Option<String> {
        let hit = self.table_hit(position)?;
        let table = self.geo.doc.boxes().table(hit.table_box)?;
        if table.authored_html().is_empty() {
            return None;
        }
        Some(table.authored_html().to_owned())
    }

    pub fn copy_table_selection(&self) -> Option<String> {
        let range = self.table.range()?;
        let table = self.geo.doc.boxes().table(range.table_box)?;
        table_selection_markdown(&table, range)
    }

    pub fn paint_table_selection(&self, painter: &mut impl Painter) {
        if let Some(range) = self.table.range()
            && let Some(table) = self.geo.doc.boxes().table(range.table_box)
        {
            self.paint_bordered_table_cell_range(painter, &table, range);
        }
        if let Some((start, end)) = self.range() {
            for selection in self.semantic_table_selections(start, end) {
                self.paint_filled_table_cell_range(painter, &selection.table, selection.range);
            }
        }
    }

    fn paint_bordered_table_cell_range(
        &self,
        painter: &mut impl Painter,
        table: &RenderTable,
        range: TableCellRange,
    ) {
        self.paint_table_cell_range(painter, table, range, TableRangePaint::FillWithBorder);
    }

    fn paint_filled_table_cell_range(
        &self,
        painter: &mut impl Painter,
        table: &RenderTable,
        range: TableCellRange,
    ) {
        self.paint_table_cell_range(painter, table, range, TableRangePaint::Fill);
    }

    fn paint_table_cell_range(
        &self,
        painter: &mut impl Painter,
        table: &RenderTable,
        range: TableCellRange,
        paint: TableRangePaint,
    ) {
        let fill = Color::rgba8(72, 133, 237, 54);
        let border = Color::rgba8(45, 102, 201, 210);
        for row in table.rows() {
            for cell in row.cells() {
                if !cell_intersects(cell, range) {
                    continue;
                }
                self.for_each_projected_box_rect(cell.box_idx(), |rect| {
                    painter.fill_rect(rect, fill);
                    if matches!(paint, TableRangePaint::FillWithBorder) {
                        let width = 1.5;
                        painter.fill_rect(Rect::new(rect.x0, rect.y0, rect.x1, rect.y0 + width), border);
                        painter.fill_rect(Rect::new(rect.x0, rect.y1 - width, rect.x1, rect.y1), border);
                        painter.fill_rect(Rect::new(rect.x0, rect.y0 + width, rect.x0 + width, rect.y1 - width), border);
                        painter.fill_rect(Rect::new(rect.x1 - width, rect.y0 + width, rect.x1, rect.y1 - width), border);
                    }
                });
            }
        }
    }
}

fn cell_intersects(cell: &RenderTableCell, range: TableCellRange) -> bool {
    let row_end = cell.row().saturating_add(cell.rowspan()).saturating_sub(1);
    let column_end = cell.column().saturating_add(cell.colspan()).saturating_sub(1);
    cell.row() <= range.row_end && row_end >= range.row_start && cell.column() <= range.column_end && column_end >= range.column_start
}

#[cfg(test)]
mod tests {
    use super::{SelectionMode, SelectionState, TableCellRange, table_selection_markdown};
    use html::layout::LayoutConstraints;
    use html::pipeline::DocumentFactory;
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphResourceStore, GlyphShaper};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        glyph_store: GlyphResourceStore,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceStore)>,
    }

    impl GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.glyph_store.clear();
        }

        fn glyph_resources(&mut self) -> &mut GlyphResourceStore {
            &mut self.glyph_store
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::layout::ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.glyph_store.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.glyph_store = glyph_store;
            }
        }

        fn shape_glyph(&mut self, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, html::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = self.glyph_store.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    #[test]
    fn selection_interaction_distinguishes_a_click_from_a_drag() {
        let mut selection = SelectionState::default();
        selection.begin_at_glyph(12, SelectionMode::Plain);

        assert_eq!(selection.finish_interaction(), Some(12));

        selection.begin_at_glyph(12, SelectionMode::Plain);
        assert!(selection.update_interaction(Some(18), SelectionMode::Semantic));
        assert!(selection.interaction().is_semantic());
        assert_eq!(selection.finish_interaction(), None);
        assert_eq!(selection.interaction().anchor(), Some(12));
        assert_eq!(selection.interaction().active(), Some(18));
    }

    #[test]
    fn selection_update_outside_text_preserves_the_range_and_promotes_its_mode() {
        let mut selection = SelectionState::default();
        selection.begin_at_glyph(7, SelectionMode::Plain);

        assert!(!selection.update_interaction(None, SelectionMode::Semantic));
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
        let shaped = prepared.shape(&mut shaper).unwrap();
        let document = shaped.layout(LayoutConstraints::new(500.0, 20.0).unwrap());
        let boxes = document.render_view().boxes();
        let table = (0..boxes.len()).find(|&index| boxes.is_table(index)).and_then(|index| boxes.table(index)).unwrap();
        assert_eq!(table.markdown().as_deref(), Some("| A \\| B |  |\n| --- | --- |\n| One | Two & more |\n|  | Three |"));
        assert_eq!(table.unstyled_html().as_deref(), Some("<table><tr><th colspan=\"2\">A | B</th></tr><tr><td rowspan=\"2\">One</td><td>Two &amp; more</td></tr><tr><td>Three</td></tr></table>"));
        assert!(table.authored_html().contains("class=\"ledger\""));
        assert!(table.authored_html().contains("style=\"color:red\""));
        assert!(!table.authored_html().contains("onclick"));

        let range = TableCellRange { table_box: table.box_idx(), row_start: 1, row_end: 2, column_start: 1, column_end: 1 };
        assert_eq!(table_selection_markdown(&table, range).as_deref(), Some("|  |\n| --- |\n| Two & more |\n| Three |"));
    }
}
