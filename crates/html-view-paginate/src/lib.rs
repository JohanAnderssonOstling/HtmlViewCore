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

use html_view_doc::DocQuery;
use html_view_types::{
    ColumnLayout, LineScreen, PagePositions, RenderState, RepeatedTableHeader, VisibleLinePositions,
};
use std::collections::{HashMap, HashSet};

mod semantic_keeps;
mod vertical_rhythm;

// These epub/ARIA predicates are pagination's own. `semantic_keeps` carries a
// similar pair whose `has_token` additionally matches namespace-prefixed tokens
// (`foo:chapter`). The two have always differed; unifying them would widen what
// counts as a sidebar or chapter start, so they stay separate until that is a
// deliberate decision.
const EPUB_NAMESPACE: &str = "http://www.idpf.org/2007/ops";

fn has_token(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| {
        value
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case(expected))
    })
}

fn has_epub_type(boxes: html::layout::RenderBoxView<'_>, box_idx: usize, expected: &str) -> bool {
    has_token(
        boxes
            .attribute(box_idx, "epub:type")
            .or_else(|| boxes.attribute_expanded(box_idx, Some(EPUB_NAMESPACE), "type")),
        expected,
    )
}

fn is_semantic_sidebar(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    let is_note = ["footnote", "endnote", "rearnote"]
        .iter()
        .any(|kind| has_epub_type(boxes, box_idx, kind))
        || ["doc-footnote", "doc-endnote"]
            .iter()
            .any(|role| has_token(boxes.attribute(box_idx, "role"), role));
    !is_note
        && (has_epub_type(boxes, box_idx, "sidebar")
            || has_token(boxes.attribute(box_idx, "role"), "complementary")
            || boxes
                .tag(box_idx)
                .is_some_and(|tag| tag.eq_ignore_ascii_case("aside")))
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

    EPUB_TYPES
        .iter()
        .any(|kind| has_epub_type(boxes, box_idx, kind))
        || DPUB_ROLES
            .iter()
            .any(|role| has_token(boxes.attribute(box_idx, "role"), role))
}

pub fn initial_render_state() -> RenderState {
    RenderState {
        y_offset: 0.0,
        col_index: 0.0,
        should_stop: false,
    }
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

#[derive(Default)]
pub struct BoundaryAnchors {
    first_line: HashMap<usize, usize>,
    next_line: HashMap<usize, usize>,
    candidates: HashSet<usize>,
    line_indices: Vec<usize>,
    last_line: HashMap<usize, usize>,
    line_owners: Vec<(usize, usize)>,
}

impl BoundaryAnchors {
    pub(crate) fn before(&self, box_idx: usize) -> Option<usize> {
        self.first_line
            .get(&box_idx)
            .copied()
            .or_else(|| self.next_line.get(&box_idx).copied())
    }

    pub(crate) fn after(&self, box_idx: usize) -> Option<usize> {
        self.next_line.get(&box_idx).copied()
    }

    pub(crate) fn boxes(&self) -> impl Iterator<Item = usize> + '_ {
        self.candidates.iter().copied()
    }

    pub(crate) fn lines(&self) -> impl Iterator<Item = usize> + '_ {
        self.line_indices.iter().copied()
    }
}

fn collect_lines_in_range(
    lines_by_y: &[(f64, usize)],
    top: f64,
    bottom: f64,
    indices: &mut Vec<usize>,
) {
    indices.clear();
    let start = lines_by_y.partition_point(|(y, _)| *y < top - 0.01);
    let end = lines_by_y.partition_point(|(y, _)| *y < bottom - 0.01);
    indices.extend(lines_by_y[start..end].iter().map(|(_, line_idx)| *line_idx));
    indices.sort_unstable();
}

fn apply_keep_range_event(
    layout: &ColumnLayout,
    event: KeepRangeEvent,
    start_offset_y: f64,
    render_state: &mut RenderState,
) {
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

fn apply_forced_break_event(
    layout: &ColumnLayout,
    event: ForcedBreakEvent,
    start_offset_y: f64,
    previous_line: Option<LineScreen>,
    render_state: &mut RenderState,
) {
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

fn apply_table_pagination_event(
    layout: &ColumnLayout,
    event: TablePaginationEvent,
    start_offset_y: f64,
    render_state: &mut RenderState,
) -> Option<RepeatedTableHeader> {
    let fragment_height = layout.size.height;
    if fragment_height <= 0.0 || event.bottom <= start_offset_y + 0.01 {
        return None;
    }
    // A continuation viewport can begin inside the leading margin or padding
    // of a row whose first visible line is the page anchor. Treat that row as
    // starting at the viewport edge so its repeated header remains visible.
    let pagination_top = event.top.max(start_offset_y);
    let projected_top = pagination_top + render_state.y_offset - start_offset_y;
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
        TablePaginationEventKind::RowGroup => {
            (event.forced && local_top > 0.01) || (height <= fragment_height && crosses)
        }
    };
    if move_to_fresh_fragment {
        render_state.y_offset += fragment_height - local_top;
    }

    let header = event.repeated_header?;
    let header_height = header.bottom - header.top;
    if header_height + height > fragment_height + 0.01 {
        return None;
    }
    let body_projected_top = pagination_top + render_state.y_offset - start_offset_y;
    let body_local_top = body_projected_top.rem_euclid(fragment_height);
    let starts_continuation = move_to_fresh_fragment
        || (body_local_top <= 0.01
            && (start_offset_y >= header.bottom - 0.01
                || body_projected_top >= fragment_height - 0.01));
    if !starts_continuation {
        return None;
    }
    let col_index = (body_projected_top / fragment_height).floor() as i32;
    if col_index < 0 || col_index as f64 >= layout.col_count {
        return None;
    }
    render_state.y_offset += header_height;
    Some(RepeatedTableHeader {
        source_left: header.left,
        source_right: header.right,
        source_top: header.top,
        source_bottom: header.bottom,
        col_index,
    })
}

fn pagination_boundary_anchors(
    doc: DocQuery<'_>,
    start_line: usize,
    end_line: usize,
    anchors: &mut BoundaryAnchors,
) {
    let boxes = doc.boxes();
    let text = doc.text();
    let line_count = text.line_count();
    let local_start = start_line.saturating_sub(1);
    let local_end = end_line.saturating_add(1).min(line_count.saturating_sub(1));
    anchors.first_line.clear();
    anchors.next_line.clear();
    anchors.candidates.clear();
    anchors.line_indices.clear();
    if line_count > 0 {
        anchors.line_indices.extend(local_start..=local_end);
    }
    let line_indices = &anchors.line_indices;
    let candidates = &mut anchors.candidates;
    let first_line = &mut anchors.first_line;
    anchors.last_line.clear();
    anchors.line_owners.clear();
    let last_line = &mut anchors.last_line;
    let line_owners = &mut anchors.line_owners;

    for line_idx in line_indices.iter().copied() {
        let Some(line) = text.line(line_idx) else {
            continue;
        };
        let owner = line.owner_box_idx();
        line_owners.push((line_idx, owner));
        for box_idx in std::iter::once(owner).chain(boxes.ancestors(owner)) {
            candidates.insert(box_idx);
            first_line
                .entry(box_idx)
                .and_modify(|first: &mut usize| *first = (*first).min(line_idx))
                .or_insert(line_idx);
            last_line
                .entry(box_idx)
                .and_modify(|last: &mut usize| *last = (*last).max(line_idx))
                .or_insert(line_idx);
        }
    }

    // Empty boxes publish no line owner, but break-before/after on an empty
    // sibling applies to the following visible line. Layout boxes are in
    // source preorder, so only inspect the bounded gaps between local owners.
    for pair in line_owners.windows(2) {
        let [(_, left), (_, right)] = pair else {
            continue;
        };
        let (start, end) = if left <= right {
            (*left, *right)
        } else {
            (*right, *left)
        };
        for box_idx in start..=end {
            candidates.insert(box_idx);
            for ancestor in boxes.ancestors(box_idx) {
                candidates.insert(ancestor);
            }
        }
    }

    let next_line = &mut anchors.next_line;
    for box_idx in candidates.iter().copied() {
        let next = if let Some(last) = last_line.get(&box_idx).copied() {
            line_owners.iter().find_map(|&(line_idx, owner)| {
                (line_idx > last && !doc.box_descends_from(owner, box_idx)).then_some(line_idx)
            })
        } else {
            line_owners.iter().find_map(|&(line_idx, owner)| {
                (owner > box_idx && !doc.box_descends_from(owner, box_idx)).then_some(line_idx)
            })
        };
        if let Some(next) = next {
            next_line.insert(box_idx, next);
        }
    }
}

/// Semantic break information used while cutting one requested page.
#[derive(Default)]
struct PaginationEvents {
    tables: Vec<TablePaginationEvent>,
    forced_breaks: Vec<ForcedBreakEvent>,
    keeps: Vec<KeepRangeEvent>,
}

#[derive(Default)]
struct VerticalRhythmScratch {
    visible: Vec<(usize, LineScreen)>,
    column_lines: Vec<(usize, LineScreen)>,
    candidates: Vec<(usize, f64)>,
    rigid_ranges: Vec<(f64, f64)>,
    original_breaks: Vec<(f64, f64)>,
}

/// Reusable transient storage for one paginator instance. It contains no
/// document identity or page result and may be retained across documents,
/// resizes, and navigation operations.
#[derive(Default)]
pub struct PaginationScratch {
    anchors: BoundaryAnchors,
    events: PaginationEvents,
    probe_positions: Vec<LineScreen>,
    reverse_positions: Vec<(usize, LineScreen)>,
    reverse_keeps: Vec<KeepRangeEvent>,
    reverse_tables: Vec<TablePaginationEvent>,
    semantic_ranges: Vec<(usize, f64, f64)>,
    table_indices: Vec<usize>,
    table_rows: Vec<(usize, Vec<usize>)>,
    table_row_spans: HashMap<usize, usize>,
    table_headers: Vec<usize>,
    lines_by_y: Vec<(f64, usize)>,
    range_lines: Vec<usize>,
    child_pairs: Vec<(Option<usize>, usize)>,
    authored_boundaries: HashSet<usize>,
    merged_forced_breaks: Vec<ForcedBreakEvent>,
    vertical_rhythm: VerticalRhythmScratch,
}

/// Paginates a document into reader pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaginationMode {
    Web,
    Book,
    BookWithVerticalRhythm,
}

