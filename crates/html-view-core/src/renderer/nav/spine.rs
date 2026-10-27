//! Spine-document acquisition, caching, and activation.

use super::*;
use html::pipeline::{PipelineChanges, PipelineInputs};

impl RendererCore {
    pub(super) fn spine_item(&self, loaded: load::LoadedRenderDocument) -> SpineItem {
        SpineItem::new(
            loaded,
            self.page.view.layout.clone(),
            self.image_service.clone(),
        )
    }

    /// Makes `item` the page. Everything addressed by a document -- its
    /// pipeline, its images, its glyph registry -- arrives with it, which is
    /// the whole point of their being one thing.
    pub(super) fn activate_spine_item(&mut self, doc_index: usize, mut item: SpineItem) {
        // The column layout belongs to the reader rather than to the document
        // arriving, so it stays behind.
        item.view.layout = self.page.view.layout.clone();
        let previous_index = self.nav.location().document();
        let previous = mem::replace(&mut self.page, item);
        // The replacement is ready before the old session leaves the page.
        // Moving the entire item preserves its document and resource identity.
        if previous_index != doc_index {
            self.document_cache
                .insert_document(previous_index, previous);
        }
        self.nav.activate_document(doc_index);
        self.root_font_size = self.page.root_font_size();
        self.revisions
            .invalidate_pipeline_from(html::pipeline::EarliestStage::Parse);
        self.revisions.invalidate_resources();
        self.reset_view_state();
        self.host.emit(RendererEvent::TitleChanged(
            self.page
                .view
                .document
                .render_view()
                .title()
                .map(str::to_owned),
        ));
        let view = NavView {
            doc: DocQuery::from_view(self.page.view.document.render_view()),
            viewport: &self.viewport,
            frame: &self.page.view.frame,
        };
        self.nav.update_toc_anchor_filter(view);
        self.resolve_visible_annotations();
        self.resolve_media_overlay_highlight();
    }

    pub(crate) fn changes_for_cached_document(
        &self,
        doc_index: usize,
        source_inputs: &PipelineInputs,
    ) -> PipelineChanges {
        let current = self.page.inputs();
        let mut changes = PipelineChanges::default();
        if let Some(uri) = self.nav.documents().uris().get(doc_index) {
            if source_inputs.base_uri != *uri {
                changes.base_uri = Some(uri.clone());
            }
            let syntax = load::markup_syntax_for_resource(self.provider.as_ref(), uri);
            if source_inputs.markup_syntax != syntax {
                changes.markup_syntax = Some(syntax);
            }
        }
        if source_inputs.style_environment != current.style_environment {
            changes.style_environment = Some(current.style_environment);
        }
        if source_inputs.user_styles != current.user_styles {
            changes.user_styles = Some(current.user_styles.clone());
        }
        let mut layout = current.layout;
        layout.line_height = current.style_environment.root_font_size as f64;
        if source_inputs.layout != layout {
            changes.layout = Some(layout);
        }
        if source_inputs.resource_revision != current.resource_revision {
            changes.resource_revision = Some(current.resource_revision);
        }
        if source_inputs.stylesheet_revision != current.stylesheet_revision {
            changes.stylesheet_revision = Some(current.stylesheet_revision);
        }
        if source_inputs.font_environment != current.font_environment {
            changes.font_environment = Some(current.font_environment);
        }
        if source_inputs.image_metrics_revision != current.image_metrics_revision {
            changes.image_metrics_revision = Some(current.image_metrics_revision);
        }
        if source_inputs.paint != current.paint {
            changes.paint = Some(current.paint);
        }
        if !changes.is_empty() {
            changes.image_metrics = Some(current.image_metrics.clone());
        }
        changes
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
        let prepared = load::prepare_document_with_settings(
            self.provider.clone(),
            &uri,
            load::DocumentLoadSettings {
                root_font_size: self.root_font_size,
                column_width: self.page.inputs().layout.viewport_width,
                reader_overrides: self.page.inputs().reader_overrides.clone(),
                image_sizing_policy: self.page.inputs().layout.image_sizing_policy,
                text_composition_policy: self.page.inputs().layout.text_composition_policy,
                note_display: self.note_display,
            },
        )
        .expect("document preparation should succeed");
        self.host
            .set_font_overrides(&prepared.inputs().reader_overrides);
        self.host
            .prepare_font_faces(prepared.session.font_faces(), self.provider.clone());
        load::finish_prepared_document(prepared, glyph_shaper)
            .expect("document shaping should succeed")
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

        if self
            .prepared_navigation
            .as_ref()
            .is_some_and(|(index, _)| *index == doc_index)
        {
            let loaded = self.prepared_navigation.take().unwrap().1;
            let item = self.spine_item(loaded);
            self.activate_spine_item(doc_index, item);
            let (nav, mut cx) = self.nav_cx();
            nav.finish_document_navigation(&mut cx, anchor);
            return;
        }

        if let Some(mut cached) = self.document_cache.take_document(doc_index) {
            self.host.set_glyph_document(doc_index);
            self.host
                .set_font_overrides(&cached.inputs().reader_overrides);
            self.host.prepare_font_faces(
                cached.view.document.session.font_faces(),
                self.provider.clone(),
            );
            if cached
                .view
                .document
                .session
                .rehydrate_glyphs(glyph_shaper)
                .is_err()
                || cached.view.document.session.document().is_none()
            {
                let fallback = self.load_document_for_index(glyph_shaper, doc_index);
                let fallback = self.spine_item(fallback);
                self.activate_spine_item(doc_index, fallback);
                let (nav, mut cx) = self.nav_cx();
                nav.finish_document_navigation(&mut cx, anchor);
                return;
            }
            let requested_inputs = self.changes_for_cached_document(doc_index, cached.inputs());
            let inputs_match = requested_inputs.is_empty();
            self.activate_spine_item(doc_index, cached);
            if !inputs_match
                && self
                    .apply_pipeline_changes(requested_inputs, glyph_shaper)
                    .is_err()
            {
                let fallback = self.load_document_for_index(glyph_shaper, doc_index);
                let fallback = self.spine_item(fallback);
                self.activate_spine_item(doc_index, fallback);
            }
        } else {
            let loaded = self.load_document_for_index(glyph_shaper, doc_index);
            let item = self.spine_item(loaded);
            self.activate_spine_item(doc_index, item);
        }

        let (nav, mut cx) = self.nav_cx();
        nav.finish_document_navigation(&mut cx, anchor);
    }
}
