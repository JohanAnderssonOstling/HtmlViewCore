//! Spine-document acquisition and activation.

use super::*;

impl RendererCore {
    pub(super) fn spine_item(&self, loaded: load::LoadedRenderDocument) -> SpineItem {
        SpineItem::new(loaded, self.page.layout.clone(), self.image_service.clone())
    }

    /// Makes `item` the page. Everything addressed by a document -- its
    /// pipeline, its images, its glyph registry -- arrives with it, which is
    /// the whole point of their being one thing.
    pub(super) fn activate_spine_item(&mut self, doc_index: usize, mut item: SpineItem) {
        // The column layout belongs to the reader rather than to the document
        // arriving, so it stays behind.
        item.layout = self.page.layout.clone();
        self.page = item;
        self.nav.activate_document(doc_index);
        self.root_font_size = self.page.root_font_size();
        self.revisions
            .invalidate_pipeline_from(html::pipeline::EarliestStage::Parse);
        self.revisions.invalidate_resources();
        self.reset_view_state();
        self.host.emit(RendererEvent::TitleChanged(
            self.page.render_view().title().map(str::to_owned),
        ));
        let view = NavView {
            doc: DocQuery::from_view(self.page.render_view()),
            viewport: &self.viewport,
            frame: &self.page.frame,
        };
        self.nav.update_toc_anchor_filter(view);
        self.resolve_visible_annotations();
        self.resolve_media_overlay_highlight();
    }

    pub(crate) fn load_document_for_index(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
    ) -> load::LoadedRenderDocument {
        if self
            .prepared_navigation
            .as_ref()
            .is_some_and(|(index, _)| *index == doc_index)
        {
            return self.prepared_navigation.take().unwrap().1;
        }
        // Shaping fills this document's glyph registry, and ids only mean
        // something alongside the document they came from.
        self.host.set_glyph_document(doc_index);
        let uri = self.nav.documents().uris()[doc_index].clone();
        let mut prepared = load::prepare_document_with_settings(
            self.provider.clone(),
            &uri,
            self.document_settings(),
        )
        .expect("document preparation should succeed");
        self.host
            .set_font_overrides(&prepared.inputs().expect("prepared inputs").reader_overrides);
        self.host
            .prepare_font_faces(prepared.font_faces(), self.provider.clone());
        prepared
            .rehydrate_glyphs(glyph_shaper)
            .expect("document shaping should succeed");
        prepared
    }

    pub fn load_document_at(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
        anchor: DocAnchor,
    ) {
        if doc_index >= self.nav.documents().uris().len() {
            return;
        }

        if self.defer_document_read(doc_index, NavigationRequest::Document(doc_index, anchor)) {
            return;
        }

        if doc_index == self.nav.location().document() {
            let (nav, mut cx) = self.nav_cx();
            nav.finish_document_navigation(&mut cx, anchor);
            return;
        }

        let loaded = self.load_document_for_index(glyph_shaper, doc_index);
        let item = self.spine_item(loaded);
        self.activate_spine_item(doc_index, item);

        let (nav, mut cx) = self.nav_cx();
        nav.finish_document_navigation(&mut cx, anchor);
    }
}