#[derive(Clone, Copy)]
pub struct Paginator<'a> {
    pub(crate) doc: DocQuery<'a>,
    pub(crate) layout: &'a ColumnLayout,
    /// Book composition enables the semantic keep and break rules; web
    /// composition paginates on CSS alone.
    book_optimized: bool,
    vertical_rhythm: bool,
}

impl<'a> Paginator<'a> {
    pub fn new(doc: DocQuery<'a>, layout: &'a ColumnLayout, mode: PaginationMode) -> Self {
        Self {
            doc,
            layout,
            book_optimized: !matches!(mode, PaginationMode::Web),
            vertical_rhythm: matches!(mode, PaginationMode::BookWithVerticalRhythm),
        }
    }

    fn events_for_lines(
        &self,
        start_line: usize,
        end_line: usize,
        scratch: &mut PaginationScratch,
    ) {
        pagination_boundary_anchors(self.doc, start_line, end_line, &mut scratch.anchors);
        scratch.events.tables.clear();
        self.table_pagination_events(
            &scratch.anchors,
            &mut scratch.events.tables,
            &mut scratch.table_indices,
            &mut scratch.table_rows,
            &mut scratch.table_row_spans,
            &mut scratch.table_headers,
        );
        scratch.events.forced_breaks.clear();
        self.forced_break_events(
            &scratch.anchors,
            &mut scratch.events.forced_breaks,
            &mut scratch.authored_boundaries,
            &mut scratch.merged_forced_breaks,
        );
        scratch.events.keeps.clear();
        self.automatic_keep_events(
            &scratch.anchors,
            &mut scratch.events.keeps,
            &mut scratch.semantic_ranges,
            &mut scratch.lines_by_y,
            &mut scratch.range_lines,
            &mut scratch.child_pairs,
        );
    }

    /// Lays out only the requested page. A rule-free probe first discovers the
    /// maximum line range the viewport could expose; semantic rules are then
    /// collected solely from that range and its immediate boundary lines.
    pub fn page_at(&self, start_offset_y: f64, positions: Vec<LineScreen>) -> PagePositions {
        self.page_at_with_scratch(start_offset_y, positions, &mut PaginationScratch::default())
    }

    pub fn page_at_with_scratch(
        &self,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let probe_buffer = std::mem::take(&mut scratch.probe_positions);
        let probe = self.build_forward_positions(start_offset_y, probe_buffer, &[], &[], &[], None);
        let end_line = probe.last_line.unwrap_or(probe.start_line);
        scratch.probe_positions = probe.positions.into_buffer();
        self.events_for_lines(probe.start_line, end_line, scratch);
        self.build_forward_positions(
            start_offset_y,
            positions,
            &scratch.events.tables,
            &scratch.events.forced_breaks,
            &scratch.events.keeps,
            Some(&mut scratch.vertical_rhythm),
        )
    }

    /// Builds a page without collecting or applying book-semantic pagination
    /// events. This is a single visible-range pass intended for profiling the
    /// cost of semantic pagination independently from basic fragmentation.
    pub fn page_at_without_semantics_with_scratch(
        &self,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        _scratch: &mut PaginationScratch,
    ) -> PagePositions {
        self.build_forward_positions(start_offset_y, positions, &[], &[], &[], None)
    }

    /// Lays out a page directly from its final line toward the beginning of
    /// the document. Geometry is resolved in a mirrored block direction and
    /// mapped back to ordinary screen columns; no forward page build or offset
    /// search participates in backward navigation.
    pub fn page_ending_at(&self, end_line: usize, positions: Vec<LineScreen>) -> PagePositions {
        self.page_ending_at_with_scratch(end_line, positions, &mut PaginationScratch::default())
    }

    pub fn page_ending_at_with_scratch(
        &self,
        end_line: usize,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let end_line = end_line.min(self.doc.text().line_count().saturating_sub(1));
        let probe_buffer = std::mem::take(&mut scratch.probe_positions);
        let probe = self.build_backward_positions(end_line, 0, probe_buffer, None, scratch);
        scratch.probe_positions = probe.positions.into_buffer();
        self.events_for_lines(probe.start_line, end_line, scratch);
        let events = std::mem::take(&mut scratch.events);
        let page = self.build_backward_positions(
            end_line,
            probe.start_line,
            positions,
            Some(&events),
            scratch,
        );
        scratch.events = events;
        page
    }

