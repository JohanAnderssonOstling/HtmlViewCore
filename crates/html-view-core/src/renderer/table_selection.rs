use html::layout::{RenderTable, RenderTableCell};
use kurbo::{Point, Rect};
use peniko::Color;
use std::collections::HashSet;
use std::ops::Range;

use crate::{Painter, RendererCore, TableCellRange};

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
struct TableHit {
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

impl RendererCore {
    fn for_each_projected_box_rect(&self, box_idx: usize, mut visit: impl FnMut(Rect)) {
        let boxes = self.document.render_view().boxes();
        let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else { return };
        let rect = Rect::new(point.x, point.y, point.x + size.width, point.y + size.height);
        if rect.width() <= 0.0 || rect.height() <= 0.0 || self.layout.size.height <= 0.0 {
            return;
        }
        let mut segment_start = rect.y0;
        let mut break_index = 0;
        let mut offset = 0.0;
        while break_index < self.frame.offset_breaks.len() && self.frame.offset_breaks[break_index].0 <= segment_start {
            offset = self.frame.offset_breaks[break_index].1;
            break_index += 1;
        }
        while segment_start < rect.y1 {
            let next_break = self.frame.offset_breaks.get(break_index).map(|entry| entry.0).unwrap_or(rect.y1);
            let segment_end = rect.y1.min(next_break);
            let page_y0 = segment_start + offset - self.frame.paint_start_offset_y;
            let page_y1 = segment_end + offset - self.frame.paint_start_offset_y;
            let height = self.layout.size.height;
            let first_column = (page_y0 / height).floor().max(0.0) as i32;
            let last_column = ((page_y1 - 0.0001) / height).floor().min(self.layout.col_count - 1.0) as i32;
            for column in first_column..=last_column {
                let column_top = f64::from(column) * height;
                let y0 = page_y0.max(column_top);
                let y1 = page_y1.min(column_top + height);
                if y1 > y0 {
                    let x0 = self.col_x(f64::from(column), rect.x0);
                    visit(Rect::new(x0, y0 - column_top, x0 + rect.width(), y1 - column_top));
                }
            }
            segment_start = segment_end;
            if break_index < self.frame.offset_breaks.len() && self.frame.offset_breaks[break_index].0 <= segment_start {
                offset = self.frame.offset_breaks[break_index].1;
                break_index += 1;
            }
        }
    }

    fn box_descends_from(&self, mut box_idx: usize, ancestor: usize) -> bool {
        let boxes = self.document.render_view().boxes();
        loop {
            if box_idx == ancestor {
                return true;
            }
            let Some(parent) = boxes.parent(box_idx) else { return false };
            box_idx = parent;
        }
    }

