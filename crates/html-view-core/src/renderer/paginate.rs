//! Pagination: turning a laid-out document into the lines that appear on one
//! reader page.
//!
//! This is where the book-layout rules live -- widow and orphan limits, table
//! row-group breaks, figure and caption keeps, semantic chapter breaks, and the
//! optional vertical-rhythm refinement.
//!
//! Pagination is a pure function of the document, the column geometry and the
//! composition policy. It reads no frame, no viewport, no host and no pipeline;
//! storing the result is the caller's job. [`Paginator`] makes that checkable.

use crate::doc_query::DocQuery;
use crate::{ColumnLayout, LineScreen, PagePositions, RenderState, RepeatedTableHeader, VisibleLinePositions};

// These epub/ARIA predicates are pagination's own. `semantic_keeps` carries a
// similar pair whose `has_token` additionally matches namespace-prefixed tokens
// (`foo:chapter`). The two have always differed; unifying them would widen what
// counts as a sidebar or chapter start, so they stay separate until that is a
// deliberate decision.
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


pub(crate) fn initial_render_state() -> RenderState {
    RenderState { y_offset: 0.0, col_index: 0.0, should_stop: false }
}

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

pub(crate) struct BoundaryAnchors {
    first_line: Vec<Option<usize>>,
    next_line: Vec<Option<usize>>,
    pub(crate) subtree_end: Vec<usize>,
}

impl BoundaryAnchors {
    pub(crate) fn before(&self, box_idx: usize) -> Option<usize> {
        self.first_line.get(box_idx).copied().flatten().or_else(|| self.next_line.get(box_idx).copied().flatten())
    }

    pub(crate) fn after(&self, box_idx: usize) -> Option<usize> {
        self.next_line.get(box_idx).copied().flatten()
    }
}

fn apply_keep_range_event(layout: &ColumnLayout, event: KeepRangeEvent, start_offset_y: f64, render_state: &mut RenderState) {
    let fragment_height = layout.size.height;
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

fn apply_forced_break_event(layout: &ColumnLayout, event: ForcedBreakEvent, start_offset_y: f64, previous_line: Option<LineScreen>, render_state: &mut RenderState) {
    let fragment_height = layout.size.height;
    if fragment_height <= 0.0 || event.top < start_offset_y - 0.01 {
        return;
    }
    // At the beginning of a newly selected reader page the break has
    // already been satisfied. Otherwise base the destination solely on
    // the last column that contains actual content, never on margin-only
    // document space.
    let target_col = previous_line.map_or(0.0, |previous| match event.kind {
        html::layout::RenderForcedBreak::Column => previous.col_index as f64 + 1.0,
        html::layout::RenderForcedBreak::Page => layout.col_count,
    });
    let projected_top = event.top + render_state.y_offset - start_offset_y;
    render_state.y_offset += target_col * fragment_height - projected_top;
}

fn apply_table_pagination_event(layout: &ColumnLayout, event: TablePaginationEvent, start_offset_y: f64, render_state: &mut RenderState) -> Option<RepeatedTableHeader> {
    let fragment_height = layout.size.height;
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
    if col_index < 0 || col_index as f64 >= layout.col_count {
        return None;
    }
    render_state.y_offset += header_height;
    Some(RepeatedTableHeader { source_left: header.left, source_right: header.right, source_top: header.top, source_bottom: header.bottom, col_index })
}

impl<'a> DocQuery<'a> {
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
}

/// Pagination events for one document, computed once and reused across page
/// builds. Backward navigation binary-searches for a start offset, so this must
/// not be recomputed per candidate.
pub(crate) struct PaginationEvents {
    tables: Vec<TablePaginationEvent>,
    forced_breaks: Vec<ForcedBreakEvent>,
    keeps: Vec<KeepRangeEvent>,
}

/// Paginates a document into reader pages.
#[derive(Clone, Copy)]
pub(crate) struct Paginator<'a> {
    pub(crate) doc: DocQuery<'a>,
    pub(crate) layout: &'a ColumnLayout,
    /// Book composition enables the semantic keep and break rules; web
    /// composition paginates on CSS alone.
    book_optimized: bool,
    vertical_rhythm: bool,
}

impl<'a> Paginator<'a> {
    pub(crate) fn new(doc: DocQuery<'a>, layout: &'a ColumnLayout, book_optimized: bool, vertical_rhythm: bool) -> Self {
        Self { doc, layout, book_optimized, vertical_rhythm }
    }

    pub(crate) fn events(&self) -> PaginationEvents {
        let anchors = self.doc.pagination_boundary_anchors();
        PaginationEvents { tables: self.table_pagination_events(), forced_breaks: self.forced_break_events(&anchors), keeps: self.automatic_keep_events(&anchors) }
    }

