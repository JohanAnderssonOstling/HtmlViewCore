use crate::BoundaryAnchors;
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
    lines_by_y: &mut Vec<(f64, usize)>,
    range_lines: &mut Vec<usize>,
    child_pairs: &mut Vec<(Option<usize>, usize)>,
) {
    ranges.clear();
    let root = doc.view();
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
    let fill_lines_in_range = |top: f64, bottom: f64, indices: &mut Vec<usize>| {
        indices.clear();
        let start = lines_by_y.partition_point(|(y, _)| *y < top - EPSILON);
        let end = lines_by_y.partition_point(|(y, _)| *y < bottom - EPSILON);
        indices.extend(lines_by_y[start..end].iter().map(|(_, line_idx)| *line_idx));
        indices.sort_unstable();
    };

    child_pairs.clear();
    child_pairs.extend(
        anchors
            .boxes()
            .map(|box_idx| (boxes.parent(box_idx), box_idx)),
    );
    child_pairs.sort_unstable();

    for box_idx in anchors.boxes() {
        let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
            continue;
        };
        let bottom = point.y + size.height;
        fill_lines_in_range(point.y, bottom, range_lines);
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
        } else { None };
        let Some(limit) = compact_line_limit else {
            continue;
        };
        let has_forced_inside = anchors.boxes().any(|descendant| {
            descendant != box_idx
                && doc.view().semantics().contains(box_idx, descendant)
                && (boxes.forces_break_before(descendant) || boxes.forces_break_after(descendant))
        });
        if range_lines.len() <= limit
            && !has_forced_inside
            && let Some(trigger_line) = anchors.before(box_idx)
        {
            ranges.push((trigger_line, point.y, bottom));
        }
    }

    let mut group_start = 0;
    while group_start < child_pairs.len() {
        let parent = child_pairs[group_start].0;
        let group_end = child_pairs[group_start..]
            .partition_point(|(candidate, _)| *candidate == parent)
            + group_start;
        for pair in child_pairs[group_start..group_end].windows(2) {
            let label = pair[0].1;
            let content = pair[1].1;
            let is_definition_pair = root.semantics().content(label).has_role(ContentRole::DefinitionTerm)
                && root.semantics().content(content).has_role(ContentRole::DefinitionDescription);
            let is_statement_label_pair = boxes.parent(label).is_some_and(|parent| {
                boxes.parent(content) == Some(parent) && root.semantics().content(parent).has_role(ContentRole::Statement)
            }) && root.semantics().content(label).has_role(ContentRole::Label);
            if !is_definition_pair && !is_statement_label_pair {
                continue;
            }
            if boxes.forces_break_after(label) || boxes.forces_break_before(content) {
                continue;
            }
            let (Some(label_point), Some(content_point), Some(content_size)) = (
                boxes.point(label),
                boxes.point(content),
                boxes.size(content),
            ) else {
                continue;
            };
            fill_lines_in_range(
                content_point.y,
                content_point.y + content_size.height,
                range_lines,
            );
            let required_bottom = range_lines
                .iter()
                .take(2)
                .filter_map(|&line_idx| {
                    lines
                        .get(line_idx)
                        .map(|line| line.point().y + line.height())
                })
                .fold(label_point.y, f64::max);
            if let Some(trigger_line) = anchors.before(label)
                && required_bottom > label_point.y + EPSILON
            {
                ranges.push((trigger_line, label_point.y, required_bottom));
            }
        }
        group_start = group_end;
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
