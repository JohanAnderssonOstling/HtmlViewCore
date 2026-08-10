use html_view_doc::TextGeometry;
use crate::RendererCore;
use kurbo::Point;

impl RendererCore {
    pub(crate) fn text_geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.page.frame, &self.page.layout)
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



}
