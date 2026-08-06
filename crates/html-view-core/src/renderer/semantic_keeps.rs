use crate::{RendererCore, render::BoundaryAnchors};

const EPUB_NAMESPACE: &str = "http://www.idpf.org/2007/ops";
const EPSILON: f64 = 0.01;

fn has_token(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| value.split_ascii_whitespace().any(|token| token.eq_ignore_ascii_case(expected) || token.rsplit_once(':').is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case(expected))))
}

fn has_epub_type(boxes: html::layout::RenderBoxView<'_>, box_idx: usize, expected: &str) -> bool {
    has_token(boxes.attribute(box_idx, "epub:type").or_else(|| boxes.attribute_expanded(box_idx, Some(EPUB_NAMESPACE), "type")), expected)
}

fn has_any_epub_type(boxes: html::layout::RenderBoxView<'_>, box_idx: usize, expected: &[&str]) -> bool {
    expected.iter().any(|kind| has_epub_type(boxes, box_idx, kind))
}

fn has_any_role(boxes: html::layout::RenderBoxView<'_>, box_idx: usize, expected: &[&str]) -> bool {
    expected.iter().any(|role| has_token(boxes.attribute(box_idx, "role"), role))
}

fn is_compact_semantic_structure(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    has_any_epub_type(boxes, box_idx, &["epigraph", "poem", "verse", "stanza", "biblioentry", "dedication", "abstract", "notice", "pullquote", "theorem", "proof", "example", "exercise"])
        || has_any_role(boxes, box_idx, &["doc-epigraph", "doc-biblioentry", "doc-dedication", "doc-abstract", "doc-notice", "doc-pullquote", "doc-example"])
}

fn is_semantic_statement(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    has_any_epub_type(boxes, box_idx, &["theorem", "proof", "example", "exercise"]) || has_any_role(boxes, box_idx, &["doc-example"])
}

fn is_semantic_label(boxes: html::layout::RenderBoxView<'_>, box_idx: usize) -> bool {
    has_epub_type(boxes, box_idx, "label") || boxes.tag(box_idx).is_some_and(|tag| ["h1", "h2", "h3", "h4", "h5", "h6"].iter().any(|heading| tag.eq_ignore_ascii_case(heading)))
}

impl RendererCore {
    /// Returns soft keep ranges for compact book structures. These are
    /// pagination preferences, not layout constraints: the normal keep event
    /// path discards any range taller than a fragmentainer.
    pub(super) fn semantic_keep_ranges(&self, anchors: &BoundaryAnchors) -> Vec<(usize, f64, f64)> {
        let root = self.document.render_view();
        let boxes = root.boxes();
        let lines = root.text().lines();
        let mut lines_by_y = lines.iter().enumerate().map(|(line_idx, line)| (line.point().y, line_idx)).collect::<Vec<_>>();
        lines_by_y.sort_by(|left, right| left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let lines_in_range = |top: f64, bottom: f64| {
            let start = lines_by_y.partition_point(|(y, _)| *y < top - EPSILON);
            let end = lines_by_y.partition_point(|(y, _)| *y < bottom - EPSILON);
            let mut indices = lines_by_y[start..end].iter().map(|(_, line_idx)| *line_idx).collect::<Vec<_>>();
            indices.sort_unstable();
            indices
        };

        let mut children = vec![Vec::new(); boxes.len() + 1];
        for box_idx in 0..boxes.len() {
            children[boxes.parent(box_idx).unwrap_or(boxes.len())].push(box_idx);
        }

        let mut ranges = Vec::new();
        for box_idx in 0..boxes.len() {
            let (Some(point), Some(size)) = (boxes.point(box_idx), boxes.size(box_idx)) else {
                continue;
            };
            let bottom = point.y + size.height;
            let block_lines = lines_in_range(point.y, bottom);
            if block_lines.is_empty() {
                continue;
            }
            let tag = boxes.tag(box_idx);
            let compact_line_limit = if tag.is_some_and(|tag| tag.eq_ignore_ascii_case("li")) {
                Some(4)
            } else if tag.is_some_and(|tag| tag.eq_ignore_ascii_case("pre") || (tag.eq_ignore_ascii_case("code") && boxes.is_block_container(box_idx))) {
                Some(12)
            } else if tag.is_some_and(|tag| tag.eq_ignore_ascii_case("blockquote")) {
                Some(6)
            } else if is_compact_semantic_structure(boxes, box_idx) {
                Some(8)
            } else {
                None
            };
            let Some(limit) = compact_line_limit else {
                continue;
            };
            let has_forced_inside = (box_idx + 1..=anchors.subtree_end[box_idx]).any(|descendant| boxes.forces_break_before(descendant) || boxes.forces_break_after(descendant));
            if block_lines.len() <= limit
                && !has_forced_inside
                && let Some(trigger_line) = anchors.before(box_idx)
            {
                ranges.push((trigger_line, point.y, bottom));
            }
        }

        for siblings in children {
            for pair in siblings.windows(2) {
                let label = pair[0];
                let content = pair[1];
                let is_definition_pair = boxes.tag(label).is_some_and(|tag| tag.eq_ignore_ascii_case("dt")) && boxes.tag(content).is_some_and(|tag| tag.eq_ignore_ascii_case("dd"));
                let is_statement_label_pair = boxes.parent(label).is_some_and(|parent| boxes.parent(content) == Some(parent) && is_semantic_statement(boxes, parent)) && is_semantic_label(boxes, label);
                if !is_definition_pair && !is_statement_label_pair {
                    continue;
                }
                if boxes.forces_break_after(label) || boxes.forces_break_before(content) {
                    continue;
                }
                let (Some(label_point), Some(content_point), Some(content_size)) = (boxes.point(label), boxes.point(content), boxes.size(content)) else {
                    continue;
                };
                let content_lines = lines_in_range(content_point.y, content_point.y + content_size.height);
                let required_bottom = content_lines.iter().take(2).filter_map(|&line_idx| lines.get(line_idx).map(|line| line.point().y + line.height())).fold(label_point.y, f64::max);
                if let Some(trigger_line) = anchors.before(label)
                    && required_bottom > label_point.y + EPSILON
                {
                    ranges.push((trigger_line, label_point.y, required_bottom));
                }
            }
        }

        ranges.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.total_cmp(&right.1)).then_with(|| right.2.total_cmp(&left.2)));
        ranges.dedup_by(|left, right| left.0 == right.0 && (left.1 - right.1).abs() <= EPSILON && (left.2 - right.2).abs() <= EPSILON);
        ranges
    }
}
