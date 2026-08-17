use crate::RendererCore;
use html_view_doc::DocQuery;

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.set_visible(None);
        let Some(href) = self.media_overlay.target_href().map(str::to_owned) else {
            return;
        };
        let Some((doc, Some(fragment))) = self.resolve_href_target(&href) else {
            return;
        };
        if doc != self.nav.location().document() {
            return;
        }
        let range = self.doc().glyph_range_for_anchor(&fragment);
        self.media_overlay.set_visible(range);
    }

    pub(crate) fn resolve_visible_annotations(&mut self) {
        let current_doc = self.nav.location().document();
        let doc = DocQuery::new(&self.page.view.document);
        self.annotations.resolve(doc, current_doc);
    }
}
