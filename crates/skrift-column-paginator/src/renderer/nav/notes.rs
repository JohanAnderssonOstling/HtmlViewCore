//! Link activation and footnote preview rendering.

use super::*;
use kurbo::Point;

#[derive(Clone, Copy)]
enum NoteReferenceKind {
    Explicit,
    Inferred,
}

impl RendererCore {
    fn handle_link_click_at(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        glyph_idx: u32,
        click_position: Option<Point>,
    ) -> bool {
        let view = self.page.render_view();
        let Some(link) = view.addressing().link_for_glyph(glyph_idx) else {
            return false;
        };
        let reference_kind = if link.is_note_reference {
            NoteReferenceKind::Explicit
        } else {
            NoteReferenceKind::Inferred
        };
        let href = link.href.to_owned();
        if let Some((doc, _)) = self.resolve_href_target(&href) {
            let hit = crate::DocumentGlyph {
                doc: self.nav.current_document(),
                glyph: glyph_idx,
            };
            if self.defer_document_read(doc, NavigationRequest::Click(hit, click_position)) {
                return true;
            }
        }
        if let Some(mut preview) =
            self.footnote_preview_for_reference(glyph_shaper, &href, reference_kind)
        {
            preview.anchor = click_position
                .map(|point| crate::FootnotePopupAnchor::at_click(point, self.page.layout.size));
            if let Some(note) = &mut self.note {
                note.preview.anchor = preview.anchor;
            }
            self.effects.emit(RendererEvent::FootnoteOpened(preview));
            return true;
        }
        if self.resolve_href_target(&href).is_some() {
            self.navigate_to_href(glyph_shaper, &href);
        } else if let Some(link) = self.page.session.resolve_link(&href) {
            let mut url = link.resource;
            if let Some(fragment) = link.fragment {
                url.push('#');
                url.push_str(&fragment);
            }
            self.effects.emit(RendererEvent::ExternalLink(url));
        }
        true
    }

    /// Activates a document-qualified click. A trailing slice is promoted to
    /// the reading cursor first, so link resolution, notes, annotations and
    /// emitted CFIs all use the document that actually received the input.
    pub(crate) fn handle_document_click(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        hit: crate::DocumentGlyph,
        click_position: Option<Point>,
    ) -> bool {
        if self.defer_document_read(hit.doc, NavigationRequest::Click(hit, click_position)) {
            return true;
        }
        if hit.doc != self.nav.current_document() {
            self.load_document_at(glyph_shaper, hit.doc, DocAnchor::Glyph(hit.glyph));
            if self.nav.current_document() != hit.doc {
                return false;
            }
        }
        if self
            .annotations
            .activate_at_glyph(hit.glyph, &mut self.effects)
        {
            return true;
        }
        self.handle_link_click_at(glyph_shaper, hit.glyph, click_position)
    }

    pub(crate) fn footnote_preview(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        href: &str,
    ) -> Option<crate::FootnotePreview> {
        if let Some((doc, _)) = self.resolve_href_target(href) {
            if self.defer_document_read(doc, NavigationRequest::Preview(href.to_owned())) {
                return None;
            }
        }
        let preview =
            self.footnote_preview_for_reference(glyph_shaper, href, NoteReferenceKind::Inferred);
        self.prepared_navigation = None;
        preview
    }

    /// Builds the preview for a note reference by asking the engine to lay the
    /// note out on its own, rather than re-deriving it from source. The note is
    /// held out of the reading flow, so it owns no glyphs in the document on
    /// screen; a scoped layout is what makes its content available.
    ///
    /// There is nothing to preview when notes read in place: the reference
    /// resolves as an ordinary link to text the reader can already see.
    fn footnote_preview_for_reference(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        href: &str,
        reference_kind: NoteReferenceKind,
    ) -> Option<crate::FootnotePreview> {
        if self.note_display == crate::NoteDisplay::AsAuthored {
            return None;
        }
        let (_, fragment) = href.split_once('#')?;
        if fragment.is_empty() {
            return None;
        }
        let (doc, _) = self.resolve_href_target(href)?;

        let mut freshly_loaded = if doc != self.nav.current_document() {
            self.nav.document_uris().get(doc)?;
            let mut loaded = match self.load_document_for_index(glyph_shaper, doc) {
                Ok(loaded) => loaded,
                Err(message) => {
                    self.emit_operation_failed(crate::RendererOperation::PreviewNote, message);
                    return None;
                }
            };
            loaded.set_image_service(self.image_service.clone());
            Some(loaded)
        } else {
            None
        };

        let width = self.note_layout_width();
        let owner = freshly_loaded.as_mut().unwrap_or(&mut self.page.session);
        let is_note_target = owner
            .render_view()?
            .addressing()
            .element(fragment)
            .is_some_and(|target| target.is_note());
        let note = if matches!(reference_kind, NoteReferenceKind::Explicit) || is_note_target {
            owner.scope(fragment, width, glyph_shaper)
        } else {
            Ok(None)
        };
        // A preview is optional: a failed scoped layout follows the ordinary
        // link path, reusing the loaded document just as a missing note does.
        let note = match note {
            Ok(note) => note,
            Err(error) => {
                self.emit_operation_failed(
                    crate::RendererOperation::PreviewNote,
                    error.to_string(),
                );
                None
            }
        };
        let Some(note) = note else {
            if let Some(loaded) = freshly_loaded {
                self.prepared_navigation = Some((doc, loaded));
            }
            return None;
        };
        // Held as a view of its own, so selecting and hit testing inside it go
        // through the same code the page uses.
        if let Some(owner) = &mut freshly_loaded {
            owner.request_images(None, 0, std::iter::once(&note));
        }
        let preview = crate::FootnotePreview {
            href: href.to_owned(),
            anchor: None,
        };
        self.note = Some(crate::document_view::OpenNote {
            view: note,
            preview: preview.clone(),
            owner: freshly_loaded,
        });
        self.prefetch_images_for_current_page();
        Some(preview)
    }
}
