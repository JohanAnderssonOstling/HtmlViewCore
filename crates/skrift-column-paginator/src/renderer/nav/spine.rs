//! Spine-document acquisition and activation.

use super::*;
use crate::{FontLoadRequest, RendererEffects, SearchOptions};
use html::{Document, PreparedDocument};
use std::mem::replace;

impl RendererCore {
    /// Replace publication-specific inputs without recreating the reader core.
    pub(crate) fn replace_document(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        prepared: RendererPreparation,
    ) -> Result<RendererPreparation, html::Error> {
        let update = prepared.document_update();
        let loaded = prepared
            .loaded
            .finish(update, glyph_shaper)
            ?;
        let previous_config = RendererInitialConfig {
            font_size: self.page.root_font_size(),
            column_width: self.page.configuration().width(),
            scale: self.page.layout.scale,
            max_column_count: self.page.layout.max_col_count,
            style_overrides: self.page.configuration().reader_overrides().clone(),
            image_sizing_policy: self.page.configuration().image_sizing_policy(),
            text_composition_policy: self.page.configuration().text_composition_policy(),
            note_display: self.note_display,
            vertical_rhythm: self.vertical_rhythm,
            paint_palette: self.reader_palette,
            interaction_palette: self.interaction_palette,
            ..Default::default()
        };
        let config = prepared.config;
        let provider = replace(&mut self.provider, prepared.provider);
        let nav = replace(
            &mut self.nav,
            NavigationState::new(prepared.document_uris, prepared.doc_index),
        );
        self.nav.cfi = crate::cfi::PublicationCfi::new(config.publication_cfi_bases);
        self.image_service = Self::create_image_service(
            &self.services,
            self.provider.clone(),
            prepared.image_loader,
        );
        self.pending_navigation = None;
        self.prepared_navigation = None;
        self.pending_file_events.clear();
        self.effects = RendererEffects::default();
        self.annotations = Default::default();
        self.media_overlay = Default::default();
        self.highlight
            .restart(config.search_query, SearchOptions::default());
        if config.search_active {
            self.highlight.activate();
        } else {
            self.highlight.deactivate();
        }
        self.note_display = config.note_display;
        self.vertical_rhythm = config.vertical_rhythm;
        self.reader_palette = config.paint_palette;
        self.interaction_palette = config.interaction_palette;
        let viewport = self.page.layout.size * self.page.layout.scale;
        let width = config.column_width.clamp(200.0, 1200.0).round();
        self.page.layout.scale = config.scale.clamp(0.5, 3.0);
        self.page.layout.max_col_count = config.max_column_count.map(|count| count.max(1));
        self.page.layout.commit_column_width(Some(width), Some(width));
        self.page.layout.configure_viewport(viewport);

        let previous = self.install_document(loaded, prepared.doc_index);
        self.effects.font_loads.push(FontLoadRequest {
            faces: self.page.session.font_faces(),
            use_publication_fonts: self
                .page
                .configuration()
                .reader_overrides()
                .uses_publication_fonts(),
            provider: self.provider.clone(),
        });
        self.restore_initial_position(
            glyph_shaper,
            prepared.initial_anchor.as_deref(),
            prepared.nav_state.as_deref(),
            prepared.glyph_index,
        );
        // Article history already recorded the jump from the previous document.
        self.nav.cancel_jump();
        Ok(RendererPreparation {
            loaded: previous.into_preparation(),
            provider,
            image_loader: None,
            document_uris: nav.document_uris().to_vec(),
            doc_index: nav.current_document(),
            glyph_index: 0,
            nav_state: None,
            initial_anchor: None,
            config: previous_config,
            font_revision: None,
            refresh_fonts: false,
        })
    }

    pub(crate) fn load_document_for_index(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
    ) -> Result<Document, html::Error> {
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
    ) -> Result<Document, html::Error> {
        self.effects.font_loads.push(FontLoadRequest {
            faces: prepared.font_faces(),
            use_publication_fonts: prepared
                .configuration()
                .reader_overrides()
                .uses_publication_fonts(),
            provider: self.provider.clone(),
        });
        prepared
            .finish(DocumentUpdate::default(), glyph_shaper)

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
        self.install_document(loaded, doc_index);
        self.effects.emit(RendererEvent::TitleChanged(
            self.page.render_view().title().map(str::to_owned),
        ));

        let (nav, mut cx) = self.nav_cx();
        nav.finish_document_navigation(&mut cx, anchor);
    }

    /// Chapters and standalone documents share installation and page invalidation.
    fn install_document(&mut self, mut loaded: Document, doc_index: usize) -> Document {
        loaded.set_image_service(self.image_service.clone());
        let previous = replace(&mut self.page.session, loaded);
        self.nav.activate_document(doc_index);
        self.revisions.replace_document();
        self.revisions.invalidate_resources();
        self.reset_view_state();
        self.effects.request_repaint();
        self.resolve_visible_annotations();
        self.resolve_media_overlay_highlight();
        self.refresh_visible_search_matches();

        previous
    }
}
