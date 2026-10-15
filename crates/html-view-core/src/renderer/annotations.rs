use crate::RendererCore;
use html_view_doc::DocQuery;

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.set_visible(None);
        if let Some(target) = self.media_overlay.reading_target() {
            let ranges = target.glyph_ranges.clone();
            self.media_overlay.set_reading_ranges(ranges);
            return;
        }
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