    /// Reverse counterpart of [`Self::page_at_without_semantics_with_scratch`].
    pub fn page_ending_at_without_semantics_with_scratch(
        &self,
        end_line: usize,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let end_line = end_line.min(self.doc.text().line_count().saturating_sub(1));
        self.build_backward_positions(end_line, 0, positions, None, scratch)
    }

    fn table_pagination_events(
        &self,
        anchors: &BoundaryAnchors,
        events: &mut Vec<TablePaginationEvent>,
        table_indices: &mut Vec<usize>,
        rows_by_table: &mut Vec<(usize, Vec<usize>)>,
        row_spans: &mut HashMap<usize, usize>,
        leading_headers: &mut Vec<usize>,
    ) {
        if !self.book_optimized {
            return;
        }

        let root = self.doc.view();
        let boxes = root.boxes();
        let text = root.text();
        let first_local_line_in_range = |top: f64, bottom: f64| {
            anchors.lines().find(|&line_idx| {
                text.line(line_idx).is_some_and(|line| {
                    line.point().y >= top - 0.01 && line.point().y < bottom - 0.01
                })
            })
        };
        table_indices.clear();
        table_indices.extend(anchors.boxes().filter(|&box_idx| boxes.is_table(box_idx)));
        table_indices.sort_unstable();
        while rows_by_table.len() < table_indices.len() {
            rows_by_table.push((0, Vec::new()));
        }
        for ((slot, rows), &table_idx) in rows_by_table.iter_mut().zip(table_indices.iter()) {
            *slot = table_idx;
            rows.clear();
        }
        row_spans.clear();
        for box_idx in anchors.boxes() {
            if boxes.is_table_row(box_idx)
                && let Some(table_idx) = boxes
                    .ancestors(box_idx)
                    .find(|&ancestor| boxes.is_table(ancestor))
                && let Ok(table_slot) = table_indices.binary_search(&table_idx)
            {
                rows_by_table[table_slot].1.push(box_idx);
            } else if boxes.is_table_cell(box_idx)
                && let Some(row_idx) = boxes.parent(box_idx)
            {
                row_spans
                    .entry(row_idx)
                    .and_modify(|span| *span = (*span).max(boxes.table_cell_rowspan(box_idx)))
                    .or_insert_with(|| boxes.table_cell_rowspan(box_idx));
            }
        }

        for (table_idx, rows) in rows_by_table.iter_mut().take(table_indices.len()) {
            let table_idx = *table_idx;
            let (Some(table_point), Some(table_size)) =
                (boxes.point(table_idx), boxes.size(table_idx))
            else {
                continue;
            };
            let table_bottom = table_point.y + table_size.height;
            if let Some(trigger_line) = first_local_line_in_range(table_point.y, table_bottom) {
                events.push(TablePaginationEvent {
                    trigger_line,
                    top: table_point.y,
                    bottom: table_bottom,
                    kind: TablePaginationEventKind::WholeTable,
                    forced: false,
                    repeated_header: None,
                });
            }

            rows.sort_by(|&a, &b| {
                let ay = boxes.point(a).map_or(0.0, |point| point.y);
                let by = boxes.point(b).map_or(0.0, |point| point.y);
                ay.total_cmp(&by).then_with(|| a.cmp(&b))
            });

            // A repeated header is a visible dependency even when its source
            // row is above this viewport. Inspect only the leading rows of the
            // table and stop at its first body row; continuation depth never
            // affects this work.
            leading_headers.clear();
            let mut scan = table_idx.saturating_add(1);
            while scan < boxes.len() && self.doc.box_descends_from(scan, table_idx) {
                if !boxes.is_table_row(scan) {
                    scan += 1;
                    continue;
                }
                let row = scan;
                let explicit_header = boxes
                    .ancestors(row)
                    .take_while(|&ancestor| ancestor != table_idx)
                    .any(|ancestor| boxes.is_table_header_group(ancestor));
                let mut cell_count = 0usize;
                let mut header_cell_count = 0usize;
                scan += 1;
                while scan < boxes.len()
                    && self.doc.box_descends_from(scan, table_idx)
                    && !boxes.is_table_row(scan)
                {
                    if boxes.is_table_cell(scan)
                        && boxes.ancestors(scan).any(|ancestor| ancestor == row)
                    {
                        cell_count += 1;
                        if boxes
                            .tag(scan)
                            .is_some_and(|tag| tag.eq_ignore_ascii_case("th"))
                        {
                            header_cell_count += 1;
                        }
                    }
                    scan += 1;
                }
                let inferred_header =
                    leading_headers.is_empty() && cell_count > 0 && cell_count == header_cell_count;
                if explicit_header || inferred_header {
                    leading_headers.push(row);
                } else {
                    break;
                }
            }
            let repeated_header = leading_headers
                .first()
                .zip(leading_headers.last())
                .and_then(|(&first, &last)| {
                    let top = boxes.point(first)?.y;
                    let bottom = boxes.point(last)?.y + boxes.size(last)?.height;
                    (bottom > top + 0.01).then_some(TableHeaderSpec {
                        left: table_point.x,
                        right: table_point.x + table_size.width,
                        top,
                        bottom,
                    })
                });

            let mut group_start = 0usize;
            while group_start < rows.len() {
                let mut group_end = group_start + 1;
                let mut scan = group_start;
                while scan < group_end {
                    group_end = group_end
                        .max(scan.saturating_add(row_spans.get(&rows[scan]).copied().unwrap_or(1)))
                        .min(rows.len());
                    scan += 1;
                }

                let first_row = rows[group_start];
                let last_row = rows[group_end - 1];
                let Some(group_top) = boxes.point(first_row).map(|point| point.y) else {
                    group_start = group_end;
                    continue;
                };
                let mut group_bottom = boxes
                    .point(last_row)
                    .zip(boxes.size(last_row))
                    .map_or(group_top, |(point, size)| point.y + size.height);
                if group_end == rows.len()
                    && anchors.lines().any(|line_idx| {
                        self.doc.text().line(line_idx).is_some_and(|line| {
                            line.point().y >= group_bottom - 0.01
                                && line.point().y < table_bottom - 0.01
                        })
                    })
                {
                    // A bottom caption is part of the table box but not of the
                    // last row. Include it in the final atomic group.
                    group_bottom = group_bottom.max(table_bottom);
                }
                let forced = boxes.forces_break_before(first_row)
                    || (group_start > 0 && boxes.forces_break_after(rows[group_start - 1]));
                if let Some(trigger_line) = first_local_line_in_range(group_top, group_bottom) {
                    events.push(TablePaginationEvent {
                        trigger_line,
                        top: group_top,
                        bottom: group_bottom,
                        kind: TablePaginationEventKind::RowGroup,
                        forced,
                        repeated_header: repeated_header
                            .filter(|header| group_top >= header.bottom - 0.01),
                    });
                }
                group_start = group_end;
            }
        }
        events.sort_by_key(|event| (event.trigger_line, event.kind));
    }

    fn keep_range_event(
        &self,
        trigger_line: usize,
        top: f64,
        bottom: f64,
    ) -> Option<KeepRangeEvent> {
        (bottom > top + 0.01).then_some(KeepRangeEvent {
            trigger_line,
            top,
            bottom,
        })
    }

