use std::time::Duration;

use kurbo::{Point, Rect};
use peniko::Color;

use crate::doc_query::DocQuery;
use crate::{LineScreen, PagePositions, PaintDirection, Painter, RenderState, RendererCore, RepeatedTableHeader, VisibleLinePositions};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum TablePaginationEventKind {
    WholeTable,
    RowGroup,
}

#[derive(Clone, Copy, Debug)]
struct TablePaginationEvent {
    trigger_line: usize,
    top: f64,
    bottom: f64,
    kind: TablePaginationEventKind,
    forced: bool,
    repeated_header: Option<TableHeaderSpec>,
}

#[derive(Clone, Copy, Debug)]
struct TableHeaderSpec {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
}

#[derive(Clone, Copy, Debug)]
struct ForcedBreakEvent {
    trigger_line: usize,
    top: f64,
    kind: html::layout::RenderForcedBreak,
}

#[derive(Clone, Copy, Debug)]
struct KeepRangeEvent {
    trigger_line: usize,
    top: f64,
    bottom: f64,
}

pub(super) struct BoundaryAnchors {
    first_line: Vec<Option<usize>>,
    next_line: Vec<Option<usize>>,
    pub(super) subtree_end: Vec<usize>,
}

impl BoundaryAnchors {
    pub(super) fn before(&self, box_idx: usize) -> Option<usize> {
        self.first_line.get(box_idx).copied().flatten().or_else(|| self.next_line.get(box_idx).copied().flatten())
    }

    pub(super) fn after(&self, box_idx: usize) -> Option<usize> {
        self.next_line.get(box_idx).copied().flatten()
    }
}

const EPUB_NAMESPACE: &str = "http://www.idpf.org/2007/ops";

fn has_token(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| value.split_ascii_whitespace().any(|token| token.eq_ignore_ascii_case(expected)))
}

fn has_epub_type(boxes: html::layout::RenderBoxView<'_>, box_idx: usize, expected: &str) -> bool {
    has_token(boxes.attribute(box_idx, "epub:type").or_else(|| boxes.attribute_expanded(box_idx, Some(EPUB_NAMESPACE), "type")), expected)
}

fn is_semantic_sidebar(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    let is_note = ["footnote", "endnote", "rearnote"].iter().any(|kind| has_epub_type(boxes, box_idx, kind)) || ["doc-footnote", "doc-endnote"].iter().any(|role| has_token(boxes.attribute(box_idx, "role"), role));
    !is_note && (has_epub_type(boxes, box_idx, "sidebar") || has_token(boxes.attribute(box_idx, "role"), "complementary") || boxes.tag(box_idx).is_some_and(|tag| tag.eq_ignore_ascii_case("aside")))
}

fn is_semantic_page_start(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    const EPUB_TYPES: &[&str] = &[
        "chapter",
        "part",
        "appendix",
        "prologue",
        "epilogue",
        "foreword",
        "preface",
        "introduction",
        "conclusion",
        "acknowledgments",
        "afterword",
        "bibliography",
        "glossary",
        "index",
        "colophon",
        "titlepage",
        "halftitlepage",
        "copyright-page",
        "frontmatter",
        "bodymatter",
        "backmatter",
    ];
    const DPUB_ROLES: &[&str] = &[
        "doc-chapter",
        "doc-part",
        "doc-appendix",
        "doc-prologue",
        "doc-epilogue",
        "doc-foreword",
        "doc-preface",
        "doc-introduction",
        "doc-conclusion",
        "doc-acknowledgments",
        "doc-afterword",
        "doc-bibliography",
        "doc-glossary",
        "doc-index",
        "doc-colophon",
    ];

    EPUB_TYPES.iter().any(|kind| has_epub_type(boxes, box_idx, kind)) || DPUB_ROLES.iter().any(|role| has_token(boxes.attribute(box_idx, "role"), role))
}

fn line_paint_origin(point: Point) -> Point {
    point
}

fn project_inline_decoration_rect(rect: Rect, logical_line_point: Point, screen_point: Point, extra_x_offset: f64, optical_offset_x: f64) -> Rect {
    let x = screen_point.x + extra_x_offset + optical_offset_x + (rect.x0 - logical_line_point.x);
    let y = screen_point.y + (rect.y0 - logical_line_point.y);
    Rect::new(x, y, x + rect.width(), y + rect.height())
}

/// Compute scaled image dimensions to fit column width
fn scaled_image_size(frag: &html::layout::RenderImageFragment) -> (f64, f64) {
    let size = frag.size();
    (size.width, size.height)
}

impl<'a> DocQuery<'a> {
    pub(crate) fn effective_line_height(self, line_idx: usize, fallback: f64) -> f64 {
        if line_idx >= self.text().line_count() {
            return fallback;
        }
        let mut max_height = 0.0;
        for frag in self.view().fragments().images_for_line(line_idx).iter() {
            let (_, height) = scaled_image_size(&frag);
            if height > max_height {
                max_height = height;
            }
        }
        if max_height > 0.0 { max_height } else { fallback }
    }

    pub(crate) fn find_line_index_for_decoration(self, y0: f64, y1: f64) -> Option<usize> {
        // find first line overlapping a decoration rect
        let lines = self.text().lines();
        if lines.is_empty() {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = lines.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() < y0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for idx in lo..lines.len() {
            let line = lines.get(idx).expect("line index must be within line count");
            let ly0 = line.point().y;
            let ly1 = line.point().y + line.height();
            if ly0 > y1 {
                break;
            }
            // Inline top and bottom borders sit immediately outside the line
            // box and therefore only touch its edge. Keep that owning line so
            // pagination can project the decoration into the correct column.
            if ly1 >= y0 && ly0 <= y1 {
                return Some(idx);
            }
        }
        None
    }

    pub(crate) fn first_line_in_vertical_range(self, top: f64, bottom: f64) -> Option<usize> {
        let lines = self.text().lines();
        // Bottom captions can occur before their rows in source/line order
        // while being below them geometrically, so this cannot be a binary
        // search over line Y coordinates.
        lines.iter().position(|line| line.point().y >= top - 0.01 && line.point().y < bottom - 0.01)
    }

    pub(crate) fn pagination_boundary_anchors(self) -> BoundaryAnchors {
        let root = self.view();
        let boxes = root.boxes();
        let text = root.text();
        let mut first_line: Vec<Option<usize>> = vec![None; boxes.len()];
        for run in text.text_runs().chain(text.marker_runs()) {
            let first = run.glyphs().find_map(|glyph_idx| text.line_index_for_glyph(glyph_idx));
            if let Some(slot) = first_line.get_mut(run.box_idx()) {
                *slot = match (*slot, first) {
                    (Some(current), Some(candidate)) => Some(current.min(candidate)),
                    (None, candidate) => candidate,
                    (current, None) => current,
                };
            }
        }

        // Replaced content participates in a line but has no text glyph range.
        // Use its positive box geometry only as a fallback; zero-height empty
        // boxes must anchor to later content instead of stealing a coincident line.
        for box_idx in 0..boxes.len() {
            let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
                continue;
            };
            if size.height <= 0.01 || first_line[box_idx].is_some() {
                continue;
            }
            let bottom = point.y + size.height;
            first_line[box_idx] = self.first_line_in_vertical_range(point.y, bottom);
        }

        let mut subtree_end = (0..boxes.len()).collect::<Vec<_>>();
        for box_idx in (0..boxes.len()).rev() {
            if let Some(parent) = boxes.parent(box_idx) {
                subtree_end[parent] = subtree_end[parent].max(subtree_end[box_idx]);
                first_line[parent] = match (first_line[parent], first_line[box_idx]) {
                    (Some(current), Some(candidate)) => Some(current.min(candidate)),
                    (None, candidate) => candidate,
                    (current, None) => current,
                };
            }
        }

        let mut next_from_index = vec![None; boxes.len() + 1];
        let mut next = None;
        for box_idx in (0..boxes.len()).rev() {
            next_from_index[box_idx + 1] = next;
            if first_line[box_idx].is_some() {
                next = first_line[box_idx];
            }
        }
        let next_line = subtree_end.iter().map(|end| next_from_index.get(end + 1).copied().flatten()).collect();
        BoundaryAnchors { first_line, next_line, subtree_end }
    }

