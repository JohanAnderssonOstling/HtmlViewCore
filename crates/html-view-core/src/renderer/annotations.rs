use html_view_doc::DocQuery;
use crate::RendererCore;

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.set_visible(None);
        let Some(href) = self.media_overlay.target_href().map(str::to_owned) else { return };
        let Some((doc, Some(fragment))) = self.resolve_href_target(&href) else { return };
        if doc != self.nav.current_doc_index {
            return;
        }
        let range = self.doc().glyph_range_for_anchor(&fragment);
        self.media_overlay.set_visible(range);
    }

    pub(crate) fn activate_annotation_at_glyph(&self, glyph: u32) -> bool {
        self.annotations.activate_at_glyph(glyph, self.host.as_ref())
    }

    pub(crate) fn resolve_visible_annotations(&mut self) {
        let current_doc = self.nav.current_doc_index;
        let doc = DocQuery::new(&self.page.view.document);
        self.annotations.resolve(doc, current_doc);
    }

}
