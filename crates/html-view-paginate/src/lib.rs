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
use std::collections::HashSet;

mod semantic_keeps;
mod vertical_rhythm;

use html::layout::ContentRole;

pub fn initial_render_state() -> RenderState {
    RenderState {
        y_offset: 0.0,
        col_index: 0.0,
        should_stop: false,
    }
}

use html::structure::{BlockConstraints, HeaderPolicy, SiblingPair, Structure, TableScratch};
use html::structure::{
    BoundaryIndex as BoundaryAnchors, TableGroup as TablePaginationEvent,
    TableGroupKind as TablePaginationEventKind,
};

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

/// Shared bounded inputs for CSS and reader keep preferences.
#[derive(Default)]
struct KeepContext {
    lines_by_y: Vec<(f64, usize)>,
    constraints: Vec<(usize, BlockConstraints)>,
    siblings: Vec<SiblingPair>,
}

impl KeepContext {
    fn prepare(&mut self, doc: DocQuery<'_>, anchors: &BoundaryAnchors) {
        let root = doc.view();
        let lines = root.pagination_lines();
        self.lines_by_y.clear();
        self.lines_by_y.extend(
            anchors
                .lines()
                .filter_map(|index| lines.group_start(index))
                .filter_map(|cursor| lines.at(cursor).map(|line| (line.point().y, cursor))),
        );
        self.lines_by_y.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        self.lines_by_y.dedup_by_key(|(_, cursor)| *cursor);
        let structure = Structure::new(root);
        self.constraints.clear();
        self.constraints.extend(
            anchors
                .boxes()
                .map(|index| (index, structure.constraints(index, anchors))),
        );
        self.siblings = structure.sibling_pairs(anchors);
    }

    fn lines_in_range(&self, top: f64, bottom: f64, indices: &mut Vec<usize>) {
        indices.clear();
        let start = self.lines_by_y.partition_point(|(y, _)| *y < top - 0.01);
        let end = self.lines_by_y.partition_point(|(y, _)| *y < bottom - 0.01);
        indices.extend(self.lines_by_y[start..end].iter().map(|(_, line)| *line));
        indices.sort_unstable();
    }
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

