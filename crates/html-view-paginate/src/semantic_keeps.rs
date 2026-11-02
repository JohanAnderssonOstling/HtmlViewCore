use crate::{BoundaryAnchors, KeepContext};
use html_view_doc::DocQuery;

use html::layout::ContentRole;

const EPSILON: f64 = 0.01;

/// Returns soft keep ranges for compact book structures. These are
/// pagination preferences, not layout constraints: the normal keep event
/// path discards any range taller than a fragmentainer.
pub(crate) fn semantic_keep_ranges(
    doc: DocQuery<'_>,
    anchors: &BoundaryAnchors,
    ranges: &mut Vec<(usize, f64, f64)>,
    context: &KeepContext,
    range_lines: &mut Vec<usize>,
) {
    ranges.clear();
    let root = doc.view();
    let boxes = root.boxes();
    let lines = root.pagination_lines();
    for &(box_idx, ref constraints) in &context.constraints {
        let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
            continue;
        };
        let bottom = point.y + size.height;
        context.lines_in_range(point.y, bottom, range_lines);
        if range_lines.is_empty() {
            continue;
        }
        let semantics = root.semantics().content(box_idx);
        let compact_line_limit = if semantics.has_role(ContentRole::ListItem) {
            Some(4)
        } else if semantics.has_role(ContentRole::CodeBlock) {
            Some(12)
        } else if semantics.has_role(ContentRole::Quote) {
            Some(6)
        } else if semantics.has_role(ContentRole::CompactStructure) {
            Some(8)
        } else {
            None
        };
        let Some(limit) = compact_line_limit else {
            continue;
        };
        let has_forced_inside = constraints.has_forced_descendant;
        if range_lines.len() <= limit
            && !has_forced_inside
            && let Some(trigger_line) = anchors.before(box_idx)
        {
            ranges.push((trigger_line, point.y, bottom));
        }
    }

    for pair in &context.siblings {
        if (!pair.definition_pair && !pair.statement_label_pair) || pair.forced_boundary {
            continue;
        }
        let label = pair.previous;
        let content = pair.next;
        let (Some(label_point), Some(content_point), Some(content_size)) = (
            boxes.point(label),
            boxes.point(content),
            boxes.size(content),
        ) else {
            continue;
        };
        context.lines_in_range(
            content_point.y,
            content_point.y + content_size.height,
            range_lines,
        );
        let required_bottom = range_lines
            .iter()
            .take(2)
            .filter_map(|&line_idx| {
                lines
                    .at(line_idx)
                    .map(|line| line.point().y + line.height())
            })
            .fold(label_point.y, f64::max);
        if let Some(trigger_line) = anchors.before(label)
            && required_bottom > label_point.y + EPSILON
        {
            ranges.push((trigger_line, label_point.y, required_bottom));
        }
    }

    ranges.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.total_cmp(&right.1))
            .then_with(|| right.2.total_cmp(&left.2))
    });
    ranges.dedup_by(|left, right| {
        left.0 == right.0
            && (left.1 - right.1).abs() <= EPSILON
            && (left.2 - right.2).abs() <= EPSILON
    });
}
