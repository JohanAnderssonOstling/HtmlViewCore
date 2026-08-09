//! Text and table selection.
//!
//! Selection depends on the document, the page currently laid out and its own
//! state -- nothing else in the renderer. Queries go through [`SelectionView`];
//! the two state types own their own transitions. Nothing here touches the
//! clipboard: the copy methods return the payload and the caller decides what
//! to do with it.

use html::layout::{RenderTable, RenderTableCell};
use html::render::Painter;
use html_view_doc::TextGeometry;
use kurbo::{Point, Rect};
use peniko::Color;
use std::collections::HashSet;
use std::ops::Range;

#[derive(Default)]
pub struct SelectionState {
    pub is_selecting: bool,
    pub selection_anchor: Option<u32>,
    pub selection_active: Option<u32>,
    pub pending_link_glyph: Option<u32>,
    pub selected_text: Option<String>,
    pub selected_text_markdown: Option<String>,
    pub link_cursor_active: bool,
    pub semantic_selection: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableCellRange {
    table_box: usize,
    row_start: usize,
    row_end: usize,
    column_start: usize,
    column_end: usize,
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

struct SemanticMarkdownUnit {
    glyph_start: u32,
    glyph_end: u32,
    glyph_ranges: Vec<Range<u32>>,
    markdown: String,
}

#[derive(Clone, Copy, Debug)]
pub struct TableHit {
    table_box: usize,
    row: usize,
    column: usize,
    row_end: usize,
    column_end: usize,
}

fn markdown_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
fn table_plain_text(table: &RenderTable, selection: Option<TableCellRange>) -> Option<String> {
    let (grid, _) = table_grid(table, selection)?;
    Some(grid.into_iter().map(|row| row.join("\t")).collect::<Vec<_>>().join("\n"))
}

fn table_unstyled_html(table: &RenderTable) -> Option<String> {
    if table.rows().is_empty() {
        return None;
    }
    let mut html = String::from("<table>");
    for row in table.rows() {
        html.push_str("<tr>");
        for cell in row.cells() {
            let tag = if cell.is_header() { "th" } else { "td" };
            html.push('<');
            html.push_str(tag);
            if cell.rowspan() > 1 {
                html.push_str(&format!(" rowspan=\"{}\"", cell.rowspan()));
            }
            if cell.colspan() > 1 {
                html.push_str(&format!(" colspan=\"{}\"", cell.colspan()));
            }
            html.push('>');
            html.push_str(&html_escape(cell.text()));
            html.push_str("</");
            html.push_str(tag);
            html.push('>');
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    Some(html)
}

fn table_grid(table: &RenderTable, selection: Option<TableCellRange>) -> Option<(Vec<Vec<String>>, bool)> {
    let (row_start, row_end, column_start, column_end) = selection.map_or_else(
        || (0, table.rows().len().saturating_sub(1), 0, table.column_count().saturating_sub(1)),
        |range| (range.row_start, range.row_end.min(table.rows().len().saturating_sub(1)), range.column_start, range.column_end.min(table.column_count().saturating_sub(1))),
    );
    if row_start > row_end || column_start > column_end {
        return None;
    }
    let width = column_end - column_start + 1;
    let mut grid = vec![vec![String::new(); width]; row_end - row_start + 1];
    let mut first_row_has_header = false;
    for row in table.rows() {
        for cell in row.cells() {
            let cell_row_end = cell.row().saturating_add(cell.rowspan()).saturating_sub(1);
            let cell_column_end = cell.column().saturating_add(cell.colspan()).saturating_sub(1);
            if cell_row_end < row_start || cell.row() > row_end || cell_column_end < column_start || cell.column() > column_end {
                continue;
            }
            let target_row = cell.row().max(row_start) - row_start;
            let target_column = cell.column().max(column_start) - column_start;
            if cell.row() == row_start && cell.is_header() {
                first_row_has_header = true;
            }
            grid[target_row][target_column] = cell.text().to_owned();
        }
    }
    Some((grid, first_row_has_header))
}

fn table_markdown(table: &RenderTable, selection: Option<TableCellRange>) -> Option<String> {
    let (mut grid, first_row_has_header) = table_grid(table, selection)?;
    for row in &mut grid {
        for cell in row {
            *cell = markdown_escape(cell);
        }
    }
    let width = grid.first()?.len();

    let format_row = |row: &[String]| format!("| {} |", row.join(" | "));
    let separator = format!("| {} |", std::iter::repeat_n("---", width).collect::<Vec<_>>().join(" | "));
    let mut lines = Vec::new();
    if first_row_has_header {
        lines.push(format_row(&grid[0]));
        lines.push(separator);
        lines.extend(grid.iter().skip(1).map(|row| format_row(row)));
    } else {
        lines.push(format_row(&vec![String::new(); width]));
        lines.push(separator);
        lines.extend(grid.iter().map(|row| format_row(row)));
    }
    Some(lines.join("\n"))
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

    fn anchor_to(&mut self, hit: TableHit, dragging: bool) {
        self.anchor = Some((hit.table_box, hit.row, hit.row_end, hit.column, hit.column_end));
        self.range = Some(TableCellRange { table_box: hit.table_box, row_start: hit.row, row_end: hit.row_end, column_start: hit.column, column_end: hit.column_end });
        self.dragging = dragging;
    }

    pub fn begin(&mut self, hit: TableHit) {
        self.anchor_to(hit, false);
    }

    /// Starts a drag from an existing selection. Returns whether `hit` was in
    /// the selected table; a drag may not start in a different one.
    pub fn begin_drag(&mut self, hit: TableHit) -> bool {
        if !self.covers(hit) {
            return false;
        }
        self.anchor_to(hit, true);
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
        let (Some(a), Some(b)) = (self.state.selection_anchor, self.state.selection_active) else {
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
        for (line_idx, screen) in self.geo.frame.last_line_positions.iter() {
            let Some(line) = self.geo.doc.text().line(line_idx) else {
                continue;
            };
            if position.y < screen.point.y || position.y > screen.point.y + line.height() {
                continue;
            }

            let mut ranges = self.geo.doc.line_text_intersections(line_idx, selection_start, selection_end).collect::<Vec<_>>();
            for range in &promoted {
                ranges.extend(self.geo.doc.line_text_intersections(line_idx, range.start, range.end));
            }
            for (start, end) in ranges {
                let (x0, x1) = self.geo.glyph_x_span_in_line(&line, start, end);
                let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };

                if position.x >= x0 && position.x <= x1 {
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
        while break_index < self.geo.frame.offset_breaks.len() && self.geo.frame.offset_breaks[break_index].0 <= segment_start {
            offset = self.geo.frame.offset_breaks[break_index].1;
            break_index += 1;
        }
        while segment_start < rect.y1 {
            let next_break = self.geo.frame.offset_breaks.get(break_index).map(|entry| entry.0).unwrap_or(rect.y1);
            let segment_end = rect.y1.min(next_break);
            let page_y0 = segment_start + offset - self.geo.frame.paint_start_offset_y;
            let page_y1 = segment_end + offset - self.geo.frame.paint_start_offset_y;
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
            if break_index < self.geo.frame.offset_breaks.len() && self.geo.frame.offset_breaks[break_index].0 <= segment_start {
                offset = self.geo.frame.offset_breaks[break_index].1;
                break_index += 1;
            }
        }
    }

    fn semantic_text_units(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticMarkdownUnit> {
        if !self.state.semantic_selection || selection_start >= selection_end {
            return Vec::new();
        }
        let mut list_items = HashSet::new();
        let mut code_blocks = HashSet::new();
        for run in self.geo.doc.text().text_runs() {
            let glyphs = run.glyphs();
            if glyphs.start >= selection_end || glyphs.end <= selection_start || self.geo.doc.nearest_table_ancestor(run.box_idx()).is_some() {
                continue;
            }
            if let Some(block) = self.geo.doc.nearest_code_block_ancestor(run.box_idx()) {
                code_blocks.insert(block);
            } else if let Some(item) = self.geo.doc.nearest_tag_ancestor(run.box_idx(), "li") {
                list_items.insert(item);
            }
        }
        let selected_list_items = list_items.clone();
        list_items.retain(|item| !self.geo.doc.boxes().ancestors(*item).any(|ancestor| selected_list_items.contains(&ancestor)));
        code_blocks.retain(|block| !self.geo.doc.boxes().ancestors(*block).any(|ancestor| list_items.contains(&ancestor)));

        let mut units = Vec::new();
        for item in list_items {
            let ranges = self.geo.doc.box_text_glyph_ranges(item, true);
            if let (Some(start), Some(end)) = (ranges.iter().map(|range| range.start).min(), ranges.iter().map(|range| range.end).max()) {
                units.push(SemanticMarkdownUnit { glyph_start: start, glyph_end: end, glyph_ranges: ranges, markdown: self.geo.doc.list_item_markdown(item, 0) });
            }
        }
        for block in code_blocks {
            let ranges = self.geo.doc.box_text_glyph_ranges(block, false);
            if let (Some(start), Some(end)) = (ranges.iter().map(|range| range.start).min(), ranges.iter().map(|range| range.end).max()) {
                // Code remains character-selectable while both ends of the
                // selection stay inside it. Promote only when the drag crosses
                // into or out of the block.
                if selection_start >= start && selection_end <= end {
                    continue;
                }
                let markdown = self.geo.doc.code_block_markdown(block, &ranges);
                units.push(SemanticMarkdownUnit { glyph_start: start, glyph_end: end, glyph_ranges: ranges, markdown });
            }
        }
        units.sort_by_key(|unit| unit.glyph_start);
        units
    }

    fn semantic_table_selections(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticTableSelection> {
        if !self.state.semantic_selection || selection_start >= selection_end {
            return Vec::new();
        }
        let boxes = self.geo.doc.boxes();
        let mut selections = Vec::new();
        for table_box in (0..boxes.len()).filter(|&index| boxes.is_table(index)) {
            let Some(table) = boxes.table(table_box) else { continue };
            let cells = table.rows().iter().flat_map(|row| row.cells()).filter_map(|cell| self.geo.doc.cell_glyph_range(table_box, cell).map(|glyphs| (cell, glyphs))).collect::<Vec<_>>();
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
        let mut units = self.semantic_text_units(selection_start, selection_end);
        units.extend(self.semantic_table_selections(selection_start, selection_end).into_iter().filter_map(|selection| {
            table_markdown(&selection.table, Some(selection.range)).map(|markdown| SemanticMarkdownUnit { glyph_start: selection.glyph_start, glyph_end: selection.glyph_end, glyph_ranges: selection.cell_glyph_ranges, markdown })
        }));
        units.sort_by_key(|unit| unit.glyph_start);
        if units.is_empty() {
            return self.geo.doc.build_selection_text(selection_start, selection_end);
        }
        let plain = self.geo.doc.build_selection_text(selection_start, selection_end).0;
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
        for unit in units {
            let ordinary_end = unit.glyph_start.min(selection_end);
            if cursor < ordinary_end {
                let (_, segment_markdown) = self.geo.doc.build_selection_text(cursor, ordinary_end);
                if let Some(value) = segment_markdown.as_deref() {
                    append(&mut markdown, value);
                }
            }
            append(&mut markdown, &unit.markdown);
            cursor = cursor.max(unit.glyph_end);
        }
        if cursor < selection_end {
            let (_, segment_markdown) = self.geo.doc.build_selection_text(cursor, selection_end);
            if let Some(value) = segment_markdown.as_deref() {
                append(&mut markdown, value);
            }
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
        let markdown = table_markdown(&table, None)?;
        Some(markdown)
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> Option<String> {
        let hit = self.table_hit(position)?;
        let table = self.geo.doc.boxes().table(hit.table_box)?;
        let html = table_unstyled_html(&table)?;
        Some(html)
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
        let markdown = table_markdown(&table, Some(range))?;
        Some(markdown)
    }

    pub fn paint_table_selection(&self, painter: &mut impl Painter) {
        if let Some(range) = self.table.range()
            && let Some(table) = self.geo.doc.boxes().table(range.table_box)
        {
            self.paint_table_cell_range(painter, &table, range, true);
        }
        if let Some((start, end)) = self.range() {
            for selection in self.semantic_table_selections(start, end) {
                self.paint_table_cell_range(painter, &selection.table, selection.range, false);
            }
        }
    }

    pub fn paint_table_cell_range(&self, painter: &mut impl Painter, table: &RenderTable, range: TableCellRange, bordered: bool) {
        let fill = Color::rgba8(72, 133, 237, 54);
        let border = Color::rgba8(45, 102, 201, 210);
        for row in table.rows() {
            for cell in row.cells() {
                if !cell_intersects(cell, range) {
                    continue;
                }
                self.for_each_projected_box_rect(cell.box_idx(), |rect| {
                    painter.fill_rect(rect, fill);
                    if bordered {
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
    use super::{TableCellRange, table_markdown, table_plain_text, table_unstyled_html};
    use html::layout::LayoutConstraints;
    use html::pipeline::DocumentFactory;
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
    }

    impl GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
        }

        fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, html::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = glyph_metrics.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
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
        assert_eq!(table_markdown(&table, None).as_deref(), Some("| A \\| B |  |\n| --- | --- |\n| One | Two & more |\n|  | Three |"));
        assert_eq!(table_plain_text(&table, None).as_deref(), Some("A | B\t\nOne\tTwo & more\n\tThree"));
        assert_eq!(table_unstyled_html(&table).as_deref(), Some("<table><tr><th colspan=\"2\">A | B</th></tr><tr><td rowspan=\"2\">One</td><td>Two &amp; more</td></tr><tr><td>Three</td></tr></table>"));
        assert!(table.authored_html().contains("class=\"ledger\""));
        assert!(table.authored_html().contains("style=\"color:red\""));
        assert!(!table.authored_html().contains("onclick"));

        let range = TableCellRange { table_box: table.box_idx(), row_start: 1, row_end: 2, column_start: 1, column_end: 1 };
        assert_eq!(table_markdown(&table, Some(range)).as_deref(), Some("|  |\n| --- |\n| Two & more |\n| Three |"));
    }
}
