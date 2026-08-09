use html_view_doc::TextGeometry;
use crate::RendererCore;
use kurbo::Point;

impl RendererCore {
    pub(crate) fn text_geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.frame, &self.layout)
    }

    pub fn glyph_x_span_in_line(&self, line: &html::layout::RenderLine, start: u32, end: u32) -> (f64, f64) {
        self.text_geometry().glyph_x_span_in_line(line, start, end)
    }

    pub fn hit_test_glyph(&self, pos: Point) -> Option<u32> {
        self.text_geometry().hit_test_glyph(pos)
    }

    pub fn hit_test_image(&self, pos: Point) -> Option<u32> {
        self.text_geometry().hit_test_image(pos)
    }



    pub fn selection_range(&self) -> Option<(u32, u32)> {
        // normalize selection anchor/active into a range
        let (Some(a), Some(b)) = (self.selection.selection_anchor, self.selection.selection_active) else {
            return None;
        };
        if a == b {
            return None;
        }
        let (start, end) = if a < b { (a, b) } else { (b, a) }; // normalize range order
        Some((start, end))
    }

    pub fn update_selection_text(&mut self) {
        // refresh selection buffers based on current range
        let Some((start, end)) = self.selection_range() else {
            self.selection.selected_text = None;
            self.selection.selected_text_markdown = None;
            return;
        };
        let (plain, ordinary_markdown) = self.doc().build_selection_text(start, end);
        let markdown = if self.selection.semantic_selection { self.build_semantic_selection_text(start, end).1 } else { ordinary_markdown };
        self.selection.selected_text = plain;
        self.selection.selected_text_markdown = markdown;
    }

    /// Returns whether a point intersects the selection as it is painted.
    pub fn selection_contains_point(&self, position: Point) -> bool {
        let Some((selection_start, selection_end)) = self.selection_range() else {
            return false;
        };
        if self.semantic_selection_contains_point(position, selection_start, selection_end) {
            return true;
        }
        let promoted = self.semantic_promoted_glyph_ranges(selection_start, selection_end);
        for (line_idx, screen) in self.frame.last_line_positions.iter() {
            let Some(line) = self.doc().text().line(line_idx) else {
                continue;
            };
            if position.y < screen.point.y || position.y > screen.point.y + line.height() {
                continue;
            }

            let mut ranges = self.doc().line_text_intersections(line_idx, selection_start, selection_end).collect::<Vec<_>>();
            for range in &promoted {
                ranges.extend(self.doc().line_text_intersections(line_idx, range.start, range.end));
            }
            for (start, end) in ranges {
                let (x0, x1) = self.glyph_x_span_in_line(&line, start, end);
                let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };

                if position.x >= x0 && position.x <= x1 {
                    return true;
                }
            }
        }
        false
    }

}
