//! Spine-document acquisition and activation.

use super::*;
use html::{Document, PreparedDocument};

impl RendererCore {
    pub(crate) fn load_document_for_index(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
    ) -> Result<Document, String> {
        if self
            .prepared_navigation
            .as_ref()
            .is_some_and(|(index, _)| *index == doc_index)
        {
            return Ok(self.prepared_navigation.take().unwrap().1);
        }
        // Shaping fills this document's glyph registry, and ids only mean
        // something alongside the document they came from.

        let uri = self.nav.document_uris()[doc_index].clone();
        let prepared = load::prepare_document_with_settings(
            self.provider.clone(),
            &uri,
            self.document_settings(),
        )?;
        self.finish_prepared_document(glyph_shaper, prepared)
    }

    pub(super) fn finish_prepared_document(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        prepared: PreparedDocument,
    ) -> Result<Document, String> {
        self.effects.font_loads.push(crate::FontLoadRequest {
            faces: prepared.font_faces(),
            use_publication_fonts: prepared
                .configuration()
                .reader_overrides()
                .uses_publication_fonts(),
            provider: self.provider.clone(),
        });
        prepared
            .finish(DocumentUpdate::default(), glyph_shaper)
            .map_err(|error| error.to_string())
    }

    pub fn load_document_at(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
        anchor: DocAnchor,
    ) {
        if doc_index >= self.nav.document_uris().len() {
            return;
        }

        if self.defer_document_read(doc_index, NavigationRequest::Document(doc_index, anchor)) {
            return;
        }

        if doc_index == self.nav.current_document() {
            let (nav, mut cx) = self.nav_cx();
            nav.finish_document_navigation(&mut cx, anchor);
            return;
        }

        let loaded = match self.load_document_for_index(glyph_shaper, doc_index) {
            Ok(loaded) => loaded,
            Err(message) => {
                self.emit_operation_failed(crate::RendererOperation::UpdateDocument, message);
                return;
            }
        };
        // Keep the reader's column layout when replacing the active chapter.
        self.page = SpineItem::new(loaded, self.page.layout.clone(), self.image_service.clone());
        self.nav.activate_document(doc_index);
        self.revisions.replace_document();
        self.revisions.invalidate_resources();
        self.reset_view_state();
        self.effects.emit(RendererEvent::TitleChanged(
            self.page.render_view().title().map(str::to_owned),
        ));
        self.resolve_visible_annotations();
        self.resolve_media_overlay_highlight();
        self.refresh_visible_search_matches();

        let (nav, mut cx) = self.nav_cx();
        nav.finish_document_navigation(&mut cx, anchor);
    }
}
