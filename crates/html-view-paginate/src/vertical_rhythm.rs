use crate::{Paginator, VerticalRhythmScratch};
use html_view_types::PagePositions;

const MAX_GAP_STRETCH: f64 = 2.0;
const MAX_COLUMN_STRETCH: f64 = 12.0;
const EPSILON: f64 = 0.01;

impl Paginator<'_> {
    /// Refines an already-paginated page without changing CSS layout or line
    /// breaking. Only positive gaps between line boxes are elastic, and each
    /// receives at most a two-pixel adjustment. Pagination, tables, and keep
    /// ranges remain authoritative.
    pub(super) fn apply_vertical_rhythm(
        &self,
        page: &mut PagePositions,
        scratch: &mut VerticalRhythmScratch,
    ) {
        let lines = self.doc.text().lines();
        let layout = self.layout;
        scratch.visible.clear();
        scratch.visible.extend(page.positions.iter());
        let Some(last_visible_column) = scratch
            .visible
            .iter()
            .map(|(_, screen)| screen.col_index)
            .max()
        else {
            return;
        };

        for column in 0..=last_visible_column {
            // The terminal document column is intentionally left ragged. There
            // is no following fragmentainer whose baseline edge it should meet.
            if page.reached_end && column == last_visible_column {
                continue;
            }

            scratch.column_lines.clear();
            scratch.column_lines.extend(
                scratch
                    .visible
                    .iter()
                    .copied()
                    .filter(|(_, screen)| screen.col_index == column),
            );
            let Some(&(last_idx, last_screen)) = scratch.column_lines.last() else {
                continue;
            };
            let Some(last_line) = lines.get(last_idx) else {
                continue;
            };
            let last_height = self.doc.effective_line_height(last_idx, last_line.height());
            let slack = (layout.size.height - (last_screen.point.y + last_height)).max(0.0);
            if slack <= EPSILON {
                continue;
            }

            scratch.candidates.clear();
            for pair in scratch.column_lines.windows(2) {
                let [(current_idx, _), (next_idx, _)] = pair else {
                    continue;
                };
                let (Some(current), Some(next)) = (lines.get(*current_idx), lines.get(*next_idx))
                else {
                    continue;
                };
                let current_height = self
                    .doc
                    .effective_line_height(*current_idx, current.height());
                if next.point().y - (current.point().y + current_height) <= EPSILON {
                    continue;
                }

                let trigger = next.point().y;
                if page
                    .offset_breaks
                    .iter()
                    .any(|(y, _)| (*y - trigger).abs() <= EPSILON)
                    || scratch.rigid_ranges.iter().any(|(top, bottom)| {
                        trigger > *top + EPSILON && trigger < *bottom - EPSILON
                    })
                    || self
                        .doc
                        .view()
                        .fragments()
                        .decorations_for_line(*next_idx)
                        .iter()
                        .any(|decoration| {
                            !decoration.is_inline()
                                && decoration.border_width().is_some()
                                && trigger > decoration.rect().y0 + EPSILON
                                && trigger < decoration.rect().y1 - EPSILON
                        })
                {
                    continue;
                }
                scratch.candidates.push((*next_idx, trigger));
            }
            if scratch.candidates.is_empty() {
                continue;
            }

            let total = slack
                .min(MAX_COLUMN_STRETCH)
                .min(scratch.candidates.len() as f64 * MAX_GAP_STRETCH);
            let per_gap = total / scratch.candidates.len() as f64;
            scratch.original_breaks.clear();
            scratch
                .original_breaks
                .extend_from_slice(&page.offset_breaks);
            let mut cumulative = 0.0;
            for &(next_idx, trigger) in &scratch.candidates {
                cumulative += per_gap;
                for line_idx in next_idx..=last_idx {
                    if let Some(screen) = page.positions.get_mut(line_idx)
                        && screen.col_index == column
                    {
                        screen.point.y += per_gap;
                    }
                }
                let base_offset = scratch
                    .original_breaks
                    .iter()
                    .take_while(|(y, _)| *y <= trigger)
                    .last()
                    .map_or(0.0, |(_, offset)| *offset);
                page.offset_breaks.push((trigger, base_offset + cumulative));
            }
        }
        page.offset_breaks
            .sort_by(|left, right| left.0.total_cmp(&right.0));
    }
}
