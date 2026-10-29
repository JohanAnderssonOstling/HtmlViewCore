//! Reader page projection. This module places source geometry; the engine owns
//! the meaning and drawing order of every HTML paint item.

use html::render::{PaintFragment, PaintProjection, ProjectedLine};
use html_view_doc::TextGeometry;
use kurbo::{Point, Rect};

pub(crate) struct PageProjection<'a>(pub TextGeometry<'a>);

impl PaintProjection for PageProjection<'_> {
    fn candidates(
        &self,
        view: html::layout::RenderView<'_>,
        lines: &mut Vec<usize>,
        decorations: &mut Vec<usize>,
    ) -> bool {
        let Some(blocks) = self.0.frame.page().block_decoration_indices() else {
            return false;
        };
        lines.extend(
            self.0
                .frame
                .page()
                .line_positions()
                .iter()
                .map(|(line, _)| line),
        );
        decorations.extend_from_slice(blocks);
        let mut header_decorations = Vec::new();
        for header in self.0.frame.page().repeated_table_headers() {
            lines.extend(
                view.text()
                    .lines()
                    .iter()
                    .filter(|line| {
                        let source = line.point();
                        source.x >= header.source_left - 0.01
                            && source.x < header.source_right + 0.01
                            && source.y >= header.source_top - 0.01
                            && source.y < header.source_bottom - 0.01
                    })
                    .map(|line| line.index()),
            );
            view.fragments().block_decoration_indices_intersecting(
                header.source_top,
                header.source_bottom,
                &mut header_decorations,
            );
            decorations.extend_from_slice(&header_decorations);
        }
        true
    }
    fn bounds(&self) -> Rect {
        Rect::new(
            0.0,
            0.0,
            self.0.layout.size.width.max(0.0),
            self.0.layout.size.height.max(0.0),
        )
    }

    fn project_line(&self, line: usize, source: Point, visit: &mut dyn FnMut(ProjectedLine)) {
        if let Some(screen) = self.0.frame.page().line_positions().get(line) {
            visit(ProjectedLine {
                origin: screen.point,
                clip: None,
            });
        }
        for header in self.0.frame.page().repeated_table_headers() {
            if source.x >= header.source_left - 0.01
                && source.x < header.source_right + 0.01
                && source.y >= header.source_top - 0.01
                && source.y < header.source_bottom - 0.01
            {
                visit(ProjectedLine {
                    origin: Point::new(
                        self.0.layout.col_x(header.col_index as f64, source.x),
                        source.y - header.source_top,
                    ),
                    clip: Some(Rect::new(
                        self.0
                            .layout
                            .col_x(header.col_index as f64, header.source_left),
                        0.0,
                        self.0
                            .layout
                            .col_x(header.col_index as f64, header.source_right),
                        header.source_bottom - header.source_top,
                    )),
                });
            }
        }
    }

    fn project_rect(
        &self,
        source: Rect,
        line: Option<usize>,
        visit: &mut dyn FnMut(PaintFragment),
    ) {
        self.0.project_rect(source, line, true, visit);
    }
}