    let header = event.header?;
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
    tables: TableScratch,
    keep_context: KeepContext,
    range_lines: Vec<usize>,
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
        let structure = Structure::new(self.doc.view());
        structure.index_lines(
            start_line.saturating_sub(1)..end_line.saturating_add(2),
            &mut scratch.anchors,
        );
        scratch.events.tables.clear();
        if self.book_optimized {
            scratch
                .events
                .tables
                .extend_from_slice(structure.table_dependencies(
                    &scratch.anchors,
                    HeaderPolicy::InferLeadingHeaderCells,
                    &mut scratch.tables,
                ));
        }
        scratch.events.forced_breaks.clear();
        self.forced_break_events(
            &scratch.anchors,
            &mut scratch.events.forced_breaks,
            &mut scratch.authored_boundaries,
            &mut scratch.merged_forced_breaks,
        );
        scratch.events.keeps.clear();
        scratch.keep_context.prepare(self.doc, &scratch.anchors);
        self.automatic_keep_events(
            &scratch.anchors,
            &mut scratch.events.keeps,
            &mut scratch.semantic_ranges,
            &scratch.keep_context,
            &mut scratch.range_lines,
        );
    }

    /// Lays out only the requested page. A rule-free probe first discovers the
    /// maximum line range the viewport could expose; semantic rules are then
    /// collected solely from that range and its immediate boundary lines.
    /// For page turns, use [`Self::page_starting_at_with_scratch`] with the
    /// next cursor: an offset alone cannot distinguish overlapping groups.
    pub fn page_at(&self, start_offset_y: f64, positions: Vec<LineScreen>) -> PagePositions {
        self.page_at_with_scratch(start_offset_y, positions, &mut PaginationScratch::default())
    }

    pub fn page_at_with_scratch(
        &self,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let start_line = self
            .doc
            .view()
            .pagination_lines()
            .start_at(start_offset_y, 0.1);
        self.page_from_line_at_with_scratch(start_line, start_offset_y, positions, scratch)
    }

    /// Starts at an explicit pagination cursor. Use the preceding page's
    /// `last_line + 1` to continue even when groups share a document-space y.
    pub fn page_starting_at_with_scratch(
        &self,
        start_line: usize,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let lines = self.doc.view().pagination_lines();
        let group = lines.group(start_line);
        let start_line = group
            .as_ref()
            .map_or(lines.len(), |group| group.cursors().start);
        let offset = group.map_or(0.0, |group| group.point().y);
        self.page_from_line_at_with_scratch(start_line, offset, positions, scratch)
    }

    fn page_from_line_at_with_scratch(
        &self,
        start_line: usize,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let probe_buffer = std::mem::take(&mut scratch.probe_positions);
        let probe = self.build_forward_positions(
            start_line,
            start_offset_y,
            probe_buffer,
            &[],
            &[],
            &[],
            None,
        );
        let end_line = probe.last_line.unwrap_or(probe.start_line);
        scratch.probe_positions = probe.positions.into_buffer();
        self.events_for_lines(probe.start_line, end_line, scratch);
        self.build_forward_positions(
            start_line,
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
        let start_line = self
            .doc
            .view()
            .pagination_lines()
            .start_at(start_offset_y, 0.1);
        self.build_forward_positions(start_line, start_offset_y, positions, &[], &[], &[], None)
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
        let lines = self.doc.view().pagination_lines();
        let end_line = lines
            .group_end(end_line.min(lines.len().saturating_sub(1)))
            .unwrap_or(0);
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
        let lines = self.doc.view().pagination_lines();
        let end_line = lines
            .group_end(end_line.min(lines.len().saturating_sub(1)))
            .unwrap_or(0);
        self.build_backward_positions(end_line, 0, positions, None, scratch)
    }

    fn keep_range_event(
        &self,
        trigger_line: usize,
        top: f64,
        bottom: f64,
    ) -> Option<KeepRangeEvent> {
        (bottom > top + 0.01).then_some(KeepRangeEvent {
            trigger_line: self.doc.view().pagination_lines().group_start(trigger_line)?,
            top,
            bottom,
        })
    }

    fn automatic_keep_events(
        &self,
        anchors: &BoundaryAnchors,
        events: &mut Vec<KeepRangeEvent>,
        semantic_ranges: &mut Vec<(usize, f64, f64)>,
        context: &KeepContext,
        range_lines: &mut Vec<usize>,
    ) {
        let root = self.doc.view();
        let boxes = root.boxes();
        let lines = root.pagination_lines();
        for &(box_idx, ref constraints) in &context.constraints {
            let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
                continue;
            };
            let bottom = point.y + size.height;
            let semantics = self.doc.view().semantics().content(box_idx);
            range_lines.clear();
            if boxes.is_block_container(box_idx) {
                context.lines_in_range(point.y, bottom, range_lines);
            }
            if (constraints.avoid_inside
                || (self.book_optimized
                    && (semantics.has_role(ContentRole::Figure)
                        || semantics.has_role(ContentRole::Sidebar))))
                && !constraints.has_forced_descendant
                && let Some(trigger_line) = anchors.before(box_idx)
                && let Some(event) = self.keep_range_event(trigger_line, point.y, bottom)
            {
                events.push(event);
            }

            if constraints.owns_line_flow && range_lines.len() > 1 {
                for (count, from_end) in [(constraints.orphans, false), (constraints.widows, true)]
                {
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
                        .filter_map(|&line_idx| lines.at(line_idx).map(|line| line.point().y))
                        .fold(f64::INFINITY, f64::min);
                    let group_bottom = selected
                        .iter()
                        .filter_map(|&line_idx| {
                            lines
                                .at(line_idx)
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

        for pair in &context.siblings {
            if pair.forced_boundary {
                continue;
            }
            let previous = pair.previous;
            let next = pair.next;
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

            if pair.avoid_boundary || (self.book_optimized && pair.caption_pair) {
                let group_top = previous_point.y.min(next_point.y);
                let group_bottom = previous_bottom.max(next_bottom);
                if let Some(trigger_line) = anchors.before(previous)
                    && let Some(event) =
                        self.keep_range_event(trigger_line, group_top, group_bottom)
                {
                    events.push(event);
                }
            } else if self.book_optimized && pair.heading_following {
                range_lines.clear();
                context.lines_in_range(next_point.y, next_bottom, range_lines);
                let required_bottom = range_lines
                    .iter()
                    .take(2)
                    .filter_map(|&line_idx| {
                        lines
                            .at(line_idx)
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

        if self.book_optimized {
            semantic_keeps::semantic_keep_ranges(
                self.doc,
                anchors,
                semantic_ranges,
                context,
                range_lines,
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
        authored_boundary.clear();
        for boundary in Structure::new(self.doc.view()).authored_boundaries(anchors) {
            authored_boundary.insert(boundary.trigger_line);
            if let Some(kind) = boundary.forced {
                events.push(ForcedBreakEvent {
                    trigger_line: boundary.trigger_line,
                    top: boundary.top,
                    kind,
                });
            }
        }
        for box_idx in anchors.boxes() {
            if self.book_optimized
                && self
                    .doc
                    .view()
                    .semantics()
                    .content(box_idx)
                    .has_role(ContentRole::MajorSection)
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

    fn prepare_visible_block_decorations(
        &self,
        start_offset_y: f64,
        positions: &VisibleLinePositions,
        indexes: &mut Vec<usize>,
    ) {
        let height = self.layout.size.height;
        let mut windows = vec![(
            start_offset_y,
            start_offset_y + self.layout.col_count * height,
        )];
        // A compressed or moved group can expose source geometry beyond the
        // nominal viewport interval. Include its actual column's source window.
        for (index, screen) in positions.iter() {
            if let Some(line) = self.doc.text().line(index) {
                let top = line.point().y - screen.point.y;
                windows.push((top, top + height));
            }
        }
        windows.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (top, bottom) in windows {
            if let Some(previous) = merged.last_mut()
                && top <= previous.1
            {
                previous.1 = previous.1.max(bottom);
            } else {
                merged.push((top, bottom));
            }
        }
        indexes.clear();
        let mut candidates = Vec::new();
        for (top, bottom) in merged {
            self.doc
                .view()
                .fragments()
                .block_decoration_indices_intersecting(top, bottom, &mut candidates);
            indexes.extend_from_slice(&candidates);
        }
        // Scene replay restores CSS order after combining the source windows.
        indexes.sort_unstable();
        indexes.dedup();
    }

    fn build_backward_positions(
        &self,
        end_line: usize,
        minimum_line: usize,
        positions: Vec<LineScreen>,
        events: Option<&PaginationEvents>,
        scratch: &mut PaginationScratch,
    ) -> PagePositions {
        let lines = self.doc.view().pagination_lines();
        let Some(end_data) = lines.group(end_line) else {
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
        let end_height = end_data.height();
        let end_bottom = end_data.point().y + end_height;
        let mut render_state = initial_render_state();
        let mut reversed = std::mem::take(&mut scratch.reverse_positions);
        reversed.clear();

        let reverse_range = |top: f64, bottom: f64| (end_bottom - bottom, end_bottom - top);
        let range_trigger = |top: f64, bottom: f64| {
            (minimum_line..=end_line)
                .rev()
                .find(|&line_idx| {
                    lines.at(line_idx).is_some_and(|line| {
                        let height = line.height();
                        line.point().y < bottom - 0.01 && line.point().y + height > top + 0.01
                    })
                })
                .and_then(|cursor| lines.group_end(cursor))
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
                    table_box: event.table_box,
                    trigger_line,
                    top,
                    bottom,
                    kind: event.kind,
                    forced: event.forced,
                    header: None,
                })
            }));
        }

        let mut line_idx = end_line;
        while line_idx >= minimum_line {
            let line = lines
                .group(line_idx)
                .expect("reverse line index must stay within line count");
            let effective_height = line.height();
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
            // Positioned groups can overlap in source coordinates. A group
            // fitted to a whole column must start in an unoccupied column.
            if oversized && !reversed.is_empty() {
                let projected = reverse_y + render_state.y_offset;
                let next_column = (render_state.col_index + 1.0) * self.layout.size.height;
                render_state.y_offset += (next_column - projected).max(0.0);
            }
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
            for (cursor, member) in line.cursors().rev().zip(line.lines().rev()) {
                reversed.push((
                    cursor,
                    LineScreen {
                        point: point + (member.point() - line.point()),
                        col_index,
                    },
                ));
            }
            let Some(previous) = line.cursors().start.checked_sub(1) else {
                break;
            };
            if oversized {
                if render_state.col_index + 1.0 >= self.layout.col_count {
                    break;
                }
                let previous_group = lines.group(previous).expect("previous pagination group");
                let gap = (line.point().y - (previous_group.point().y + previous_group.height()))
                    .max(0.0);
                let previous_y =
                    (end_bottom - (previous_group.point().y + previous_group.height())).max(0.0);
                render_state.y_offset =
                    (render_state.col_index + 1.0) * self.layout.size.height + gap - previous_y;
            }
            line_idx = previous;
        }

        reversed.reverse();
        let start_line = reversed.first().map_or(end_line, |(line_idx, _)| *line_idx);
        let paint_start_offset_y = lines.at(start_line).map_or(0.0, |line| line.point().y);
        let mut visible = VisibleLinePositions::with_buffer(start_line, positions);
        let mut offset_breaks = Vec::new();
        let mut previous_offset: Option<f64> = None;
        for (line_idx, screen) in reversed.drain(..) {
            let line = lines
                .at(line_idx)
                .expect("positioned reverse line must exist");
            let offset = screen.point.y
                + screen.col_index as f64 * self.layout.size.height
                + paint_start_offset_y
                - line.point().y;
            if previous_offset.is_none_or(|previous| (previous - offset).abs() > 1e-6) {
                offset_breaks.push((line.point().y, offset));
                previous_offset = Some(offset);
            }
            visible.push(line.id(), screen);
        }
        self.prepare_visible_paint_order(&mut visible);
        let mut block_decoration_indices = Vec::new();
        self.prepare_visible_block_decorations(
            paint_start_offset_y,
            &visible,
            &mut block_decoration_indices,
        );
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
        start_line: usize,
        start_offset_y: f64,
        positions: Vec<LineScreen>,
        table_events: &[TablePaginationEvent],
        forced_break_events: &[ForcedBreakEvent],
        keep_events: &[KeepRangeEvent],
        rhythm_scratch: Option<&mut VerticalRhythmScratch>,
    ) -> PagePositions {
        let lines = self.doc.view().pagination_lines();
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
        let mut line_idx = start_line;
        while line_idx < lines.len() {
            while forced_break_events
                .get(forced_break_event_idx)
                .is_some_and(|event| event.trigger_line == line_idx)
            {
                let previous_offset = render_state.y_offset;
                let previous_line = last_line.and_then(|line_idx| positions.at_cursor(line_idx));
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
                .group(line_idx)
                .expect("line index must stay within line_count");
            let effective_height = line.height();
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
            // Positioned groups can overlap in source coordinates. A group
            // fitted to a whole column must start in an unoccupied column.
            if oversized && last_line.is_some() {
                let projected = line.point().y + render_state.y_offset - start_offset_y;
                let next_column = (render_state.col_index + 1.0) * self.layout.size.height;
                render_state.y_offset += (next_column - projected).max(0.0);
            }
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
                for member in line.lines() {
                    positions.push(
                        member.id(),
                        LineScreen {
                            point: point + (member.point() - line.point()),
                            col_index: render_state.col_index as i32,
                        },
                    );
                }
                last_line = line.cursors().end.checked_sub(1);

                if oversized && let Some(next_line) = lines.group(line.cursors().end) {
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
            line_idx = line.cursors().end;
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
        self.prepare_visible_block_decorations(
            start_offset_y,
            &page.positions,
            &mut page.block_decoration_indices,
        );
        page
    }
}

#[cfg(test)]
mod tests {
    use html::text::{GlyphRequest, ShapedGlyph};

    use super::*;
    use html::layout::{
        FontSlant, GlyphId, GlyphMetric, GlyphResourceGeneration, GlyphResourceStore, GlyphShaper,
        LaidOutDocument, LayoutConstraints,
    };
    use html::pipeline::DocumentFactory;
    use kurbo::Size;
    use std::collections::HashMap;

    /// Fixed-metric shaper: every glyph is half the font size wide and
    /// three-quarters tall, so line heights in these tests are predictable.
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

    fn layout(
        html: &str,
        width: f64,
        resources: &mut GlyphResourceStore,
    ) -> LaidOutDocument<'static> {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut shaper, resources)
            .expect("the test shaper registers every glyph")
            .layout(
                LayoutConstraints::new(width, 16.0).expect("constraints must be valid"),
                resources,
            )
            .expect("test layout succeeds")
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
        resources: &GlyphResourceStore,
    ) -> Vec<(usize, i32, f64)> {
        let paginator = Paginator::new(DocQuery::new(document, resources), layout, mode);
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
    fn wrapped_list_placements_preserve_line_identity_in_both_directions() {
        let mut resources = GlyphResourceStore::default();
        let document = layout(
            "<body style='margin:0'><ul style='margin:0'><li>First item wraps onto several lines with its bullet beside the first line.</li><li>Another wrapped item keeps its continuation below its own first line.</li></ul></body>",
            200.0,
            &mut resources,
        );
        let view = document.render_view(&resources);
        let lines = view.text().lines();
        assert!(
            lines
                .iter()
                .enumerate()
                .any(|(rank, line)| rank != line.index())
        );
        let geometry = columns(200.0, 2000.0, 1.0);
        for mode in [
            PaginationMode::Web,
            PaginationMode::Book,
            PaginationMode::BookWithVerticalRhythm,
        ] {
            let paginator = Paginator::new(DocQuery::new(&document, &resources), &geometry, mode);
            for page in [
                paginator.page_at(0.0, Vec::new()),
                paginator.page_ending_at(lines.len() - 1, Vec::new()),
            ] {
                assert_eq!(page.positions.len(), lines.len());
                let first = lines.first().unwrap();
                let displacement = page.positions.get(first.index()).unwrap().point - first.point();
                for line in lines.iter() {
                    let actual = page
                        .positions
                        .get(line.index())
                        .expect("every line has a placement");
                    assert_eq!(actual.col_index, 0);
                    let expected = line.point() + displacement;
                    assert!(
                        (actual.point - expected).hypot() < 0.001,
                        "{mode:?}: line {} must retain its own geometry: actual={:?}, expected={expected:?}",
                        line.index(),
                        actual.point
                    );
                }
            }
        }
    }

    #[test]
    fn paginating_wrapped_lists_across_pages_keeps_bullets_with_their_text() {
        let mut resources = GlyphResourceStore::default();
        let content =
            "<li>Several words make this list item wrap across multiple lines.</li>".repeat(6);
        let document = layout(
            &format!("<body style='margin:0'><ul style='margin:0'>{content}</ul></body>"),
            200.0,
            &mut resources,
        );
        let view = document.render_view(&resources);
        let geometry = columns(200.0, 115.0, 2.0);
        for mode in [
            PaginationMode::Web,
            PaginationMode::Book,
            PaginationMode::BookWithVerticalRhythm,
        ] {
            let paginator = Paginator::new(DocQuery::new(&document, &resources), &geometry, mode);
            let mut seen = HashSet::new();
            let mut offset = 0.0;
            for _ in 0..view.text().line_count() {
                let page = paginator.page_at(offset, Vec::new());
                assert!(!page.positions.is_empty());
                for (id, screen) in page.positions.iter() {
                    assert!(seen.insert(id), "line {id} must appear on exactly one page");
                    let line = view.text().line(id).unwrap();
                    assert_eq!(
                        screen.point.x,
                        geometry.col_x(f64::from(screen.col_index), line.point().x)
                    );
                    let content: String = line
                        .glyphs()
                        .map(|glyph| {
                            view.text()
                                .glyph_metric(view.text().glyph_at(glyph as usize).unwrap())
                                .unwrap()
                                .ch()
                        })
                        .collect();
                    if content == "•" {
                        let first_text = view
                            .text()
                            .lines()
                            .iter()
                            .find(|candidate| {
                                candidate.index() != id
                                    && (candidate.point_y() - line.point_y()).abs() < 0.001
                            })
                            .unwrap();
                        let text_screen = page
                            .positions
                            .get(first_text.index())
                            .expect("a bullet and its text must share a page");
                        assert_eq!(screen.col_index, text_screen.col_index);
                        assert!((screen.point.y - text_screen.point.y).abs() < 0.001);
                    }
                }
                if page.reached_end {
                    break;
                }
                assert!(page.next_start_offset_y > offset);
                offset = page.next_start_offset_y;
            }
            assert_eq!(seen.len(), view.text().line_count());
        }
    }

    #[test]
    fn oversized_list_content_keeps_its_marker_on_the_same_page() {
        let mut resources = GlyphResourceStore::default();
        let document = layout(
            "<body style='margin:0'><ul style='margin:0'><li><span style='line-height:80px;font-size:60px'>Big</span></li></ul></body>",
            200.0,
            &mut resources,
        );
        let geometry = columns(200.0, 40.0, 1.0);
        let page = Paginator::new(
            DocQuery::new(&document, &resources),
            &geometry,
            PaginationMode::Book,
        )
        .page_at(0.0, Vec::new());
        assert!(page.reached_end);
        assert_eq!(
            page.positions.len(),
            document.render_view(&resources).text().line_count()
        );
        for (_, screen) in page.positions.iter() {
            assert_eq!(screen.col_index, 0);
            assert_eq!(screen.point.y, 0.0);
        }
    }

    #[test]
    fn interleaved_positioned_text_does_not_split_an_oversized_list_group() {
        for other_y in [10, 30] {
            let mut resources = GlyphResourceStore::default();
            let document = layout(
                &format!(
                    "<body style='margin:0;line-height:20px'><ul style='margin:0'><li><span style='line-height:80px'>Tall first</span></li></ul><div style='position:absolute;top:{other_y}px;left:120px'>Other</div></body>"
                ),
                200.0,
                &mut resources,
            );
            let view = document.render_view(&resources);
            let geometry = columns(200.0, 40.0, 1.0);
            let item = (0..view.boxes().len())
                .find(|&index| view.boxes().tag(index) == Some("li"))
                .unwrap();
            let marker_box = view.boxes().list_marker(item).unwrap().marker_box();
            let marker = view
                .text()
                .lines()
                .iter()
                .find(|line| line.owner_box_idx() == marker_box)
                .unwrap();
            let source = view
                .pagination_lines()
                .source_line(view.pagination_lines().cursor(marker.id()).unwrap())
                .unwrap();
            for mode in [
                PaginationMode::Web,
                PaginationMode::Book,
                PaginationMode::BookWithVerticalRhythm,
            ] {
                let paginator = Paginator::new(DocQuery::new(&document, &resources), &geometry, mode);
                let mut seen = HashSet::new();
                let mut offset = 0.0;
                for _ in 0..view.text().line_count() {
                    let page = paginator.page_at(offset, Vec::new());
                    assert!(!page.positions.is_empty());
                    for (id, _) in page.positions.iter() {
                        assert!(seen.insert(id), "a line must not repeat across pages");
                    }
                    assert_eq!(
                        page.positions.contains(marker.index()),
                        page.positions.contains(source.index()),
                        "a marker and its text must share a page"
                    );
                    if page.reached_end {
                        break;
                    }
                    assert!(page.next_start_offset_y > offset);
                    offset = page.next_start_offset_y;
                }
                assert_eq!(seen.len(), view.text().line_count());
            }
        }
    }

    #[test]
    fn backward_page_includes_the_complete_requested_group() {
        let mut resources = GlyphResourceStore::default();
        let document = layout(
            "<body style='margin:0'><ul style='margin:0'><li>First</li></ul></body>",
            200.0,
            &mut resources,
        );
        let geometry = columns(200.0, 100.0, 1.0);
        let paginator = Paginator::new(
            DocQuery::new(&document, &resources),
            &geometry,
            PaginationMode::Book,
        );
        for end in 0..document.render_view(&resources).text().line_count() {
            let page = paginator.page_ending_at(end, Vec::new());
            assert_eq!(
                page.positions.len(),
                document.render_view(&resources).text().line_count()
            );
            let mut scratch = PaginationScratch::default();
            let page =
                paginator.page_ending_at_without_semantics_with_scratch(end, Vec::new(), &mut scratch);
            assert_eq!(
                page.positions.len(),
                document.render_view(&resources).text().line_count()
            );
        }
    }

    #[test]
    fn page_retains_only_visible_lines_in_css_paint_order() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<style>.front { position: relative; z-index: 2 }</style><p>first</p><p class='front'>front</p><p>last</p>",
            200.0,
            &mut document_resources,
        );
        let geometry = columns(200.0, 200.0, 1.0);
        let page = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &geometry,
            PaginationMode::Book,
        )
        .page_at(0.0, Vec::new());
        let expected = document
            .render_view(&document_resources)
            .text()
            .paint_order_indices()
            .iter()
            .map(|line| line.index() as u32)
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(&content, 200.0, &mut document_resources);
        let geometry = columns(200.0, 100.0, 1.0);
        let page = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &geometry,
            PaginationMode::Book,
        )
        .page_at(0.0, Vec::new());
        let fragments = document.render_view(&document_resources).fragments();
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(PARAGRAPHS, 200.0, &mut document_resources);
        let placed = paginate(
            &document,
            &columns(200.0, 40.0, 2.0),
            PaginationMode::Book,
            &document_resources,
        );

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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(PARAGRAPHS, 200.0, &mut document_resources);
        let geometry = columns(200.0, 40.0, 2.0);
        for (line, _, y) in paginate(
            &document,
            &geometry,
            PaginationMode::Book,
            &document_resources,
        ) {
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<p>aaa</p><p style='break-before: column'>bbb</p>",
            200.0,
            &mut document_resources,
        );
        let placed = paginate(
            &document,
            &columns(200.0, 200.0, 2.0),
            PaginationMode::Book,
            &document_resources,
        );

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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(source, 200.0, &mut document_resources);
        let layout = columns(200.0, 200.0, 2.0);

        let book = paginate(
            &document,
            &layout,
            PaginationMode::Book,
            &document_resources,
        );
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(source, 200.0, &mut document_resources);
        let layout = columns(200.0, 200.0, 2.0);

        let web = paginate(&document, &layout, PaginationMode::Web, &document_resources);
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(source, 60.0, &mut document_resources);
        let layout = columns(60.0, 60.0, 2.0);

        let book = paginate(
            &document,
            &layout,
            PaginationMode::Book,
            &document_resources,
        );
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            "<table><tr><td>aaa</td></tr></table><section epub:type='chapter'><p>bbb</p></section>",
            200.0,
            &mut document_resources,
        );
        let layout = columns(200.0, 200.0, 2.0);
        let paginator = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &layout,
            PaginationMode::Web,
        );
        let mut scratch = PaginationScratch::default();
        paginator.events_for_lines(
            0,
            document
                .render_view(&document_resources)
                .text()
                .line_count()
                .saturating_sub(1),
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(PARAGRAPHS, 200.0, &mut document_resources);
        let layout = columns(200.0, 40.0, 2.0);
        assert_eq!(
            paginate(
                &document,
                &layout,
                PaginationMode::Book,
                &document_resources
            ),
            paginate(
                &document,
                &layout,
                PaginationMode::Book,
                &document_resources
            )
        );
    }

    #[test]
    fn a_later_start_offset_begins_at_a_later_line() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(PARAGRAPHS, 200.0, &mut document_resources);
        let layout = columns(200.0, 40.0, 2.0);
        let paginator = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &layout,
            PaginationMode::Book,
        );
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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout("", 200.0, &mut document_resources);
        let placed = paginate(
            &document,
            &columns(200.0, 200.0, 2.0),
            PaginationMode::Book,
            &document_resources,
        );
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
        let mut short_resources = GlyphResourceStore::default();
        let short = layout(&prefix, 200.0, &mut short_resources);
        let mut long_resources = GlyphResourceStore::default();
        let long = layout(&format!("{prefix}{trailing}"), 200.0, &mut long_resources);
        let geometry = columns(200.0, 120.0, 1.0);

        let candidate_count = |document: &LaidOutDocument, resources: &GlyphResourceStore| {
            let paginator = Paginator::new(
                DocQuery::new(document, resources),
                &geometry,
                PaginationMode::Book,
            );
            let probe = paginator.build_forward_positions(0, 0.0, Vec::new(), &[], &[], &[], None);
            let mut anchors = BoundaryAnchors::default();
            Structure::new(paginator.doc.view()).index_lines(
                probe.start_line.saturating_sub(1)
                    ..probe
                        .last_line
                        .unwrap_or(probe.start_line)
                        .saturating_add(2),
                &mut anchors,
            );
            anchors.boxes().count()
        };

        assert_eq!(
            candidate_count(&short, &short_resources),
            candidate_count(&long, &long_resources),
            "content after the viewport must not add pagination candidates"
        );
    }

    #[test]
    fn a_table_header_remains_a_bounded_visible_dependency_on_later_pages() {
        let rows = (0..40)
            .map(|index| format!("<tr><td>body {index}</td></tr>"))
            .collect::<String>();
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            &format!("<table><thead><tr><th>header</th></tr></thead><tbody>{rows}</tbody></table>"),
            200.0,
            &mut document_resources,
        );
        let geometry = columns(200.0, 60.0, 1.0);
        let paginator = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &geometry,
            PaginationMode::Book,
        );

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
        let mut document_resources = GlyphResourceStore::default();
        let document = layout(
            &format!(
                "<h1>Heading</h1><table><thead><tr><th>header</th></tr></thead><tbody>{rows}</tbody></table>"
            ),
            120.0,
            &mut document_resources,
        );
        let geometry = columns(120.0, 80.0, 2.0);
        let paginator = Paginator::new(
            DocQuery::new(&document, &document_resources),
            &geometry,
            PaginationMode::BookWithVerticalRhythm,
        );
        let mut scratch = PaginationScratch::default();

        let _ = paginator.page_at_with_scratch(0.0, Vec::new(), &mut scratch);
        let capacities = [
            scratch.anchors.capacities()[0],
            scratch.anchors.capacities()[1],
            scratch.anchors.capacities()[2],
            scratch.events.tables.capacity(),
            scratch.events.forced_breaks.capacity(),
            scratch.events.keeps.capacity(),
            scratch.probe_positions.capacity(),
            scratch.tables.capacities()[1],
            scratch.tables.capacities()[2],
            scratch.tables.capacities()[3],
            scratch.keep_context.lines_by_y.capacity(),
            scratch.range_lines.capacity(),
            scratch.vertical_rhythm.visible.capacity(),
            scratch.vertical_rhythm.column_lines.capacity(),
            scratch.vertical_rhythm.candidates.capacity(),
            scratch.vertical_rhythm.rigid_ranges.capacity(),
        ];

        let _ = paginator.page_at_with_scratch(0.0, Vec::new(), &mut scratch);
        let reused_capacities = [
            scratch.anchors.capacities()[0],
            scratch.anchors.capacities()[1],
            scratch.anchors.capacities()[2],
            scratch.events.tables.capacity(),
            scratch.events.forced_breaks.capacity(),
            scratch.events.keeps.capacity(),
            scratch.probe_positions.capacity(),
            scratch.tables.capacities()[1],
            scratch.tables.capacities()[2],
            scratch.tables.capacities()[3],
            scratch.keep_context.lines_by_y.capacity(),
            scratch.range_lines.capacity(),
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
