use crate::RendererCore;
use html_view_doc::TextGeometry;

impl RendererCore {
    pub(crate) fn text_geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.page.view.frame, &self.page.view.layout)
    }
}