    fn nearest_table_ancestor(&self, mut box_idx: usize) -> Option<usize> {
        let boxes = self.document.render_view().boxes();
        loop {
            if boxes.is_table(box_idx) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    fn nearest_tag_ancestor(&self, mut box_idx: usize, wanted: &str) -> Option<usize> {
        let boxes = self.document.render_view().boxes();
        loop {
            if boxes.tag(box_idx).is_some_and(|tag| tag.eq_ignore_ascii_case(wanted)) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    fn nearest_code_block_ancestor(&self, mut box_idx: usize) -> Option<usize> {
        let boxes = self.document.render_view().boxes();
        loop {
            let tag = boxes.tag(box_idx);
            if tag.is_some_and(|tag| tag.eq_ignore_ascii_case("pre") || (tag.eq_ignore_ascii_case("code") && boxes.is_block_container(box_idx))) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    fn box_text_glyph_ranges(&self, box_idx: usize, include_marker: bool) -> Vec<Range<u32>> {
        let text = self.document.render_view().text();
        let mut ranges = text.text_runs().filter(|run| self.box_descends_from(run.box_idx(), box_idx)).map(|run| run.glyphs()).collect::<Vec<_>>();
        if include_marker && let Some(marker) = self.document.render_view().boxes().list_marker(box_idx) {
            ranges.extend(text.marker_runs().only_box(marker.marker_box()).map(|run| run.glyphs()));
        }
        ranges.sort_by_key(|range| range.start);
        ranges
    }

    fn glyph_text(&self, ranges: &[Range<u32>]) -> String {
        let text = self.document.render_view().text();
        let mut output = String::new();
        for range in ranges {
            for index in range.clone() {
                if let Some(metric) = text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)) {
                    output.push(metric.ch());
                }
            }
        }
        output
    }

    fn list_marker_markdown(&self, list_item: usize) -> String {
        let text = self.document.render_view().text();
        let marker = self.document.render_view().boxes().list_marker(list_item).map(|marker| {
            text.marker_runs().only_box(marker.marker_box()).flat_map(|run| run.glyphs()).filter_map(|index| text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>()
        });
        let fallback = if self.document.render_view().boxes().ancestors(list_item).any(|ancestor| self.document.render_view().boxes().tag(ancestor).is_some_and(|tag| tag.eq_ignore_ascii_case("ol"))) { "1." } else { "-" };
        let marker = marker.as_deref().unwrap_or(fallback).trim();
        if marker.chars().next().is_some_and(|character| character.is_alphanumeric()) && (marker.ends_with('.') || marker.ends_with(')')) { marker.to_owned() } else { "-".to_owned() }
    }

    fn direct_list_item_text_ranges(&self, list_item: usize) -> Vec<Range<u32>> {
        self.document.render_view().text().text_runs().filter(|run| self.box_descends_from(run.box_idx(), list_item) && self.nearest_tag_ancestor(run.box_idx(), "li") == Some(list_item)).map(|run| run.glyphs()).collect()
    }

    fn list_item_markdown(&self, list_item: usize, depth: usize) -> String {
        let own_ranges = self.direct_list_item_text_ranges(list_item);
        let mut body = String::new();
        for range in own_ranges {
            if let Some(segment) = self.build_selection_text(range.start, range.end).1 {
                body.push_str(&segment);
            }
        }
        let indent = "    ".repeat(depth);
        let continuation = format!("\n{indent}    ");
        let mut output = format!("{indent}{} {}", self.list_marker_markdown(list_item), body.trim_end().replace('\n', &continuation));

        let boxes = self.document.render_view().boxes();
        for child in (0..boxes.len()).filter(|&candidate| boxes.tag(candidate).is_some_and(|tag| tag.eq_ignore_ascii_case("li")) && self.nearest_tag_ancestor(boxes.parent(candidate).unwrap_or(candidate), "li") == Some(list_item)) {
            output.push('\n');
            output.push_str(&self.list_item_markdown(child, depth + 1));
        }
        output
    }

    fn code_block_language(&self, block: usize) -> Option<String> {
        let boxes = self.document.render_view().boxes();
        let language_from_class = |class: &str| class.split_ascii_whitespace().find_map(|token| token.strip_prefix("language-").or_else(|| token.strip_prefix("lang-")).filter(|language| !language.is_empty()).map(str::to_owned));
        if let Some(language) = boxes.attribute(block, "class").and_then(language_from_class) {
            return Some(language);
        }
        if let Some(language) = boxes.attribute(block, "data-language").filter(|language| !language.is_empty()) {
            return Some(language.to_owned());
        }
        for candidate in 0..boxes.len() {
            if !self.box_descends_from(candidate, block) || !boxes.tag(candidate).is_some_and(|tag| tag.eq_ignore_ascii_case("code")) {
                continue;
            }
            if let Some(language) = boxes.attribute(candidate, "class").and_then(language_from_class) {
                return Some(language);
            }
            if let Some(language) = boxes.attribute(candidate, "data-language").filter(|language| !language.is_empty()) {
                return Some(language.to_owned());
            }
        }
        None
    }

    fn code_block_markdown(&self, block: usize, ranges: &[Range<u32>]) -> String {
        let source = self.glyph_text(ranges);
        let longest_backticks = source.split(|character| character != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(3.max(longest_backticks + 1));
        let language = self.code_block_language(block).unwrap_or_default();
        let trailing_newline = if source.ends_with('\n') { "" } else { "\n" };
        format!("{fence}{language}\n{source}{trailing_newline}{fence}")
    }

    fn semantic_text_units(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticMarkdownUnit> {
        if !self.selection.semantic_selection || selection_start >= selection_end {
            return Vec::new();
        }
        let mut list_items = HashSet::new();
        let mut code_blocks = HashSet::new();
        for run in self.document.render_view().text().text_runs() {
            let glyphs = run.glyphs();
            if glyphs.start >= selection_end || glyphs.end <= selection_start || self.nearest_table_ancestor(run.box_idx()).is_some() {
                continue;
            }
            if let Some(block) = self.nearest_code_block_ancestor(run.box_idx()) {
                code_blocks.insert(block);
            } else if let Some(item) = self.nearest_tag_ancestor(run.box_idx(), "li") {
                list_items.insert(item);
            }
        }
        let selected_list_items = list_items.clone();
        list_items.retain(|item| !self.document.render_view().boxes().ancestors(*item).any(|ancestor| selected_list_items.contains(&ancestor)));
        code_blocks.retain(|block| !self.document.render_view().boxes().ancestors(*block).any(|ancestor| list_items.contains(&ancestor)));

        let mut units = Vec::new();
        for item in list_items {
            let ranges = self.box_text_glyph_ranges(item, true);
            if let (Some(start), Some(end)) = (ranges.iter().map(|range| range.start).min(), ranges.iter().map(|range| range.end).max()) {
                units.push(SemanticMarkdownUnit { glyph_start: start, glyph_end: end, glyph_ranges: ranges, markdown: self.list_item_markdown(item, 0) });
            }
        }
        for block in code_blocks {
            let ranges = self.box_text_glyph_ranges(block, false);
            if let (Some(start), Some(end)) = (ranges.iter().map(|range| range.start).min(), ranges.iter().map(|range| range.end).max()) {
                // Code remains character-selectable while both ends of the
                // selection stay inside it. Promote only when the drag crosses
                // into or out of the block.
                if selection_start >= start && selection_end <= end {
                    continue;
                }
                let markdown = self.code_block_markdown(block, &ranges);
                units.push(SemanticMarkdownUnit { glyph_start: start, glyph_end: end, glyph_ranges: ranges, markdown });
            }
        }
        units.sort_by_key(|unit| unit.glyph_start);
        units
    }

    fn cell_glyph_range(&self, table_box: usize, cell: &RenderTableCell) -> Option<std::ops::Range<u32>> {
        let mut start = u32::MAX;
        let mut end = 0;
        for run in self.document.render_view().text().text_runs() {
            if self.box_descends_from(run.box_idx(), cell.box_idx()) && self.nearest_table_ancestor(run.box_idx()) == Some(table_box) {
                start = start.min(run.glyphs().start);
                end = end.max(run.glyphs().end);
            }
        }
        (start < end).then_some(start..end)
    }

    fn semantic_table_selections(&self, selection_start: u32, selection_end: u32) -> Vec<SemanticTableSelection> {
        if !self.selection.semantic_selection || selection_start >= selection_end {
            return Vec::new();
        }
        let boxes = self.document.render_view().boxes();
        let mut selections = Vec::new();
        for table_box in (0..boxes.len()).filter(|&index| boxes.is_table(index)) {
            let Some(table) = boxes.table(table_box) else { continue };
            let cells = table.rows().iter().flat_map(|row| row.cells()).filter_map(|cell| self.cell_glyph_range(table_box, cell).map(|glyphs| (cell, glyphs))).collect::<Vec<_>>();
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

    pub(crate) fn semantic_cell_glyph_ranges(&self, selection_start: u32, selection_end: u32) -> Vec<std::ops::Range<u32>> {
        self.semantic_table_selections(selection_start, selection_end).into_iter().flat_map(|selection| selection.cell_glyph_ranges).collect()
    }

    pub(crate) fn semantic_promoted_glyph_ranges(&self, selection_start: u32, selection_end: u32) -> Vec<Range<u32>> {
        self.semantic_text_units(selection_start, selection_end).into_iter().flat_map(|unit| unit.glyph_ranges).collect()
    }

    pub(crate) fn semantic_selection_contains_point(&self, position: Point, selection_start: u32, selection_end: u32) -> bool {
        self.semantic_table_selections(selection_start, selection_end).into_iter().any(|selection| {
            selection.table.rows().iter().flat_map(|row| row.cells()).filter(|cell| cell_intersects(cell, selection.range)).any(|cell| {
                let mut contains = false;
                self.for_each_projected_box_rect(cell.box_idx(), |rect| contains |= rect.contains(position));
                contains
            })
        })
    }

    pub(crate) fn build_semantic_selection_text(&self, selection_start: u32, selection_end: u32) -> (Option<String>, Option<String>) {
        let mut units = self.semantic_text_units(selection_start, selection_end);
        units.extend(self.semantic_table_selections(selection_start, selection_end).into_iter().filter_map(|selection| {
            table_markdown(&selection.table, Some(selection.range)).map(|markdown| SemanticMarkdownUnit { glyph_start: selection.glyph_start, glyph_end: selection.glyph_end, glyph_ranges: selection.cell_glyph_ranges, markdown })
        }));
        units.sort_by_key(|unit| unit.glyph_start);
        if units.is_empty() {
            return self.build_selection_text(selection_start, selection_end);
        }
        let plain = self.build_selection_text(selection_start, selection_end).0;
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
                let (_, segment_markdown) = self.build_selection_text(cursor, ordinary_end);
                if let Some(value) = segment_markdown.as_deref() {
                    append(&mut markdown, value);
                }
            }
            append(&mut markdown, &unit.markdown);
            cursor = cursor.max(unit.glyph_end);
        }
        if cursor < selection_end {
            let (_, segment_markdown) = self.build_selection_text(cursor, selection_end);
            if let Some(value) = segment_markdown.as_deref() {
                append(&mut markdown, value);
            }
        }
        (plain, (!markdown.is_empty()).then_some(markdown))
    }

    fn table_hit(&self, position: Point) -> Option<TableHit> {
        let boxes = self.document.render_view().boxes();
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
        self.table_hit(position).is_some_and(|hit| self.table_selection.range.is_some_and(|range| range.table_box == hit.table_box))
    }

    pub(crate) fn table_selection_active(&self) -> bool {
        self.table_selection.range.is_some()
    }

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        let Some(hit) = self.table_hit(position) else { return false };
        self.table_selection.anchor = Some((hit.table_box, hit.row, hit.row_end, hit.column, hit.column_end));
        self.table_selection.range = Some(TableCellRange { table_box: hit.table_box, row_start: hit.row, row_end: hit.row_end, column_start: hit.column, column_end: hit.column_end });
        self.table_selection.dragging = false;
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn begin_table_selection_drag(&mut self, position: Point) -> bool {
        let Some(current) = self.table_selection.range else {
            return false;
        };
        let Some(hit) = self.table_hit(position) else { return false };
        if hit.table_box != current.table_box {
            return false;
        }
        self.table_selection.anchor = Some((hit.table_box, hit.row, hit.row_end, hit.column, hit.column_end));
        self.table_selection.range = Some(TableCellRange { table_box: hit.table_box, row_start: hit.row, row_end: hit.row_end, column_start: hit.column, column_end: hit.column_end });
        self.table_selection.dragging = true;
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn update_table_selection_drag(&mut self, position: Point) -> bool {
        if !self.table_selection.dragging {
            return false;
        }
        let Some(hit) = self.table_hit(position) else { return true };
        let Some((table_box, anchor_row, anchor_row_end, anchor_column, anchor_column_end)) = self.table_selection.anchor else { return true };
        if hit.table_box != table_box {
            return true;
        }
        let next = TableCellRange { table_box, row_start: anchor_row.min(hit.row), row_end: anchor_row_end.max(hit.row_end), column_start: anchor_column.min(hit.column), column_end: anchor_column_end.max(hit.column_end) };
        if self.table_selection.range != Some(next) {
            self.table_selection.range = Some(next);
            self.request_overlay_repaint();
        }
        true
    }

    pub(crate) fn finish_table_selection_drag(&mut self) -> bool {
        let was_dragging = self.table_selection.dragging;
        self.table_selection.dragging = false;
        was_dragging
    }

    pub fn clear_table_selection(&mut self) -> bool {
        if self.table_selection.range.take().is_none() {
            return false;
        }
        self.table_selection.anchor = None;
        self.table_selection.dragging = false;
        self.request_overlay_repaint();
        true
    }

    pub fn copy_table_at(&self, position: Point) -> Result<bool, String> {
        let Some(hit) = self.table_hit(position) else { return Ok(false) };
        let Some(table) = self.document.render_view().boxes().table(hit.table_box) else { return Ok(false) };
        let Some(markdown) = table_markdown(&table, None) else { return Ok(false) };
        self.host.set_clipboard(&markdown)?;
        Ok(true)
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> Result<bool, String> {
        let Some(hit) = self.table_hit(position) else { return Ok(false) };
        let Some(table) = self.document.render_view().boxes().table(hit.table_box) else { return Ok(false) };
        let Some(html) = table_unstyled_html(&table) else { return Ok(false) };
        self.host.set_clipboard(&html)?;
        Ok(true)
    }

    pub fn copy_table_styled_html_at(&self, position: Point) -> Result<bool, String> {
        let Some(hit) = self.table_hit(position) else { return Ok(false) };
        let Some(table) = self.document.render_view().boxes().table(hit.table_box) else { return Ok(false) };
        if table.authored_html().is_empty() {
            return Ok(false);
        }
        self.host.set_clipboard(table.authored_html())?;
        Ok(true)
    }

    pub fn copy_table_selection(&self) -> Result<bool, String> {
        let Some(range) = self.table_selection.range else { return Ok(false) };
        let Some(table) = self.document.render_view().boxes().table(range.table_box) else { return Ok(false) };
        let Some(markdown) = table_markdown(&table, Some(range)) else { return Ok(false) };
        self.host.set_clipboard(&markdown)?;
        Ok(true)
    }

    pub(crate) fn paint_table_selection(&self, painter: &mut impl Painter) {
        if let Some(range) = self.table_selection.range
            && let Some(table) = self.document.render_view().boxes().table(range.table_box)
        {
            self.paint_table_cell_range(painter, &table, range, true);
        }
        if let Some((start, end)) = self.selection_range() {
            for selection in self.semantic_table_selections(start, end) {
                self.paint_table_cell_range(painter, &selection.table, selection.range, false);
            }
        }
    }

    fn paint_table_cell_range(&self, painter: &mut impl Painter, table: &RenderTable, range: TableCellRange, bordered: bool) {
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
    use crate::layout::LayoutConstraints;
    use crate::parser::DocumentFactory;
    use crate::text_backend::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
    }

    impl crate::GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
        }

        fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, crate::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(crate::layout::ShapeError::rejected_metric)?;
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