    pub(crate) fn start_line_at(self, offset_y: f64, epsilon: f64) -> usize {
        let lines = self.text().lines();
        let mut lo = 0;
        let mut hi = lines.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() <= offset_y + epsilon {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub(crate) fn end_line_at(self, offset_y: f64, epsilon: f64) -> Option<usize> {
        let lines = self.text().lines();
        let Some(first_line) = lines.first() else {
            return None;
        };
        if first_line.point().y + first_line.height() > offset_y + epsilon {
            return None;
        }
        let mut lo = 0;
        let mut hi = lines.len() - 1;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() <= offset_y + epsilon {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Some(lo)
    }

}

impl RendererCore {
    fn table_pagination_events(&self) -> Vec<TablePaginationEvent> {
        if !self.pipeline_inputs.layout.text_composition_policy.is_book_optimized() {
            return Vec::new();
        }

        let root = self.document.render_view();
        let boxes = root.boxes();
        let table_indices = (0..boxes.len()).filter(|&box_idx| boxes.is_table(box_idx)).collect::<Vec<_>>();
        let mut rows_by_table = table_indices.iter().copied().map(|table_idx| (table_idx, Vec::new())).collect::<Vec<_>>();
        let mut row_spans = vec![1usize; boxes.len()];
        let mut row_cell_counts = vec![0usize; boxes.len()];
        let mut row_header_cell_counts = vec![0usize; boxes.len()];
        for box_idx in 0..boxes.len() {
            if boxes.is_table_row(box_idx)
                && let Some(table_idx) = boxes.ancestors(box_idx).find(|&ancestor| boxes.is_table(ancestor))
                && let Ok(table_slot) = table_indices.binary_search(&table_idx)
            {
                rows_by_table[table_slot].1.push(box_idx);
            } else if boxes.is_table_cell(box_idx)
                && let Some(row_idx) = boxes.parent(box_idx)
                && let Some(row_span) = row_spans.get_mut(row_idx)
            {
                *row_span = (*row_span).max(boxes.table_cell_rowspan(box_idx));
                row_cell_counts[row_idx] += 1;
                if boxes.tag(box_idx).is_some_and(|tag| tag.eq_ignore_ascii_case("th")) {
                    row_header_cell_counts[row_idx] += 1;
                }
            }
        }

        let mut events = Vec::new();
        for (table_idx, mut rows) in rows_by_table {
            let (Some(table_point), Some(table_size)) = (boxes.point(table_idx), boxes.size(table_idx)) else {
                continue;
            };
            let table_bottom = table_point.y + table_size.height;
            if let Some(trigger_line) = self.doc().first_line_in_vertical_range(table_point.y, table_bottom) {
                events.push(TablePaginationEvent { trigger_line, top: table_point.y, bottom: table_bottom, kind: TablePaginationEventKind::WholeTable, forced: false, repeated_header: None });
            }

            rows.sort_by(|&a, &b| {
                let ay = boxes.point(a).map_or(0.0, |point| point.y);
                let by = boxes.point(b).map_or(0.0, |point| point.y);
                ay.total_cmp(&by).then_with(|| a.cmp(&b))
            });

            // EPUBs commonly omit <thead> and use a leading row made entirely
            // of <th> cells. Honor explicit CSS/HTML header groups first, then
            // use that conservative structural fallback.
            let mut header_row_count = rows.iter().take_while(|&&row| boxes.ancestors(row).take_while(|&ancestor| ancestor != table_idx).any(|ancestor| boxes.is_table_header_group(ancestor))).count();
            if header_row_count == 0 {
                header_row_count = rows.iter().take_while(|&&row| row_cell_counts[row] > 0 && row_cell_counts[row] == row_header_cell_counts[row]).count();
            }
            let repeated_header = (header_row_count > 0).then(|| rows.first().zip(rows.get(header_row_count - 1))).flatten().and_then(|(&first, &last)| {
                let top = boxes.point(first)?.y;
                let bottom = boxes.point(last)?.y + boxes.size(last)?.height;
                (bottom > top + 0.01).then_some(TableHeaderSpec { left: table_point.x, right: table_point.x + table_size.width, top, bottom })
            });

            let mut group_start = 0usize;
            while group_start < rows.len() {
                let mut group_end = group_start + 1;
                let mut scan = group_start;
                while scan < group_end {
                    group_end = group_end.max(scan.saturating_add(row_spans[rows[scan]])).min(rows.len());
                    scan += 1;
                }

                let first_row = rows[group_start];
                let last_row = rows[group_end - 1];
                let Some(group_top) = boxes.point(first_row).map(|point| point.y) else {
                    group_start = group_end;
                    continue;
                };
                let mut group_bottom = boxes.point(last_row).zip(boxes.size(last_row)).map_or(group_top, |(point, size)| point.y + size.height);
                if group_end == rows.len() {
                    // A bottom caption is part of the table box but not of the
                    // last row. Include it in the final atomic group.
                    group_bottom = group_bottom.max(table_bottom);
                }
                let forced = boxes.forces_break_before(first_row) || (group_start > 0 && boxes.forces_break_after(rows[group_start - 1]));
                if let Some(trigger_line) = self.doc().first_line_in_vertical_range(group_top, group_bottom) {
                    events.push(TablePaginationEvent {
                        trigger_line,
                        top: group_top,
                        bottom: group_bottom,
                        kind: TablePaginationEventKind::RowGroup,
                        forced,
                        repeated_header: repeated_header.filter(|header| group_top >= header.bottom - 0.01),
                    });
                }
                group_start = group_end;
            }
        }
        events.sort_by_key(|event| (event.trigger_line, event.kind));
        events
    }

    fn keep_range_event(&self, trigger_line: usize, top: f64, bottom: f64) -> Option<KeepRangeEvent> {
        (bottom > top + 0.01).then_some(KeepRangeEvent { trigger_line, top, bottom })
    }

    fn automatic_keep_events(&self, anchors: &BoundaryAnchors) -> Vec<KeepRangeEvent> {
        if !self.pipeline_inputs.layout.text_composition_policy.is_book_optimized() {
            return Vec::new();
        }
        let root = self.document.render_view();
        let boxes = root.boxes();
        let lines = root.text().lines();
        let mut lines_by_y = lines.iter().enumerate().map(|(line_idx, line)| (line.point().y, line_idx)).collect::<Vec<_>>();
        lines_by_y.sort_by(|left, right| left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let lines_in_range = |top: f64, bottom: f64| {
            let start = lines_by_y.partition_point(|(y, _)| *y < top - 0.01);
            let end = lines_by_y.partition_point(|(y, _)| *y < bottom - 0.01);
            let mut indices = lines_by_y[start..end].iter().map(|(_, line_idx)| *line_idx).collect::<Vec<_>>();
            indices.sort_unstable();
            indices
        };
        let mut events = Vec::new();
        let mut children = vec![Vec::new(); boxes.len() + 1];
        for box_idx in 0..boxes.len() {
            let parent_slot = boxes.parent(box_idx).unwrap_or(boxes.len());
            children[parent_slot].push(box_idx);
        }

        for box_idx in 0..boxes.len() {
            let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
                continue;
            };
            let bottom = point.y + size.height;
            let tag = boxes.tag(box_idx);
            let block_lines = if boxes.is_block_container(box_idx) { lines_in_range(point.y, bottom) } else { Vec::new() };
            let has_forced_inside = (box_idx + 1..=anchors.subtree_end[box_idx]).any(|descendant| boxes.forces_break_before(descendant) || boxes.forces_break_after(descendant));
            if (boxes.avoids_break_inside(box_idx) || tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figure")) || is_semantic_sidebar(boxes, box_idx))
                && !has_forced_inside
                && let Some(trigger_line) = anchors.before(box_idx)
                && let Some(event) = self.keep_range_event(trigger_line, point.y, bottom)
            {
                events.push(event);
            }

            let owns_line_flow = boxes.is_block_container(box_idx) && !children[box_idx].iter().any(|&child| boxes.is_block_container(child)) && !boxes.is_table(box_idx) && !boxes.ancestors(box_idx).any(|ancestor| boxes.is_table(ancestor));
            if owns_line_flow && block_lines.len() > 1 {
                for (count, from_end) in [(boxes.orphans(box_idx), false), (boxes.widows(box_idx), true)] {
                    let count = count.min(block_lines.len());
                    if count <= 1 {
                        continue;
                    }
                    let selected = if from_end { &block_lines[block_lines.len() - count..] } else { &block_lines[..count] };
                    let group_top = selected.iter().filter_map(|&line_idx| lines.get(line_idx).map(|line| line.point().y)).fold(f64::INFINITY, f64::min);
                    let group_bottom = selected.iter().filter_map(|&line_idx| lines.get(line_idx).map(|line| line.point().y + line.height())).fold(f64::NEG_INFINITY, f64::max);
                    if let Some(&trigger_line) = selected.iter().min()
                        && let Some(event) = self.keep_range_event(trigger_line, group_top, group_bottom)
                    {
                        events.push(event);
                    }
                }
            }
        }

        events.extend(self.doc().semantic_keep_ranges(anchors).into_iter().filter_map(|(trigger_line, top, bottom)| self.keep_range_event(trigger_line, top, bottom)));

        for siblings in children {
            for pair in siblings.windows(2) {
                let previous = pair[0];
                let next = pair[1];
                if boxes.forces_break_after(previous) || boxes.forces_break_before(next) {
                    continue;
                }
                let (Some(previous_point), Some(previous_size), Some(next_point), Some(next_size)) = (boxes.point(previous), boxes.size(previous), boxes.point(next), boxes.size(next)) else {
                    continue;
                };
                let previous_bottom = previous_point.y + previous_size.height;
                let next_bottom = next_point.y + next_size.height;
                let previous_tag = boxes.tag(previous);
                let next_tag = boxes.tag(next);

                if boxes.avoids_break_after(previous) || boxes.avoids_break_before(next) || previous_tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figcaption")) || next_tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figcaption")) {
                    let group_top = previous_point.y.min(next_point.y);
                    let group_bottom = previous_bottom.max(next_bottom);
                    if let Some(trigger_line) = anchors.before(previous)
                        && let Some(event) = self.keep_range_event(trigger_line, group_top, group_bottom)
                    {
                        events.push(event);
                    }
                } else if previous_tag.is_some_and(|tag| ["h1", "h2", "h3", "h4", "h5", "h6"].iter().any(|heading| tag.eq_ignore_ascii_case(heading))) {
                    let next_lines = lines_in_range(next_point.y, next_bottom);
                    let required_bottom = next_lines.iter().take(2).filter_map(|&line_idx| lines.get(line_idx).map(|line| line.point().y + line.height())).fold(previous_bottom, f64::max);
                    if let Some(trigger_line) = anchors.before(previous)
                        && let Some(event) = self.keep_range_event(trigger_line, previous_point.y, required_bottom)
                    {
                        events.push(event);
                    }
                }
            }
        }

        events.sort_by(|left, right| left.trigger_line.cmp(&right.trigger_line).then_with(|| left.top.total_cmp(&right.top)).then_with(|| right.bottom.total_cmp(&left.bottom)));
        events.dedup_by(|left, right| left.trigger_line == right.trigger_line && (left.top - right.top).abs() <= 0.01 && (left.bottom - right.bottom).abs() <= 0.01);
        events
    }

    fn apply_keep_range_event(&self, event: KeepRangeEvent, start_offset_y: f64, render_state: &mut RenderState) {
        let fragment_height = self.layout.size.height;
        if fragment_height <= 0.0 || event.top < start_offset_y - 0.01 {
            return;
        }
        let height = (event.bottom - event.top).max(0.0);
        if height > fragment_height + 0.01 {
            return;
        }
        let projected_top = event.top + render_state.y_offset - start_offset_y;
        let local_top = projected_top.rem_euclid(fragment_height);
        if local_top > 0.01 && local_top + height > fragment_height + 0.01 {
            render_state.y_offset += fragment_height - local_top;
        }
    }

    fn forced_break_events(&self, anchors: &BoundaryAnchors) -> Vec<ForcedBreakEvent> {
        if !self.pipeline_inputs.layout.text_composition_policy.is_book_optimized() {
            return Vec::new();
        }
        let boxes = self.document.render_view().boxes();
        let lines = self.document.render_view().text().lines();
        let mut events = Vec::new();
        let mut authored_boundary = vec![false; lines.len()];
        for box_idx in 0..boxes.len() {
            let Some(point) = boxes.point(box_idx) else {
                continue;
            };
            if let Some(kind) = boxes.forced_break_before(box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
            {
                authored_boundary[trigger_line] = true;
                events.push(ForcedBreakEvent { trigger_line, top: point.y, kind });
            } else if boxes.avoids_break_before(box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
            {
                authored_boundary[trigger_line] = true;
            }
            if let Some(kind) = boxes.forced_break_after(box_idx)
                && let Some(trigger_line) = anchors.after(box_idx)
            {
                authored_boundary[trigger_line] = true;
                // The next line's document position is the only geometry
                // pagination needs. Margins remain layout concerns and are
                // neither copied into nor interpreted by the paginator.
                events.push(ForcedBreakEvent { trigger_line, top: lines.get(trigger_line).expect("trigger line must exist").point().y, kind });
            } else if boxes.avoids_break_after(box_idx)
                && let Some(trigger_line) = anchors.after(box_idx)
            {
                authored_boundary[trigger_line] = true;
            }
        }
        for box_idx in 0..boxes.len() {
            if is_semantic_page_start(boxes, box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
                && !authored_boundary[trigger_line]
                && let Some(point) = boxes.point(box_idx)
            {
                events.push(ForcedBreakEvent { trigger_line, top: point.y, kind: html::layout::RenderForcedBreak::Page });
            }
        }
        events.sort_by(|left, right| left.trigger_line.cmp(&right.trigger_line).then_with(|| left.top.total_cmp(&right.top)).then_with(|| left.kind.cmp(&right.kind)));
        // Several nested boxes, or an adjoining break-after/break-before pair,
        // can describe the same boundary. Keep one event and let a page break
        // dominate a column break.
        let mut merged: Vec<ForcedBreakEvent> = Vec::with_capacity(events.len());
        for event in events {
            if let Some(previous) = merged.last_mut()
                && previous.trigger_line == event.trigger_line
            {
                if event.kind > previous.kind {
                    previous.kind = event.kind;
                }
                previous.top = previous.top.max(event.top);
            } else {
                merged.push(event);
            }
        }
        merged
    }

    fn apply_forced_break_event(&self, event: ForcedBreakEvent, start_offset_y: f64, previous_line: Option<LineScreen>, render_state: &mut RenderState) {
        let fragment_height = self.layout.size.height;
        if fragment_height <= 0.0 || event.top < start_offset_y - 0.01 {
            return;
        }
        // At the beginning of a newly selected reader page the break has
        // already been satisfied. Otherwise base the destination solely on
        // the last column that contains actual content, never on margin-only
        // document space.
        let target_col = previous_line.map_or(0.0, |previous| match event.kind {
            html::layout::RenderForcedBreak::Column => previous.col_index as f64 + 1.0,
            html::layout::RenderForcedBreak::Page => self.layout.col_count,
        });
        let projected_top = event.top + render_state.y_offset - start_offset_y;
        render_state.y_offset += target_col * fragment_height - projected_top;
    }

    #[cfg(test)]
    pub(crate) fn table_row_group_bounds_containing(&self, y: f64) -> Option<(f64, f64)> {
        self.table_pagination_events().into_iter().find(|event| event.kind == TablePaginationEventKind::RowGroup && event.top <= y + 0.01 && event.bottom > y + 0.01).map(|event| (event.top, event.bottom))
    }

    fn apply_table_pagination_event(&self, event: TablePaginationEvent, start_offset_y: f64, render_state: &mut RenderState) -> Option<RepeatedTableHeader> {
        let fragment_height = self.layout.size.height;
        if fragment_height <= 0.0 || event.top < start_offset_y - 0.01 {
            return None;
        }
        let projected_top = event.top + render_state.y_offset - start_offset_y;
        let local_top = projected_top.rem_euclid(fragment_height);

        let height = (event.bottom - event.top).max(0.0);
        let crosses = local_top + height > fragment_height + 0.01;
        let move_to_fresh_fragment = match event.kind {
            // Moving the whole table is useful only when it can actually fit
            // in a fresh fragment. An intrinsically oversized table should
            // start in the available space; its row-group events below will
            // choose safe boundaries as each row is encountered.
            TablePaginationEventKind::WholeTable => height <= fragment_height && crosses,
            // A forced break that already lands on a fragment boundary is
            // satisfied; advancing again would skip an entire column.
            TablePaginationEventKind::RowGroup => (event.forced && local_top > 0.01) || (height <= fragment_height && crosses),
        };
        if move_to_fresh_fragment {
            render_state.y_offset += fragment_height - local_top;
        }

        let header = event.repeated_header?;
        let header_height = header.bottom - header.top;
        if header_height + height > fragment_height + 0.01 {
            return None;
        }
        let body_projected_top = event.top + render_state.y_offset - start_offset_y;
        let body_local_top = body_projected_top.rem_euclid(fragment_height);
        let starts_continuation = move_to_fresh_fragment || (body_local_top <= 0.01 && (start_offset_y >= header.bottom - 0.01 || body_projected_top >= fragment_height - 0.01));
        if !starts_continuation {
            return None;
        }
        let col_index = (body_projected_top / fragment_height).floor() as i32;
        if col_index < 0 || col_index as f64 >= self.layout.col_count {
            return None;
        }
        render_state.y_offset += header_height;
        Some(RepeatedTableHeader { source_left: header.left, source_right: header.right, source_top: header.top, source_bottom: header.bottom, col_index })
    }

    /// Compute x-coordinate for a column given a local x offset
    #[inline]
    pub(crate) fn col_x(&self, col_index: f64, local_x: f64) -> f64 {
        self.layout.col_gap + col_index * (self.layout.col_width + self.layout.col_gap) + local_x
    }

    fn resolve_point_at(&self, point: Point, elem_height: f64, mut render_state: RenderState, start_offset_y: f64) -> (Option<Point>, RenderState) {
        // map document point to screen columns
        let mut y = point.y + render_state.y_offset - start_offset_y;
        let mut col_index = (y / self.layout.size.height).floor(); // initial column based on vertical position
        y -= col_index * self.layout.size.height;

        if y + elem_height > self.layout.size.height {
            // move to next column if element would overflow
            col_index += 1.0;
            render_state.y_offset += self.layout.size.height - y;
            y = 0.;
        }

        // Past all columns - stop rendering
        if col_index >= self.layout.col_count {
            // stop when past last column
            render_state.should_stop = true;
            return (None, render_state);
        }

        render_state.col_index = col_index;
        let x = self.col_x(col_index, point.x);
        let resolved = Point::new(x, y);
        (Some(resolved), render_state)
    }

    fn initial_render_state() -> RenderState {
        // canonical renderer state for a fresh pass
        RenderState { y_offset: 0.0, col_index: 0.0, should_stop: false }
    }

    fn paint_decorations(
        &self, cx: &mut impl Painter, start_offset_y: f64, breaks: &[(f64, f64)], line_positions: Option<&VisibleLinePositions>, foreground: bool, positioned_layer: bool, negative_positioned_layer: bool, independent_positioned_layer: bool,
    ) {
        // draw background decorations across columns
        let page_start = start_offset_y;
        let page_end = start_offset_y + self.layout.col_count * self.layout.size.height; // visible doc y end for this page
        let inline_metrics = self.inline_metrics();
        let lines = self.document.render_view().text().lines();
        for decoration in self.document.render_view().fragments().decorations().iter() {
            if decoration.is_foreground() != foreground {
                continue;
            }
            if decoration.is_in_positioned_layer() != positioned_layer {
                continue;
            }
            if decoration.is_in_negative_positioned_layer() != negative_positioned_layer {
                continue;
            }
            if decoration.is_in_independent_positioned_layer() != independent_positioned_layer {
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
                let x0 = if clip.clips_x() { rect.x0.max(clip_rect.x0) } else { rect.x0 };
                let y0 = if clip.clips_y() { rect.y0.max(clip_rect.y0) } else { rect.y0 };
                let x1 = if clip.clips_x() { rect.x1.min(clip_rect.x1) } else { rect.x1 };
                let y1 = if clip.clips_y() { rect.y1.min(clip_rect.y1) } else { rect.y1 };
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                rect = Rect::new(x0, y0, x1, y1);
            }
            if rect.y1 <= page_start || rect.y0 >= page_end {
                continue;
            }
            if decoration.is_inline() {
                if let Some(line_idx) = decoration.line_idx().or_else(|| self.doc().find_line_index_for_decoration(rect.y0, rect.y1)) {
                    if let Some(screen) = line_positions.and_then(|positions| positions.get(line_idx)) {
                        let Some(line) = lines.get(line_idx) else {
                            continue;
                        };
                        self.paint_inline_decoration_at(cx, &rect, &line, screen.point, 0.0, &decoration);
                    } else if let Some(line) = self.document.render_view().text().line(line_idx) {
                        let render_state = Self::initial_render_state();
                        let (point, _) = self.resolve_point_at(line.point(), line.height(), render_state, start_offset_y);
                        if let Some(point) = point {
                            self.paint_inline_decoration_at(cx, &rect, &line, point, inline_metrics.offset, &decoration);
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
                self.paint_decoration_segment(cx, &rect, seg_start, seg_end, current_offset, start_offset_y, &decoration);
                seg_start = seg_end;
                if break_idx < breaks.len() && breaks[break_idx].0 <= seg_start {
                    current_offset = breaks[break_idx].1;
                    break_idx += 1;
                }
            }
        }
    }

    fn paint_repeated_table_header_decorations(&self, cx: &mut impl Painter, foreground: bool, positioned_layer: bool, negative_positioned_layer: bool, independent_positioned_layer: bool) {
        for header in &self.frame.repeated_table_headers {
            for decoration in self.document.render_view().fragments().decorations().iter() {
                if decoration.is_foreground() != foreground
                    || decoration.is_in_positioned_layer() != positioned_layer
                    || decoration.is_in_negative_positioned_layer() != negative_positioned_layer
                    || decoration.is_in_independent_positioned_layer() != independent_positioned_layer
                {
                    continue;
                }
                let mut rect = decoration.rect();
                if let Some(clip) = decoration.overflow_clip() {
                    let clip_rect = clip.rect();
                    rect = Rect::new(
                        if clip.clips_x() { rect.x0.max(clip_rect.x0) } else { rect.x0 },
                        if clip.clips_y() { rect.y0.max(clip_rect.y0) } else { rect.y0 },
                        if clip.clips_x() { rect.x1.min(clip_rect.x1) } else { rect.x1 },
                        if clip.clips_y() { rect.y1.min(clip_rect.y1) } else { rect.y1 },
                    );
                }
                let source_x0 = rect.x0.max(header.source_left);
                let source_x1 = rect.x1.min(header.source_right);
                let source_y0 = rect.y0.max(header.source_top);
                let source_y1 = rect.y1.min(header.source_bottom);
                if source_x1 <= source_x0 || source_y1 <= source_y0 {
                    continue;
                }
                let x = self.col_x(header.col_index as f64, source_x0);
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

    fn paint_repeated_table_header_lines(&self, cx: &mut impl Painter, positioned_layer: bool, negative_positioned_layer: bool, independent_positioned_layer: bool, mut render_state: RenderState) -> RenderState {
        let lines = self.document.render_view().text().lines();
        for header in &self.frame.repeated_table_headers {
            for (line_idx, line) in lines.iter().enumerate() {
                if line.point().x < header.source_left - 0.01
                    || line.point().x >= header.source_right + 0.01
                    || line.point().y < header.source_top - 0.01
                    || line.point().y >= header.source_bottom - 0.01
                    || line.is_in_positioned_layer() != positioned_layer
                    || line.is_in_negative_positioned_layer() != negative_positioned_layer
                    || line.is_in_independent_positioned_layer() != independent_positioned_layer
                {
                    continue;
                }
                let point = Point::new(self.col_x(header.col_index as f64, line.point().x), line.point().y - header.source_top);
                render_state = self.draw_line_at(cx, line_idx, point, render_state);
            }
        }
        render_state
    }

    fn paint_decoration_segment(&self, cx: &mut impl Painter, rect: &kurbo::Rect, seg_start: f64, seg_end: f64, y_offset: f64, start_offset_y: f64, decoration: &html::layout::RenderDecoration) {
        // split decoration across columns and draw
        if seg_end <= seg_start {
            return;
        }

        let col_height = self.layout.size.height;
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
        let max_col = self.layout.col_count as i32 - 1;
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
            let x = self.col_x(col as f64, rect.x0);
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

    fn inline_metrics(&self) -> crate::InlineMetrics {
        // compute inline decoration offset within column gap
        const MIN_GAP: f64 = 20.0;
        let effective_gap = (self.layout.col_gap - MIN_GAP).max(0.0);
        let half_gap = effective_gap / 2.0;
        crate::InlineMetrics { offset: half_gap / 2.0 }
    }

    fn paint_inline_decoration_at(&self, cx: &mut impl Painter, rect: &kurbo::Rect, line: &html::layout::RenderLine, screen_point: Point, x_offset: f64, decoration: &html::layout::RenderDecoration) {
        // Inline backgrounds, borders, and text decorations share the same
        // paint-only optical shift as their glyphs. The source rect remains in
        // logical layout coordinates, so pagination and overflow geometry do
        // not change.
        let split_rect = project_inline_decoration_rect(*rect, line.point(), screen_point, x_offset, line.optical_offset_x());
        html::render::paint_resolved_decoration(cx, decoration, split_rect);
    }

    fn paint_decorations_for_line(&self, cx: &mut impl Painter, line_idx: usize, screen_point: Point, foreground: bool, positioned_layer: bool, negative_positioned_layer: bool, independent_positioned_layer: bool) {
        let Some(line) = self.document.render_view().text().line(line_idx) else { return };
        for decoration in self.document.render_view().fragments().decorations_for_line(line_idx).iter() {
            if decoration.is_foreground() != foreground
                || decoration.is_in_positioned_layer() != positioned_layer
                || decoration.is_in_negative_positioned_layer() != negative_positioned_layer
                || decoration.is_in_independent_positioned_layer() != independent_positioned_layer
            {
                continue;
            }
            self.paint_line_decoration(cx, &line, screen_point, &decoration);
        }
    }

    fn paint_line_decoration(&self, cx: &mut impl Painter, line: &html::layout::RenderLine, screen_point: Point, decoration: &html::layout::RenderDecoration) {
        let mut rect = decoration.rect();
        if let Some(clip) = decoration.overflow_clip() {
            let clip_rect = clip.rect();
            let x0 = if clip.clips_x() { rect.x0.max(clip_rect.x0) } else { rect.x0 };
            let y0 = if clip.clips_y() { rect.y0.max(clip_rect.y0) } else { rect.y0 };
            let x1 = if clip.clips_x() { rect.x1.min(clip_rect.x1) } else { rect.x1 };
            let y1 = if clip.clips_y() { rect.y1.min(clip_rect.y1) } else { rect.y1 };
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            rect = Rect::new(x0, y0, x1, y1);
        }
        self.paint_inline_decoration_at(cx, &rect, line, screen_point, 0.0, decoration);
    }

    fn draw_line_at(&self, cx: &mut impl Painter, line_idx: usize, point: Point, render_state: RenderState) -> RenderState {
        // Preserve the layout-space line origin for decorations and replaced
        // content. Text painters snap only the baseline axis where required;
        // moving the whole line would detach backgrounds from equivalent block
        // geometry by as much as half a device pixel.
        let point = line_paint_origin(point);
        let clip = self.document.render_view().text().line_overflow_clip(line_idx).and_then(|clip| {
            let line_point = self.document.render_view().text().line(line_idx)?.point();
            let delta = point - line_point;
            let rect = clip.rect() + delta;
            Some(Rect::new(
                if clip.clips_x() { rect.x0 } else { 0.0 },
                if clip.clips_y() { rect.y0 } else { 0.0 },
                if clip.clips_x() { rect.x1 } else { self.layout.size.width },
                if clip.clips_y() { rect.y1 } else { self.layout.size.height },
            ))
        });
        if let Some(clip) = clip {
            cx.push_clip(clip);
        }
        let positioned_layer = self.document.render_view().text().line(line_idx).is_some_and(|line| line.is_in_positioned_layer());
        let negative_positioned_layer = self.document.render_view().text().line(line_idx).is_some_and(|line| line.is_in_negative_positioned_layer());
        let independent_positioned_layer = self.document.render_view().text().line(line_idx).is_some_and(|line| line.is_in_independent_positioned_layer());
        let color = self.reader_palette.foreground.or_else(|| self.document.render_view().text().line(line_idx).and_then(|line| line.paint_color()));
        self.paint_decorations_for_line(cx, line_idx, point, false, positioned_layer, negative_positioned_layer, independent_positioned_layer);
        self.draw_images_for_line(cx, line_idx, point);
        if !html::render::paint_line_text_runs(&self.document, line_idx, point, self.layout.scale, color, cx) {
            html::render::paint_line_glyphs(&self.document, line_idx, point, self.layout.scale, color, cx);
        }
        self.paint_decorations_for_line(cx, line_idx, point, true, positioned_layer, negative_positioned_layer, independent_positioned_layer);
        if clip.is_some() {
            cx.pop_clip();
        }
        render_state
    }

    fn draw_images_for_line(&self, cx: &mut impl Painter, line_idx: usize, point: Point) {
        let images = self.document.render_view().fragments().images_for_line(line_idx);
        if images.is_empty() {
            return;
        }
        let placeholder = Color::rgba8(230, 230, 230, 255);
        for frag in images.iter() {
            self.draw_image_fragment_with_placeholder(cx, &frag, point, placeholder);
        }
    }

    fn draw_image_fragment_with_placeholder(&self, cx: &mut impl Painter, frag: &html::layout::RenderImageFragment, point: Point, placeholder: Color) {
        let (width, height) = scaled_image_size(frag);
        let offset = frag.offset();
        let rect = Rect::new(point.x + offset.x, point.y + offset.y, point.x + offset.x + width, point.y + offset.y + height);
        if let Some(decoded) = self.image_pipeline.get_decoded(frag.image_idx()) {
            match decoded {
                html::resources::DecodedImage::Raster { image, hash, .. } => cx.draw_image(image, hash, rect),
                html::resources::DecodedImage::Svg { bytes, hash, width, height } => cx.draw_svg(bytes, hash, (*width, *height), rect),
            }
        } else {
            cx.fill_rect(rect, placeholder);
        }
    }

    fn fill_selection_rect(&self, cx: &mut impl Painter, x0: f64, x1: f64, y: f64, height: f64) {
        // paint selection highlight for a horizontal span
        if x1 <= x0 {
            return;
        }
        let highlight = self.interaction_palette.selection;
        let rect = Rect::new(x0, y, x1, y + height);
        cx.fill_rect(rect, highlight);
    }

    fn paint_selection_for_line(&self, cx: &mut impl Painter, line_idx: usize, line: &html::layout::RenderLine, screen_point: Point) {
        // draw selection highlight for a line
        let Some((sel_start, sel_end)) = self.selection_range() else {
            return;
        };
        let semantic_cells = self.semantic_cell_glyph_ranges(sel_start, sel_end);
        let mut selected_ranges = self.doc().line_text_intersections(line_idx, sel_start, sel_end).map(|(start, end)| start..end).collect::<Vec<_>>();
        for promoted in self.semantic_promoted_glyph_ranges(sel_start, sel_end) {
            selected_ranges.extend(self.doc().line_text_intersections(line_idx, promoted.start, promoted.end).map(|(start, end)| start..end));
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
                let (range_x0, range_x1) = self.glyph_x_span_in_line(line, fragment.start, fragment.end);
                let x0 = screen_point.x + range_x0;
                let x1 = screen_point.x + range_x1;
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
                self.fill_selection_rect(cx, x0, x1, screen_point.y, line.height());
            }
        }
    }

    fn build_forward_positions(&self, start_offset_y: f64, positions: Vec<LineScreen>, table_events: &[TablePaginationEvent], forced_break_events: &[ForcedBreakEvent], keep_events: &[KeepRangeEvent]) -> PagePositions {
        let lines = self.document.render_view().text().lines();
        let start_line = self.doc().start_line_at(start_offset_y, 0.1);
        let mut table_event_idx = table_events.partition_point(|event| event.trigger_line < start_line);
        let mut forced_break_event_idx = forced_break_events.partition_point(|event| event.trigger_line < start_line);
        let mut keep_event_idx = keep_events.partition_point(|event| event.trigger_line < start_line);
        let mut positions = VisibleLinePositions::with_buffer(start_line, positions);
        let mut offset_breaks = Vec::new();
        let mut repeated_table_headers = Vec::new();
        let mut render_state = Self::initial_render_state();
        let mut last_line: Option<usize> = None;
        let mut next_start_offset_y = start_offset_y;
        for line_idx in start_line..lines.len() {
            while forced_break_events.get(forced_break_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                let previous_line = last_line.and_then(|line_idx| positions.get(line_idx));
                self.apply_forced_break_event(forced_break_events[forced_break_event_idx], start_offset_y, previous_line, &mut render_state);
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((forced_break_events[forced_break_event_idx].top, render_state.y_offset));
                }
                forced_break_event_idx += 1;
            }
            while keep_events.get(keep_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                self.apply_keep_range_event(keep_events[keep_event_idx], start_offset_y, &mut render_state);
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((keep_events[keep_event_idx].top, render_state.y_offset));
                }
                keep_event_idx += 1;
            }
            while table_events.get(table_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                if let Some(repeated_header) = self.apply_table_pagination_event(table_events[table_event_idx], start_offset_y, &mut render_state) {
                    repeated_table_headers.push(repeated_header);
                }
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((table_events[table_event_idx].top, render_state.y_offset));
                }
                table_event_idx += 1;
            }
            let line = lines.get(line_idx).expect("line index must stay within line_count");
            let effective_height = self.doc().effective_line_height(line_idx, line.height());
            let oversized = effective_height > self.layout.size.height;
            // An indivisible line taller than a column can never satisfy the
            // normal fit check. Fit it as one column-high atomic item so a
            // fresh page can always make progress; viewport clipping handles
            // the excess paint below the column.
            let fit_height = if oversized { self.layout.size.height } else { effective_height };
            let previous_offset = render_state.y_offset;
            let (point, next_state) = self.resolve_point_at(line.point(), fit_height, render_state, start_offset_y);
            render_state = next_state;
            if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                offset_breaks.push((line.point().y, render_state.y_offset));
            }
            if let Some(point) = point {
                positions.push(LineScreen { point, col_index: render_state.col_index as i32 });
                last_line = Some(line_idx);

                if oversized && let Some(next_line) = lines.get(line_idx + 1) {
                    let gap_after = (next_line.point().y - (line.point().y + effective_height)).max(0.0);
                    let next_page_y = (render_state.col_index + 1.0) * self.layout.size.height + gap_after;
                    render_state.y_offset = next_page_y + start_offset_y - next_line.point().y;
                    if render_state.col_index + 1.0 >= self.layout.col_count {
                        render_state.should_stop = true;
                        next_start_offset_y = next_line.point().y;
                        break;
                    }
                }
            }
            if render_state.should_stop {
                next_start_offset_y = line.point().y;
                break;
            }
        }
        let reached_end = !render_state.should_stop;
        offset_breaks.sort_by(|left, right| left.0.total_cmp(&right.0));
        let mut page = PagePositions { start_line, last_line, positions, offset_breaks, repeated_table_headers, reached_end, next_start_offset_y };
        if self.vertical_rhythm && self.pipeline_inputs.layout.text_composition_policy.is_book_optimized() {
            let rigid_ranges = table_events.iter().map(|event| (event.top, event.bottom)).chain(keep_events.iter().map(|event| (event.top, event.bottom))).collect::<Vec<_>>();
            self.apply_vertical_rhythm(&mut page, &rigid_ranges);
        }
        page
    }

    fn paint_overlay_lines_from_positions(&self, cx: &mut impl Painter, first_line: usize, end_line: usize, positions: &VisibleLinePositions) {
        let lines = self.document.render_view().text().lines();
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
        &self, cx: &mut impl Painter, first_line: usize, end_line: usize, positions: &VisibleLinePositions, mut render_state: RenderState, positioned_layer: bool, negative_positioned_layer: bool, independent_positioned_layer: bool,
    ) -> RenderState {
        for line_idx in self.document.render_view().text().paint_order_indices().iter().filter_map(|&index| usize::try_from(index).ok()).filter(|&index| (first_line..=end_line).contains(&index)) {
            if let Some(screen) = positions.get(line_idx)
                && self
                    .document
                    .render_view()
                    .text()
                    .line(line_idx)
                    .is_some_and(|line| line.is_in_positioned_layer() == positioned_layer && line.is_in_negative_positioned_layer() == negative_positioned_layer && line.is_in_independent_positioned_layer() == independent_positioned_layer)
            {
                render_state = self.draw_line_at(cx, line_idx, screen.point, render_state);
            }
        }
        render_state
    }

    fn install_positioned_page(&mut self, page: PagePositions, start_offset_y: f64, paint_end: Option<usize>, use_cached_inline_positions: bool) {
        let previous_positions = std::mem::replace(&mut self.frame.last_line_positions, page.positions);
        self.frame_scratch.recycle_line_position_buffer(previous_positions.into_buffer());
        self.frame.current_page_start_line = Some(page.start_line);
        self.frame.current_page_end_line = paint_end;
        self.frame.paint_start_offset_y = start_offset_y;
        self.frame.use_cached_inline_positions = use_cached_inline_positions;
        self.frame.offset_breaks = page.offset_breaks;
        self.frame.repeated_table_headers = page.repeated_table_headers;
    }

    pub(crate) fn paint_base_before_overlay(&self, cx: &mut impl Painter) {
        let background = self.reader_palette.background.or_else(|| self.document.render_view().canvas_background_color());
        if let Some(rgba) = background {
            let [r, g, b, a] = rgba.to_be_bytes();
            cx.fill_rect(kurbo::Rect::new(0.0, 0.0, self.layout.size.width.max(0.0), self.layout.size.height.max(0.0)), peniko::Color::rgba8(r, g, b, a));
        }
        let positions = self.frame.use_cached_inline_positions.then_some(&self.frame.last_line_positions);
        // Negative stacking contexts precede every in-flow background. They
        // cannot share the later generic positioned pass with z-index:auto/0
        // because doing so puts negative images above ordinary content.
        for independent in [false, true] {
            self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, false, true, true, independent);
            self.paint_repeated_table_header_decorations(cx, false, true, true, independent);
            let mut negative_state = Self::initial_render_state();
            if let (Some(first_line), Some(end_line)) = (self.frame.current_page_start_line, self.frame.current_page_end_line) {
                negative_state = self.paint_base_lines_from_positions(cx, first_line, end_line, &self.frame.last_line_positions, negative_state, true, true, independent);
            }
            let _ = self.paint_repeated_table_header_lines(cx, true, true, independent, negative_state);
            self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, true, true, true, independent);
            self.paint_repeated_table_header_decorations(cx, true, true, true, independent);
        }

        self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, false, false, false, false);
        self.paint_repeated_table_header_decorations(cx, false, false, false, false);
        self.paint_remaining_base(cx);
    }

    pub(crate) fn paint_interaction_overlay(&self, cx: &mut impl Painter) {
        let (Some(first_line), Some(end_line)) = (self.frame.current_page_start_line, self.frame.current_page_end_line) else { return };
        self.paint_overlay_lines_from_positions(cx, first_line, end_line, &self.frame.last_line_positions);
        self.paint_table_selection(cx);
    }

    fn paint_remaining_base(&self, cx: &mut impl Painter) -> RenderState {
        let mut render_state = Self::initial_render_state();
        if let (Some(first_line), Some(end_line)) = (self.frame.current_page_start_line, self.frame.current_page_end_line) {
            render_state = self.paint_base_lines_from_positions(cx, first_line, end_line, &self.frame.last_line_positions, render_state, false, false, false);
        }
        render_state = self.paint_repeated_table_header_lines(cx, false, false, false, render_state);
        let positions = self.frame.use_cached_inline_positions.then_some(&self.frame.last_line_positions);
        self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, true, false, false, false);
        self.paint_repeated_table_header_decorations(cx, true, false, false, false);
        for independent in [false, true] {
            self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, false, true, false, independent);
            self.paint_repeated_table_header_decorations(cx, false, true, false, independent);
            if let (Some(first_line), Some(end_line)) = (self.frame.current_page_start_line, self.frame.current_page_end_line) {
                render_state = self.paint_base_lines_from_positions(cx, first_line, end_line, &self.frame.last_line_positions, render_state, true, false, independent);
            }
            render_state = self.paint_repeated_table_header_lines(cx, true, false, independent, render_state);
            self.paint_decorations(cx, self.frame.paint_start_offset_y, &self.frame.offset_breaks, positions, true, true, false, independent);
            self.paint_repeated_table_header_decorations(cx, true, true, false, independent);
        }
        render_state
    }

    /// Base content is emitted as one coherent CSS paint stream before the
    /// interaction overlay. Keeping a nominal trailing layer preserves the
    /// backend cache interface without allowing document content to obscure
    /// selection, annotation, or media overlays.
    pub(crate) fn paint_base_after_overlay(&self, _cx: &mut impl Painter) -> RenderState {
        Self::initial_render_state()
    }

    pub(crate) fn paint_frame(&self, cx: &mut impl Painter) {
        self.paint_base_before_overlay(cx);
        self.paint_interaction_overlay(cx);
        self.paint_base_after_overlay(cx);
    }

    pub(crate) fn prepare_forward(&mut self) {
        // render forward from current offset
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);
        let boundary_anchors = self.doc().pagination_boundary_anchors();
        let table_events = self.table_pagination_events();
        let forced_break_events = self.forced_break_events(&boundary_anchors);
        let keep_events = self.automatic_keep_events(&boundary_anchors);
        let page = self.build_forward_positions(self.viewport.start_offset_y, positions, &table_events, &forced_break_events, &keep_events);
        self.viewport.next_start_offset_y = page.next_start_offset_y;
        self.viewport.reached_end = page.reached_end;
        let paint_end = page.last_line;
        self.install_positioned_page(page, self.viewport.start_offset_y, paint_end, false);
    }

    pub(crate) fn prefetch_images_for_current_page(&mut self) {
        let root = self.document.render_view();
        let text = root.text();
        let fragments = root.fragments();
        if fragments.images().is_empty() || text.line_count() == 0 {
            return;
        }
        let Some(start_line) = self.frame.current_page_start_line else {
            return;
        };
        let Some(end_line) = self.frame.current_page_end_line else {
            return;
        };
        if start_line >= text.line_count() || end_line >= text.line_count() {
            return;
        }

        let page_len = end_line.saturating_sub(start_line) + 1;
        let next_start = end_line + 1;
        let next_end = if next_start >= text.line_count() { end_line } else { (next_start + page_len - 1).min(text.line_count() - 1) };

        let mut desired = std::mem::take(&mut self.frame_scratch.desired_images);
        desired.clear();
        for line_idx in start_line..=next_end {
            for frag in fragments.images_for_line(line_idx).iter() {
                desired.insert(frag.image_idx());
            }
        }

        self.image_pipeline.ensure_window(&desired);
        self.frame_scratch.desired_images = desired;
        if self.image_pipeline.has_pending() {
            self.host.schedule_repaint(Duration::from_millis(16));
        }
    }

    pub(crate) fn prepare_backward(&mut self) {
        // render backward from end offset
        self.viewport.reached_end = false;

        let lines = self.document.render_view().text().lines();
        let end_line = if let Some(idx) = self.viewport.back_anchor_end_line {
            idx.min(lines.len().saturating_sub(1))
        } else if let Some(idx) = self.doc().end_line_at(self.viewport.end_offset_y, 1e-6) {
            idx
        } else {
            self.viewport.start_offset_y = 0.0;
            self.viewport.direction = PaintDirection::Forward;
            self.prepare_forward();
            return;
        };
        if end_line == 0 {
            self.viewport.start_offset_y = 0.0;
            self.viewport.direction = PaintDirection::Forward;
            self.viewport.back_anchor_end_line = None;
            self.prepare_forward();
            return;
        }

        let available_height = self.layout.col_count * self.layout.size.height;
        let end_line_data = lines.get(end_line).expect("end line must be within line count");
        let end_line_height = self.doc().effective_line_height(end_line, end_line_data.height());
        // Keep a tiny safety margin so the anchor line doesn't land exactly on the
        // bottom edge and get treated as overflow by column fit logic.
        let end_line_target_y = (available_height - end_line_height - 0.1).max(0.0);
        self.viewport.back_start_offset_y = (end_line_data.point().y - end_line_target_y).max(0.0);

        // Use the same forward position resolver as normal rendering so lines that share
        // the same document Y (e.g. table rows across columns) keep consistent screen Y.
        let positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);
        let boundary_anchors = self.doc().pagination_boundary_anchors();
        let table_events = self.table_pagination_events();
        let forced_break_events = self.forced_break_events(&boundary_anchors);
        let keep_events = self.automatic_keep_events(&boundary_anchors);
        let mut page = self.build_forward_positions(self.viewport.back_start_offset_y, positions, &table_events, &forced_break_events, &keep_events);

        // If anchor line is not visible, find the smallest start offset that includes it.
        // This keeps the anchor as low as possible (near page bottom) instead of pinning top.
        if !page.positions.contains(end_line) {
            let mut lo = self.viewport.back_start_offset_y;
            let mut hi = end_line_data.point().y.max(lo);
            let mut best_start = None;
            let mut candidate_positions = self.frame_scratch.take_line_position_buffer(&mut self.frame.last_line_positions);

            for _ in 0..20 {
                if (hi - lo) <= 0.01 {
                    break;
                }
                let mid = (lo + hi) * 0.5;
                let candidate = self.build_forward_positions(mid, candidate_positions, &table_events, &forced_break_events, &keep_events);
                if candidate.positions.contains(end_line) {
                    let previous = std::mem::replace(&mut page, candidate);
                    candidate_positions = previous.positions.into_buffer();
                    best_start = Some(mid);
                    hi = mid;
                } else {
                    candidate_positions = candidate.positions.into_buffer();
                    lo = mid;
                }
            }
            self.frame_scratch.recycle_line_position_buffer(candidate_positions);

            if let Some(start) = best_start {
                self.viewport.back_start_offset_y = start;
            }
        }
        let first_painted = page.start_line;
        let paint_end = page.last_line.unwrap_or(first_painted).min(end_line);
        let first_painted_y = lines.get(first_painted).map(|line| line.point().y).unwrap_or(0.0);
        self.install_positioned_page(page, self.viewport.back_start_offset_y, Some(paint_end), true);

        self.viewport.start_offset_y = first_painted_y;
        self.viewport.next_start_offset_y = self.viewport.end_offset_y;

        if first_painted == 0 && self.viewport.start_offset_y <= 0.0 {
            self.viewport.direction = PaintDirection::Forward;
            self.viewport.start_offset_y = 0.0;
            self.viewport.end_offset_y = 0.0;
            self.viewport.back_anchor_end_line = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn paint_forward(&mut self, cx: &mut impl Painter) -> RenderState {
        self.prepare_forward();
        self.paint_base_before_overlay(cx);
        self.paint_interaction_overlay(cx);
        self.paint_base_after_overlay(cx)
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
        let projected = project_inline_decoration_rect(source, Point::new(10.0, 20.0), Point::new(100.0, 200.0), 3.0, -4.5);

        assert_eq!(projected, Rect::new(108.5, 210.0, 168.5, 230.0));
        assert_eq!(projected.size(), source.size());
    }
}
