use crate::RendererCore;
use html_view_doc::TextGeometry;
use kurbo::Rect;

impl RendererCore {
    pub(crate) fn text_geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.page.view.frame, &self.page.view.layout)
    }

    /// The box the current selection occupies on the page.
    ///
    /// For a caller that has to place something beside the passage rather than
    /// over it: the box is where the words are, whereas the pointer position a
    /// selection ends at is only its last line.
    pub(crate) fn selection_bounds(&self) -> Option<Rect> {
        let interaction = self.page.view.selection.interaction();
        let (Some(anchor), Some(active)) = (interaction.anchor(), interaction.active()) else {
            return None;
        };
        let (start, end) = if anchor <= active { (anchor, active) } else { (active, anchor) };
        self.painted_bounds(self.text_geometry().glyph_range_bounds(start, end))
    }

    /// The same, for an annotation already on the page.
    pub(crate) fn annotation_bounds(&self, id: &str) -> Option<Rect> {
        let (start, end) = self.annotations.glyph_range(id)?;
        self.painted_bounds(self.text_geometry().glyph_range_bounds(start, end))
    }

    /// Layout-space geometry as it is actually drawn, so a caller placing a
    /// panel against it is working in the same coordinates as the surface.
    fn painted_bounds(&self, bounds: Option<Rect>) -> Option<Rect> {
        let bounds = bounds?;
        let top_left = self.from_layout_point(kurbo::Point::new(bounds.x0, bounds.y0));
        let bottom_right = self.from_layout_point(kurbo::Point::new(bounds.x1, bounds.y1));
        Some(Rect::new(top_left.x, top_left.y, bottom_right.x, bottom_right.y))
    }
}