    /// Lays out one page starting at `start_offset_y`, reusing `positions` as
    /// scratch storage.
    pub(crate) fn page_at(&self, events: &PaginationEvents, start_offset_y: f64, positions: Vec<LineScreen>) -> PagePositions {
        self.build_forward_positions(start_offset_y, positions, &events.tables, &events.forced_breaks, &events.keeps)
    }

    fn table_pagination_events(&self) -> Vec<TablePaginationEvent> {
        if !self.book_optimized {
            return Vec::new();
        }

        let root = self.doc.view();
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
            if let Some(trigger_line) = self.doc.first_line_in_vertical_range(table_point.y, table_bottom) {
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
                if let Some(trigger_line) = self.doc.first_line_in_vertical_range(group_top, group_bottom) {
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
        if !self.book_optimized {
            return Vec::new();
        }
        let root = self.doc.view();
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

        events.extend(self.doc.semantic_keep_ranges(anchors).into_iter().filter_map(|(trigger_line, top, bottom)| self.keep_range_event(trigger_line, top, bottom)));

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

    fn forced_break_events(&self, anchors: &BoundaryAnchors) -> Vec<ForcedBreakEvent> {
        if !self.book_optimized {
            return Vec::new();
        }
        let boxes = self.doc.boxes();
        let lines = self.doc.text().lines();
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

    #[cfg(test)]
    pub(crate) fn table_row_group_bounds_containing(&self, y: f64) -> Option<(f64, f64)> {
        self.table_pagination_events().into_iter().find(|event| event.kind == TablePaginationEventKind::RowGroup && event.top <= y + 0.01 && event.bottom > y + 0.01).map(|event| (event.top, event.bottom))
    }

    fn build_forward_positions(&self, start_offset_y: f64, positions: Vec<LineScreen>, table_events: &[TablePaginationEvent], forced_break_events: &[ForcedBreakEvent], keep_events: &[KeepRangeEvent]) -> PagePositions {
        let lines = self.doc.text().lines();
        let start_line = self.doc.start_line_at(start_offset_y, 0.1);
        let mut table_event_idx = table_events.partition_point(|event| event.trigger_line < start_line);
        let mut forced_break_event_idx = forced_break_events.partition_point(|event| event.trigger_line < start_line);
        let mut keep_event_idx = keep_events.partition_point(|event| event.trigger_line < start_line);
        let mut positions = VisibleLinePositions::with_buffer(start_line, positions);
        let mut offset_breaks = Vec::new();
        let mut repeated_table_headers = Vec::new();
        let mut render_state = initial_render_state();
        let mut last_line: Option<usize> = None;
        let mut next_start_offset_y = start_offset_y;
        for line_idx in start_line..lines.len() {
            while forced_break_events.get(forced_break_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                let previous_line = last_line.and_then(|line_idx| positions.get(line_idx));
                apply_forced_break_event(self.layout, forced_break_events[forced_break_event_idx], start_offset_y, previous_line, &mut render_state);
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((forced_break_events[forced_break_event_idx].top, render_state.y_offset));
                }
                forced_break_event_idx += 1;
            }
            while keep_events.get(keep_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                apply_keep_range_event(self.layout, keep_events[keep_event_idx], start_offset_y, &mut render_state);
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((keep_events[keep_event_idx].top, render_state.y_offset));
                }
                keep_event_idx += 1;
            }
            while table_events.get(table_event_idx).is_some_and(|event| event.trigger_line == line_idx) {
                let previous_offset = render_state.y_offset;
                if let Some(repeated_header) = apply_table_pagination_event(self.layout, table_events[table_event_idx], start_offset_y, &mut render_state) {
                    repeated_table_headers.push(repeated_header);
                }
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((table_events[table_event_idx].top, render_state.y_offset));
                }
                table_event_idx += 1;
            }
            let line = lines.get(line_idx).expect("line index must stay within line_count");
            let effective_height = self.doc.effective_line_height(line_idx, line.height());
            let oversized = effective_height > self.layout.size.height;
            // An indivisible line taller than a column can never satisfy the
            // normal fit check. Fit it as one column-high atomic item so a
            // fresh page can always make progress; viewport clipping handles
            // the excess paint below the column.
            let fit_height = if oversized { self.layout.size.height } else { effective_height };
            let previous_offset = render_state.y_offset;
            let (point, next_state) = self.layout.resolve_point_at(line.point(), fit_height, render_state, start_offset_y);
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
        if self.vertical_rhythm && self.book_optimized {
            let rigid_ranges = table_events.iter().map(|event| (event.top, event.bottom)).chain(keep_events.iter().map(|event| (event.top, event.bottom))).collect::<Vec<_>>();
            self.apply_vertical_rhythm(&mut page, &rigid_ranges);
        }
        page
    }
}
