use crate::RendererCore;
use html::targets::ElementTarget;

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.set_visible(None);
        let Some(href) = self.media_overlay.target_href().map(str::to_owned) else {
            return;
        };
        let Some((doc, Some(fragment))) = self.resolve_href_target(&href) else {
            return;
        };
        if doc != self.nav.current_document() {
            return;
        }
        let range = self
            .doc()
            .addressing()
            .element(&fragment)
            .and_then(|target| match target {
                ElementTarget::Text { glyphs, .. } => Some(glyphs),
                _ => None,
            });
        self.media_overlay.set_visible(range);
    }

    pub(crate) fn resolve_visible_annotations(&mut self) {
        let current_doc = self.nav.current_document();
        let doc = self.page.render_view();
        self.annotations
            .resolve_with_cfi(doc, current_doc, &self.nav.cfi);
    }
}