    fn automatic_keep_events(
        &self,
        anchors: &BoundaryAnchors,
        events: &mut Vec<KeepRangeEvent>,
        semantic_ranges: &mut Vec<(usize, f64, f64)>,
        lines_by_y: &mut Vec<(f64, usize)>,
        range_lines: &mut Vec<usize>,
        child_pairs: &mut Vec<(Option<usize>, usize)>,
    ) {
        let root = self.doc.view();
        let boxes = root.boxes();
        let lines = root.text().lines();
        lines_by_y.clear();
        lines_by_y.extend(
            anchors
                .lines()
                .filter_map(|line_idx| lines.get(line_idx).map(|line| (line.point().y, line_idx))),
        );
        lines_by_y.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        child_pairs.clear();
        child_pairs.extend(
            anchors
                .boxes()
                .map(|box_idx| (boxes.parent(box_idx), box_idx)),
        );
        child_pairs.sort_unstable();

        for box_idx in anchors.boxes() {
            let child_start = child_pairs.partition_point(|(parent, _)| *parent < Some(box_idx));
            let child_end = child_pairs.partition_point(|(parent, _)| *parent <= Some(box_idx));
            let child_boxes = &child_pairs[child_start..child_end];
            let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
                continue;
            };
            let bottom = point.y + size.height;
            let tag = boxes.tag(box_idx);
            range_lines.clear();
            if boxes.is_block_container(box_idx) {
                collect_lines_in_range(lines_by_y, point.y, bottom, range_lines);
            }
            let has_forced_inside = anchors.boxes().any(|descendant| {
                descendant != box_idx
                    && self.doc.box_descends_from(descendant, box_idx)
                    && (boxes.forces_break_before(descendant)
                        || boxes.forces_break_after(descendant))
            });
            if (boxes.avoids_break_inside(box_idx)
                || (self.book_optimized
                    && (tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figure"))
                        || is_semantic_sidebar(boxes, box_idx))))
                && !has_forced_inside
                && let Some(trigger_line) = anchors.before(box_idx)
                && let Some(event) = self.keep_range_event(trigger_line, point.y, bottom)
            {
                events.push(event);
            }

            let owns_line_flow = boxes.is_block_container(box_idx)
                && !child_boxes
                    .iter()
                    .any(|&(_, child)| boxes.is_block_container(child))
                && !boxes.is_table(box_idx)
                && !boxes
                    .ancestors(box_idx)
                    .any(|ancestor| boxes.is_table(ancestor));
            if owns_line_flow && range_lines.len() > 1 {
                for (count, from_end) in [
                    (boxes.orphans(box_idx), false),
                    (boxes.widows(box_idx), true),
                ] {
                    let count = count.min(range_lines.len());
                    if count <= 1 {
                        continue;
                    }
                    let selected = if from_end {
                        &range_lines[range_lines.len() - count..]
                    } else {
                        &range_lines[..count]
                    };
                    let group_top = selected
                        .iter()
                        .filter_map(|&line_idx| lines.get(line_idx).map(|line| line.point().y))
                        .fold(f64::INFINITY, f64::min);
                    let group_bottom = selected
                        .iter()
                        .filter_map(|&line_idx| {
                            lines
                                .get(line_idx)
                                .map(|line| line.point().y + line.height())
                        })
                        .fold(f64::NEG_INFINITY, f64::max);
                    if let Some(&trigger_line) = selected.iter().min()
                        && let Some(event) =
                            self.keep_range_event(trigger_line, group_top, group_bottom)
                    {
                        events.push(event);
                    }
                }
            }
        }

        let mut group_start = 0;
        while group_start < child_pairs.len() {
            let parent = child_pairs[group_start].0;
            let group_end = child_pairs[group_start..]
                .partition_point(|(candidate, _)| *candidate == parent)
                + group_start;
            for pair in child_pairs[group_start..group_end].windows(2) {
                let previous = pair[0].1;
                let next = pair[1].1;
                if boxes.forces_break_after(previous) || boxes.forces_break_before(next) {
                    continue;
                }
                let (Some(previous_point), Some(previous_size), Some(next_point), Some(next_size)) = (
                    boxes.point(previous),
                    boxes.size(previous),
                    boxes.point(next),
                    boxes.size(next),
                ) else {
                    continue;
                };
                let previous_bottom = previous_point.y + previous_size.height;
                let next_bottom = next_point.y + next_size.height;
                let previous_tag = boxes.tag(previous);
                let next_tag = boxes.tag(next);

                if boxes.avoids_break_after(previous)
                    || boxes.avoids_break_before(next)
                    || (self.book_optimized
                        && (previous_tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figcaption"))
                            || next_tag.is_some_and(|tag| tag.eq_ignore_ascii_case("figcaption"))))
                {
                    let group_top = previous_point.y.min(next_point.y);
                    let group_bottom = previous_bottom.max(next_bottom);
                    if let Some(trigger_line) = anchors.before(previous)
                        && let Some(event) =
                            self.keep_range_event(trigger_line, group_top, group_bottom)
                    {
                        events.push(event);
                    }
                } else if self.book_optimized
                    && previous_tag.is_some_and(|tag| {
                        ["h1", "h2", "h3", "h4", "h5", "h6"]
                            .iter()
                            .any(|heading| tag.eq_ignore_ascii_case(heading))
                    })
                {
                    range_lines.clear();
                    collect_lines_in_range(lines_by_y, next_point.y, next_bottom, range_lines);
                    let required_bottom = range_lines
                        .iter()
                        .take(2)
                        .filter_map(|&line_idx| {
                            lines
                                .get(line_idx)
                                .map(|line| line.point().y + line.height())
                        })
                        .fold(previous_bottom, f64::max);
                    if let Some(trigger_line) = anchors.before(previous)
                        && let Some(event) =
                            self.keep_range_event(trigger_line, previous_point.y, required_bottom)
                    {
                        events.push(event);
                    }
                }
            }
            group_start = group_end;
        }

        if self.book_optimized {
            semantic_keeps::semantic_keep_ranges(
                self.doc,
                anchors,
                semantic_ranges,
                lines_by_y,
                range_lines,
                child_pairs,
            );
            events.extend(
                semantic_ranges
                    .iter()
                    .filter_map(|&(trigger_line, top, bottom)| {
                        self.keep_range_event(trigger_line, top, bottom)
                    }),
            );
        }
        events.sort_by(|left, right| {
            left.trigger_line
                .cmp(&right.trigger_line)
                .then_with(|| left.top.total_cmp(&right.top))
                .then_with(|| right.bottom.total_cmp(&left.bottom))
        });
        events.dedup_by(|left, right| {
            left.trigger_line == right.trigger_line
                && (left.top - right.top).abs() <= 0.01
                && (left.bottom - right.bottom).abs() <= 0.01
        });
    }

    fn forced_break_events(
        &self,
        anchors: &BoundaryAnchors,
        events: &mut Vec<ForcedBreakEvent>,
        authored_boundary: &mut HashSet<usize>,
        merged: &mut Vec<ForcedBreakEvent>,
    ) {
        let boxes = self.doc.boxes();
        let lines = self.doc.text().lines();
        authored_boundary.clear();
        for box_idx in anchors.boxes() {
            let Some(point) = boxes.point(box_idx) else {
                continue;
            };
            if let Some(kind) = boxes.forced_break_before(box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
            {
                authored_boundary.insert(trigger_line);
                events.push(ForcedBreakEvent {
                    trigger_line,
                    top: point.y,
                    kind,
                });
            } else if boxes.avoids_break_before(box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
            {
                authored_boundary.insert(trigger_line);
            }
            if let Some(kind) = boxes.forced_break_after(box_idx)
                && let Some(trigger_line) = anchors.after(box_idx)
            {
                authored_boundary.insert(trigger_line);
                // The next line's document position is the only geometry
                // pagination needs. Margins remain layout concerns and are
                // neither copied into nor interpreted by the paginator.
                events.push(ForcedBreakEvent {
                    trigger_line,
                    top: lines
                        .get(trigger_line)
                        .expect("trigger line must exist")
                        .point()
                        .y,
                    kind,
                });
            } else if boxes.avoids_break_after(box_idx)
                && let Some(trigger_line) = anchors.after(box_idx)
            {
                authored_boundary.insert(trigger_line);
            }
        }
        for box_idx in anchors.boxes() {
            if self.book_optimized
                && is_semantic_page_start(boxes, box_idx)
                && let Some(trigger_line) = anchors.before(box_idx)
                && !authored_boundary.contains(&trigger_line)
                && let Some(point) = boxes.point(box_idx)
            {
                events.push(ForcedBreakEvent {
                    trigger_line,
                    top: point.y,
                    kind: html::layout::RenderForcedBreak::Page,
                });
            }
        }
        events.sort_by(|left, right| {
            left.trigger_line
                .cmp(&right.trigger_line)
                .then_with(|| left.top.total_cmp(&right.top))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        // Several nested boxes, or an adjoining break-after/break-before pair,
        // can describe the same boundary. Keep one event and let a page break
        // dominate a column break.
        merged.clear();
        for event in events.drain(..) {
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
        events.clear();
        events.extend(merged.drain(..));
    }

    /// The row-group pagination bounds covering `y`, if any. Exercised by the
    /// renderer's table-pagination tests, which live in another crate and so
    /// cannot reach a `#[cfg(test)]` item here.
    pub fn table_row_group_bounds_containing(&self, y: f64) -> Option<(f64, f64)> {
        let start = self.doc.start_line_at(y, 0.1);
        let mut anchors = BoundaryAnchors::default();
        pagination_boundary_anchors(self.doc, start, start, &mut anchors);
        let mut events = Vec::new();
        let mut table_indices = Vec::new();
        let mut rows_by_table = Vec::new();
        let mut row_spans = HashMap::new();
        let mut leading_headers = Vec::new();
        self.table_pagination_events(
            &anchors,
            &mut events,
            &mut table_indices,
            &mut rows_by_table,
            &mut row_spans,
            &mut leading_headers,
        );
        events
            .into_iter()
            .find(|event| {
                event.kind == TablePaginationEventKind::RowGroup
                    && event.top <= y + 0.01
                    && event.bottom > y + 0.01
            })
            .map(|event| (event.top, event.bottom))
    }
}

impl Paginator<'_> {
    fn prepare_visible_paint_order(&self, visible: &mut VisibleLinePositions) {
        let text = self.doc.text();
        let mut indices = visible
            .iter()
            .filter_map(|(line_idx, _)| u32::try_from(line_idx).ok())
            .collect::<Vec<_>>();
        indices.sort_unstable_by_key(|&line_idx| {
            text.paint_order_rank(line_idx as usize).unwrap_or(u32::MAX)
        });
        visible.set_paint_order_indices(indices);
    }

    fn prepare_visible_block_decorations(&self, start_offset_y: f64, indexes: &mut Vec<usize>) {
        let visible_height = self.layout.col_count * self.layout.size.height;
        self.doc
            .view()
            .fragments()
            .block_decoration_indices_intersecting(
                start_offset_y,
                start_offset_y + visible_height,
                indexes,
            );
    }

    fn build_backward_positions(
        &self,
        end_line: usize,
        minimum_line: usize,
        positions: Vec<LineScreen>,
        events: Option<&PaginationEvents>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let lines = self.doc.text().lines();
        let Some(end_data) = lines.get(end_line) else {
            return PagePositions {
                start_line: 0,
                last_line: None,
                positions: VisibleLinePositions::with_buffer(0, positions),
                offset_breaks: Vec::new(),
                repeated_table_headers: Vec::new(),
                block_decoration_indices: Vec::new(),
                reached_end: true,
                next_start_offset_y: 0.0,
            };
        };
        let end_height = self.doc.effective_line_height(end_line, end_data.height());
        let end_bottom = end_data.point().y + end_height;
        let mut render_state = initial_render_state();
        let mut reversed = std::mem::take(&mut scratch.reverse_positions);
        reversed.clear();

        let reverse_range = |top: f64, bottom: f64| (end_bottom - bottom, end_bottom - top);
        let range_trigger = |top: f64, bottom: f64| {
            (minimum_line..=end_line).rev().find(|&line_idx| {
                lines.get(line_idx).is_some_and(|line| {
                    let height = self.doc.effective_line_height(line_idx, line.height());
                    line.point().y < bottom - 0.01 && line.point().y + height > top + 0.01
                })
            })
        };
        let mut reverse_keeps = std::mem::take(&mut scratch.reverse_keeps);
        reverse_keeps.clear();
        if let Some(events) = events {
            reverse_keeps.extend(events.keeps.iter().filter_map(|event| {
                let trigger_line = range_trigger(event.top, event.bottom)?;
                let (top, bottom) = reverse_range(event.top, event.bottom);
                Some(KeepRangeEvent {
                    trigger_line,
                    top,
                    bottom,
                })
            }));
        }
        let mut reverse_tables = std::mem::take(&mut scratch.reverse_tables);
        reverse_tables.clear();
        if let Some(events) = events {
            reverse_tables.extend(events.tables.iter().filter_map(|event| {
                let trigger_line = range_trigger(event.top, event.bottom)?;
                let (top, bottom) = reverse_range(event.top, event.bottom);
                Some(TablePaginationEvent {
                    trigger_line,
                    top,
                    bottom,
                    kind: event.kind,
                    forced: event.forced,
                    repeated_header: None,
                })
            }));
        }

        for line_idx in (minimum_line..=end_line).rev() {
            let line = lines
                .get(line_idx)
                .expect("reverse line index must stay within line count");
            let effective_height = self.doc.effective_line_height(line_idx, line.height());
            let reverse_y = (end_bottom - (line.point().y + effective_height)).max(0.0);

            if line_idx < end_line
                && let Some(forced) = events.and_then(|events| {
                    events
                        .forced_breaks
                        .iter()
                        .find(|event| event.trigger_line == line_idx + 1)
                })
            {
                match forced.kind {
                    html::layout::RenderForcedBreak::Page => break,
                    html::layout::RenderForcedBreak::Column => {
                        let projected = reverse_y + render_state.y_offset;
                        let local = projected.rem_euclid(self.layout.size.height);
                        if local > 0.01 {
                            render_state.y_offset += self.layout.size.height - local;
                        }
                    }
                }
            }
            for event in reverse_keeps
                .iter()
                .filter(|event| event.trigger_line == line_idx)
            {
                apply_keep_range_event(self.layout, *event, 0.0, &mut render_state);
            }
            for event in reverse_tables
                .iter()
                .filter(|event| event.trigger_line == line_idx)
            {
                let _ = apply_table_pagination_event(self.layout, *event, 0.0, &mut render_state);
            }

            let oversized = effective_height > self.layout.size.height;
            let fit_height = if oversized {
                self.layout.size.height
            } else {
                effective_height
            };
            let (reverse_point, next_state) = self.layout.resolve_point_at(
                kurbo::Point::new(line.point().x, reverse_y),
                fit_height,
                render_state,
                0.0,
            );
            render_state = next_state;
            let Some(reverse_point) = reverse_point else {
                break;
            };
            let reverse_col = render_state.col_index as i32;
            let col_index = (self.layout.col_count as i32 - 1 - reverse_col).max(0);
            let point = kurbo::Point::new(
                self.layout.col_x(col_index as f64, line.point().x),
                (self.layout.size.height - reverse_point.y - fit_height).max(0.0),
            );
            reversed.push((line_idx, LineScreen { point, col_index }));
        }

        reversed.reverse();
        let start_line = reversed.first().map_or(end_line, |(line_idx, _)| *line_idx);
        let paint_start_offset_y = lines.get(start_line).map_or(0.0, |line| line.point().y);
        let mut visible = VisibleLinePositions::with_buffer(start_line, positions);
        let mut offset_breaks = Vec::new();
        let mut previous_offset: Option<f64> = None;
        for (line_idx, screen) in reversed.drain(..) {
            let line = lines
                .get(line_idx)
                .expect("positioned reverse line must exist");
            let offset = screen.point.y
                + screen.col_index as f64 * self.layout.size.height
                + paint_start_offset_y
                - line.point().y;
            if previous_offset.is_none_or(|previous| (previous - offset).abs() > 1e-6) {
                offset_breaks.push((line.point().y, offset));
                previous_offset = Some(offset);
            }
            visible.push(screen);
        }
        self.prepare_visible_paint_order(&mut visible);
        let mut block_decoration_indices = Vec::new();
        self.prepare_visible_block_decorations(paint_start_offset_y, &mut block_decoration_indices);
        scratch.reverse_positions = reversed;
        scratch.reverse_keeps = reverse_keeps;
        scratch.reverse_tables = reverse_tables;
        PagePositions {
            start_line,
            last_line: (!visible.is_empty()).then_some(end_line),
            positions: visible,
            offset_breaks,
            repeated_table_headers: Vec::new(),
            block_decoration_indices,
            reached_end: start_line == 0,
            next_start_offset_y: end_bottom,
        }
    }

    fn build_forward_positions(
        &self,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        table_events: &[TablePaginationEvent],
        forced_break_events: &[ForcedBreakEvent],
        keep_events: &[KeepRangeEvent],
        rhythm_scratch: Option<&mut VerticalRhythmScratch>,
    ) -> PagePositions {
        let lines = self.doc.text().lines();
        let start_line = self.doc.start_line_at(start_offset_y, 0.1);
        let mut table_event_idx =
            table_events.partition_point(|event| event.trigger_line < start_line);
        let mut forced_break_event_idx =
            forced_break_events.partition_point(|event| event.trigger_line < start_line);
        let mut keep_event_idx =
            keep_events.partition_point(|event| event.trigger_line < start_line);
        let mut positions = VisibleLinePositions::with_buffer(start_line, positions);
        let mut offset_breaks = Vec::new();
        let mut repeated_table_headers = Vec::new();
        let mut render_state = initial_render_state();
        let mut last_line: Option<usize> = None;
        let mut next_start_offset_y = start_offset_y;
        for line_idx in start_line..lines.len() {
            while forced_break_events
                .get(forced_break_event_idx)
                .is_some_and(|event| event.trigger_line == line_idx)
            {
                let previous_offset = render_state.y_offset;
                let previous_line = last_line.and_then(|line_idx| positions.get(line_idx));
                apply_forced_break_event(
                    self.layout,
                    forced_break_events[forced_break_event_idx],
                    start_offset_y,
                    previous_line,
                    &mut render_state,
                );
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((
                        forced_break_events[forced_break_event_idx].top,
                        render_state.y_offset,
                    ));
                }
                forced_break_event_idx += 1;
            }
            while keep_events
                .get(keep_event_idx)
                .is_some_and(|event| event.trigger_line == line_idx)
            {
                let previous_offset = render_state.y_offset;
                apply_keep_range_event(
                    self.layout,
                    keep_events[keep_event_idx],
                    start_offset_y,
                    &mut render_state,
                );
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((keep_events[keep_event_idx].top, render_state.y_offset));
                }
                keep_event_idx += 1;
            }
            while table_events
                .get(table_event_idx)
                .is_some_and(|event| event.trigger_line == line_idx)
            {
                let previous_offset = render_state.y_offset;
                if let Some(repeated_header) = apply_table_pagination_event(
                    self.layout,
                    table_events[table_event_idx],
                    start_offset_y,
                    &mut render_state,
                ) {
                    repeated_table_headers.push(repeated_header);
                }
                if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                    offset_breaks.push((table_events[table_event_idx].top, render_state.y_offset));
                }
                table_event_idx += 1;
            }
            let line = lines
                .get(line_idx)
                .expect("line index must stay within line_count");
            let effective_height = self.doc.effective_line_height(line_idx, line.height());
            let oversized = effective_height > self.layout.size.height;
            // An indivisible line taller than a column can never satisfy the
            // normal fit check. Fit it as one column-high atomic item so a
            // fresh page can always make progress; viewport clipping handles
            // the excess paint below the column.
            let fit_height = if oversized {
                self.layout.size.height
            } else {
                effective_height
            };
            let previous_offset = render_state.y_offset;
            let (point, next_state) = self.layout.resolve_point_at(
                line.point(),
                fit_height,
                render_state,
                start_offset_y,
            );
            render_state = next_state;
            if (render_state.y_offset - previous_offset).abs() > 1e-6 {
                offset_breaks.push((line.point().y, render_state.y_offset));
            }
            if let Some(point) = point {
                positions.push(LineScreen {
                    point,
                    col_index: render_state.col_index as i32,
                });
                last_line = Some(line_idx);

                if oversized && let Some(next_line) = lines.get(line_idx + 1) {
                    let gap_after =
                        (next_line.point().y - (line.point().y + effective_height)).max(0.0);
                    let next_page_y =
                        (render_state.col_index + 1.0) * self.layout.size.height + gap_after;
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
        let mut page = PagePositions {
            start_line,
            last_line,
            positions,
            offset_breaks,
            repeated_table_headers,
            block_decoration_indices: Vec::new(),
            reached_end,
            next_start_offset_y,
        };
        if self.vertical_rhythm
            && self.book_optimized
            && let Some(scratch) = rhythm_scratch
        {
            scratch.rigid_ranges.clear();
            scratch.rigid_ranges.extend(
                table_events
                    .iter()
                    .map(|event| (event.top, event.bottom))
                    .chain(keep_events.iter().map(|event| (event.top, event.bottom))),
            );
            self.apply_vertical_rhythm(&mut page, scratch);
        }
        self.prepare_visible_paint_order(&mut page.positions);
        self.prepare_visible_block_decorations(start_offset_y, &mut page.block_decoration_indices);
        page
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use html::layout::{
        FontSlant, GlyphId, GlyphMetric, GlyphResourceStore, GlyphShaper, LaidOutDocument,
        LayoutConstraints,
    };
    use html::pipeline::DocumentFactory;
    use kurbo::Size;
    use std::collections::HashMap;

    /// Fixed-metric shaper: every glyph is half the font size wide and
    /// three-quarters tall, so line heights in these tests are predictable.
    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        glyph_store: GlyphResourceStore,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceStore)>,
    }

    impl GlyphShaper for TestShaper {
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

        fn shape_glyph(
            &mut self,
            ch: char,
            font_size: f32,
            _weight: u16,
            _slant: FontSlant,
            _color: u32,
            _family: Option<&str>,
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
            let glyph = self.glyph_store.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout(html: &str, width: f64) -> LaidOutDocument {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut shaper)
            .expect("the test shaper registers every glyph")
            .layout(LayoutConstraints::new(width, 16.0).expect("constraints must be valid"))
    }

    fn columns(width: f64, height: f64, count: f64) -> ColumnLayout {
        ColumnLayout {
            col_width: width,
            base_col_width: width,
            col_count: count,
            col_gap: 0.0,
            size: Size::new(width * count, height),
            scale: 1.0,
            max_col_count: None,
        }
    }

    /// Paginates one page and returns (line index, column, screen y) per line.
    fn paginate(
        document: &LaidOutDocument,
        layout: &ColumnLayout,
        mode: PaginationMode,
    ) -> Vec<(usize, i32, f64)> {
        let paginator = Paginator::new(DocQuery::new(document), layout, mode);
        let page = paginator.page_at(0.0, Vec::new());
        page.positions
            .iter()
            .map(|(line, screen)| (line, screen.col_index, screen.point.y))
            .collect()
    }

    fn column_of(placed: &[(usize, i32, f64)], line: usize) -> Option<i32> {
        placed
            .iter()
            .find(|(idx, _, _)| *idx == line)
            .map(|(_, col, _)| *col)
    }

    const PARAGRAPHS: &str = "<p>aaa</p><p>bbb</p><p>ccc</p><p>ddd</p><p>eee</p><p>fff</p>";

    #[test]
    fn page_retains_only_visible_lines_in_css_paint_order() {
        let document = layout(
            "<style>.front { position: relative; z-index: 2 }</style><p>first</p><p class='front'>front</p><p>last</p>",
            200.0,
        );
        let geometry = columns(200.0, 200.0, 1.0);
        let page = Paginator::new(DocQuery::new(&document), &geometry, PaginationMode::Book)
            .page_at(0.0, Vec::new());
        let expected = document
            .render_view()
            .text()
            .paint_order_indices()
            .iter()
            .copied()
            .filter(|&line_idx| page.positions.contains(line_idx as usize))
            .collect::<Vec<_>>();

        assert_eq!(page.positions.paint_order_indices(), expected);
        assert_eq!(
            expected.len(),
            page.positions.len(),
            "the retained traversal contains each visible line exactly once"
        );
    }

    #[test]
    fn page_retains_only_intersecting_block_decorations_in_css_order() {
        let content = (0..40)
            .map(|index| {
                format!("<p style='margin:0;height:20px;background:#{index:02x}0000'>row</p>")
            })
            .collect::<String>();
        let document = layout(&content, 200.0);
        let geometry = columns(200.0, 100.0, 1.0);
        let page = Paginator::new(DocQuery::new(&document), &geometry, PaginationMode::Book)
            .page_at(0.0, Vec::new());
        let fragments = document.render_view().fragments();
        let expected = fragments
            .block_decorations()
            .iter()
            .filter(|decoration| decoration.rect().y0 < 100.0 && decoration.rect().y1 > 0.0)
            .map(|decoration| (decoration.rect(), decoration.color()))
            .collect::<Vec<_>>();
        let actual = fragments
            .decorations_by_indices(&page.block_decoration_indices)
            .iter()
            .map(|decoration| (decoration.rect(), decoration.color()))
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        assert!(
            actual.len() < fragments.block_decorations().len(),
            "off-page decorations stay out of the retained page traversal"
        );
    }

    #[test]
    fn lines_that_do_not_fit_move_to_the_next_column() {
        let document = layout(PARAGRAPHS, 200.0);
        let placed = paginate(&document, &columns(200.0, 40.0, 2.0), PaginationMode::Book);

        assert!(!placed.is_empty(), "something must be laid out");
        let columns_used: Vec<i32> = placed.iter().map(|(_, col, _)| *col).collect();
        assert!(
            columns_used.contains(&1),
            "a 40px column cannot hold six paragraphs, so later lines belong in column 1"
        );
        assert!(
            columns_used.windows(2).all(|w| w[0] <= w[1]),
            "columns must fill in order"
        );
    }

    #[test]
    fn every_placed_line_sits_within_its_column() {
        let document = layout(PARAGRAPHS, 200.0);
        let geometry = columns(200.0, 40.0, 2.0);
        for (line, _, y) in paginate(&document, &geometry, PaginationMode::Book) {
            assert!(y >= 0.0, "line {line} was placed above its column");
            assert!(
                y < geometry.size.height,
                "line {line} at y={y} overflows the {}px column",
                geometry.size.height
            );
        }
    }

    #[test]
    fn a_forced_break_before_moves_content_to_a_fresh_column() {
        let document = layout("<p>aaa</p><p style='break-before: column'>bbb</p>", 200.0);
        let placed = paginate(&document, &columns(200.0, 200.0, 2.0), PaginationMode::Book);

        assert_eq!(column_of(&placed, 0), Some(0));
        assert_eq!(
            column_of(&placed, 1),
            Some(1),
            "an authored column break must move the paragraph even though the column has room"
        );
    }

    #[test]
    fn a_semantic_chapter_starts_a_fresh_page_under_book_composition() {
        let source = "<p>aaa</p><section epub:type='chapter'><p>bbb</p></section>";
        let document = layout(source, 200.0);
        let layout = columns(200.0, 200.0, 2.0);

        let book = paginate(&document, &layout, PaginationMode::Book);
        assert_eq!(column_of(&book, 0), Some(0));
        assert_ne!(
            column_of(&book, 1),
            Some(0),
            "a chapter section should not share a page with preceding content"
        );
    }

    #[test]
    fn web_composition_ignores_semantic_chapter_breaks() {
        let source = "<p>aaa</p><section epub:type='chapter'><p>bbb</p></section>";
        let document = layout(source, 200.0);
        let layout = columns(200.0, 200.0, 2.0);

        let web = paginate(&document, &layout, PaginationMode::Web);
        assert_eq!(column_of(&web, 0), Some(0));
        assert_eq!(
            column_of(&web, 1),
            Some(0),
            "web composition paginates on CSS alone, so the section stays in flow"
        );
    }

    #[test]
    fn book_composition_keeps_a_compact_list_item_whole() {
        // Four lines is the compact limit for a list item; the column below fits
        // three, so the whole item must move rather than split.
        let source = "<p>aaa</p><ul><li>one two three four five six seven eight</li></ul>";
        let document = layout(source, 60.0);
        let layout = columns(60.0, 60.0, 2.0);

        let book = paginate(&document, &layout, PaginationMode::Book);
        let item_lines: Vec<i32> = book.iter().skip(1).map(|(_, col, _)| *col).collect();
        assert!(
            item_lines.len() > 1,
            "the fixture must wrap onto several lines or the test proves nothing: {book:?}"
        );
        assert!(
            item_lines.windows(2).all(|w| w[0] == w[1]),
            "a compact list item must not be split across columns: {book:?}"
        );
    }

    #[test]
    fn events_are_empty_under_web_composition() {
        let document = layout(
            "<table><tr><td>aaa</td></tr></table><section epub:type='chapter'><p>bbb</p></section>",
            200.0,
        );
        let layout = columns(200.0, 200.0, 2.0);
        let paginator = Paginator::new(DocQuery::new(&document), &layout, PaginationMode::Web);
        let mut scratch = PaginationScratch::default();
        paginator.events_for_lines(
            0,
            document.render_view().text().line_count().saturating_sub(1),
            &mut scratch,
        );

        assert!(
            scratch.events.tables.is_empty(),
            "table pagination is a book-composition rule"
        );
        assert!(
            scratch.events.keeps.is_empty(),
            "automatic keeps are a book-composition rule"
        );
    }

    #[test]
    fn pagination_is_deterministic_for_the_same_inputs() {
        let document = layout(PARAGRAPHS, 200.0);
        let layout = columns(200.0, 40.0, 2.0);
        assert_eq!(
            paginate(&document, &layout, PaginationMode::Book),
            paginate(&document, &layout, PaginationMode::Book)
        );
    }

    #[test]
    fn a_later_start_offset_begins_at_a_later_line() {
        let document = layout(PARAGRAPHS, 200.0);
        let layout = columns(200.0, 40.0, 2.0);
        let paginator = Paginator::new(DocQuery::new(&document), &layout, PaginationMode::Book);
        let first = paginator.page_at(0.0, Vec::new());
        let next = paginator.page_at(first.next_start_offset_y, Vec::new());
        assert!(
            next.start_line > first.start_line,
            "paging forward must advance: {} then {}",
            first.start_line,
            next.start_line
        );
    }

    #[test]
    fn an_empty_document_paginates_to_nothing() {
        let document = layout("", 200.0);
        let placed = paginate(&document, &columns(200.0, 200.0, 2.0), PaginationMode::Book);
        assert!(placed.is_empty());
    }

    #[test]
    fn visible_pagination_work_does_not_grow_with_trailing_chapter_content() {
        let prefix = (0..20)
            .map(|index| format!("<p>visible {index}</p>"))
            .collect::<String>();
        let trailing = (0..2_000)
            .map(|index| format!("<p>trailing {index}</p>"))
            .collect::<String>();
        let short = layout(&prefix, 200.0);
        let long = layout(&format!("{prefix}{trailing}"), 200.0);
        let geometry = columns(200.0, 120.0, 1.0);

        let candidate_count = |document: &LaidOutDocument| {
            let paginator =
                Paginator::new(DocQuery::new(document), &geometry, PaginationMode::Book);
            let probe = paginator.build_forward_positions(0.0, Vec::new(), &[], &[], &[], None);
            let mut anchors = BoundaryAnchors::default();
            pagination_boundary_anchors(
                paginator.doc,
                probe.start_line,
                probe.last_line.unwrap_or(probe.start_line),
                &mut anchors,
            );
            anchors.candidates.len()
        };

        assert_eq!(
            candidate_count(&short),
            candidate_count(&long),
            "content after the viewport must not add pagination candidates"
        );
    }

    #[test]
    fn a_table_header_remains_a_bounded_visible_dependency_on_later_pages() {
        let rows = (0..40)
            .map(|index| format!("<tr><td>body {index}</td></tr>"))
            .collect::<String>();
        let document = layout(
            &format!("<table><thead><tr><th>header</th></tr></thead><tbody>{rows}</tbody></table>"),
            200.0,
        );
        let geometry = columns(200.0, 60.0, 1.0);
        let paginator = Paginator::new(DocQuery::new(&document), &geometry, PaginationMode::Book);

        let first = paginator.page_at(0.0, Vec::new());
        let second = paginator.page_at(first.next_start_offset_y, Vec::new());
        let third = paginator.page_at(second.next_start_offset_y, Vec::new());

        assert!(
            !second.repeated_table_headers.is_empty(),
            "the source header should repeat after the first viewport"
        );
        assert!(
            !third.repeated_table_headers.is_empty(),
            "header lookup must remain correct deeper in the table"
        );
    }

    #[test]
    fn pagination_scratch_retains_hot_path_allocations() {
        let rows = (0..12)
            .map(|index| format!("<tr><td>body {index}</td></tr>"))
            .collect::<String>();
        let document = layout(
            &format!(
                "<h1>Heading</h1><table><thead><tr><th>header</th></tr></thead><tbody>{rows}</tbody></table>"
            ),
            120.0,
        );
        let geometry = columns(120.0, 80.0, 2.0);
        let paginator = Paginator::new(
            DocQuery::new(&document),
            &geometry,
            PaginationMode::BookWithVerticalRhythm,
        );
        let mut scratch = PaginationScratch::default();

        let _ = paginator.page_at_with_scratch(0.0, Vec::new(), &mut scratch);
        let capacities = [
            scratch.anchors.line_indices.capacity(),
            scratch.anchors.line_owners.capacity(),
            scratch.anchors.candidates.capacity(),
            scratch.events.tables.capacity(),
            scratch.events.forced_breaks.capacity(),
            scratch.events.keeps.capacity(),
            scratch.probe_positions.capacity(),
            scratch.table_indices.capacity(),
            scratch.table_rows.capacity(),
            scratch.table_row_spans.capacity(),
            scratch.lines_by_y.capacity(),
            scratch.range_lines.capacity(),
            scratch.child_pairs.capacity(),
            scratch.vertical_rhythm.visible.capacity(),
            scratch.vertical_rhythm.column_lines.capacity(),
            scratch.vertical_rhythm.candidates.capacity(),
            scratch.vertical_rhythm.rigid_ranges.capacity(),
        ];

        let _ = paginator.page_at_with_scratch(0.0, Vec::new(), &mut scratch);
        let reused_capacities = [
            scratch.anchors.line_indices.capacity(),
            scratch.anchors.line_owners.capacity(),
            scratch.anchors.candidates.capacity(),
            scratch.events.tables.capacity(),
            scratch.events.forced_breaks.capacity(),
            scratch.events.keeps.capacity(),
            scratch.probe_positions.capacity(),
            scratch.table_indices.capacity(),
            scratch.table_rows.capacity(),
            scratch.table_row_spans.capacity(),
            scratch.lines_by_y.capacity(),
            scratch.range_lines.capacity(),
            scratch.child_pairs.capacity(),
            scratch.vertical_rhythm.visible.capacity(),
            scratch.vertical_rhythm.column_lines.capacity(),
            scratch.vertical_rhythm.candidates.capacity(),
            scratch.vertical_rhythm.rigid_ranges.capacity(),
        ];

        assert_eq!(
            reused_capacities, capacities,
            "a repeated page build should reuse its warmed scratch buffers"
        );
        assert!(
            capacities.iter().any(|&capacity| capacity > 0),
            "the fixture must exercise allocated scratch storage"
        );
    }
}
