//! Link activation and footnote preview rendering.

use super::*;
use kurbo::Point;

#[derive(Clone, Copy)]
enum NoteReferenceKind {
    Explicit,
    Inferred,
}

impl RendererCore {
    fn handle_link_click_at(&mut self, glyph_shaper: &mut impl GlyphShaper, glyph_idx: u32, click_position: Option<Point>) -> bool {
        let view = self.page.view.document.render_view();
        let addressing = view.addressing();
        let Some(href_idx) = addressing.link_for_glyph(glyph_idx) else {
            return false;
        };
        let reference_kind = if addressing.is_note_reference(glyph_idx) { NoteReferenceKind::Explicit } else { NoteReferenceKind::Inferred };
        let href = view.string(href_idx).to_string();
        if let Some(mut preview) = self.footnote_preview_for_reference(glyph_shaper, &href, reference_kind) {
            preview.anchor = click_position.map(|point| crate::FootnotePopupAnchor::at_click(point, self.page.view.layout.size));
            self.host.emit(RendererEvent::FootnoteOpened(preview));
            return true;
        }
        self.navigate_to_href(glyph_shaper, &href);
        true
    }

    /// Activates a document-qualified click. A trailing slice is promoted to
    /// the reading cursor first, so link resolution, notes, annotations and
    /// emitted CFIs all use the document that actually received the input.
    pub(crate) fn handle_document_click(&mut self, glyph_shaper: &mut impl GlyphShaper, hit: crate::DocumentGlyph, click_position: Option<Point>) -> bool {
        if hit.doc != self.nav.location().document() {
            self.load_document_at(glyph_shaper, hit.doc, DocAnchor::Glyph(hit.glyph));
        }
        if self.annotations.activate_at_glyph(hit.glyph, self.host.as_ref()) {
            return true;
        }
        self.handle_link_click_at(glyph_shaper, hit.glyph, click_position)
    }

    pub(crate) fn footnote_preview(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str) -> Option<crate::FootnotePreview> {
        self.footnote_preview_for_reference(glyph_shaper, href, NoteReferenceKind::Inferred)
    }

    /// Builds the preview for a note reference by asking the engine to lay the
    /// note out on its own, rather than re-deriving it from source. The note is
    /// held out of the reading flow, so it owns no glyphs in the document on
    /// screen; a scoped layout is what makes its content available.
    ///
    /// There is nothing to preview when notes read in place: the reference
    /// resolves as an ordinary link to text the reader can already see.
    fn footnote_preview_for_reference(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str, reference_kind: NoteReferenceKind) -> Option<crate::FootnotePreview> {
        if self.note_display == crate::NoteDisplay::AsAuthored {
            return None;
        }
        let (_, fragment) = href.split_once('#')?;
        if fragment.is_empty() {
            return None;
        }
        let (doc, _) = self.resolve_href_target(href)?;

        // A note in another spine document has to be laid out before it can be
        // asked anything. Loading it is not wasted: a reader following the
        // reference lands there next, and the cache keeps it.
        let mut freshly_loaded = None;
        if doc != self.nav.location().document() && self.document_cache.document(doc).is_none() {
            self.nav.documents().uris().get(doc)?;
            freshly_loaded = Some(self.load_document_for_index(glyph_shaper, doc));
        }

        // Note semantics are read first, so the borrow is released before the
        // layout below needs a mutable one: appending a note's glyphs to a
        // document's renderer resources changes that document.
        let is_note_target = {
            let laid_out = match freshly_loaded.as_ref() {
                Some(loaded) => &loaded.document,
                None if doc == self.nav.location().document() => &self.page.view.document,
                None => &self.document_cache.document(doc)?.view.document,
            };
            laid_out.render_view().addressing().is_note_target(fragment)
        };

        // One authority for what a note is, whichever document it lives in.
        if matches!(reference_kind, NoteReferenceKind::Inferred) && !is_note_target {
            return None;
        }

        // The host decides how wide a note is shown, so it is laid out to that
        // width here rather than to the page's and stretched on arrival.
        let width = self.host.note_popup_width().filter(|width| width.is_finite() && *width > 0.0).unwrap_or(self.page.view.layout.col_width);
        let constraints = html::layout::LayoutConstraints::new(width, self.root_font_size as f64).ok()?;
        let note = match freshly_loaded.as_mut() {
            Some(loaded) => loaded.session.layout_note(fragment, constraints, glyph_shaper),
            None if doc == self.nav.location().document() => self.page.session.layout_note(fragment, constraints, glyph_shaper),
            None => self.document_cache.document_mut(doc)?.session.layout_note(fragment, constraints, glyph_shaper),
        }?;
        // Held as a view of its own, so selecting and hit testing inside it go
        // through the same code the page uses.
        self.note = Some(crate::document_view::DocumentView::new(note.clone(), crate::ColumnLayout { col_width: width, base_col_width: width, ..self.page.view.layout }).framed_whole());
        self.note_document = Some(doc);

        if let Some(loaded) = freshly_loaded {
            let item = self.spine_item(loaded);
            self.document_cache.insert_document(doc, item);
        }
        Some(crate::FootnotePreview { href: href.to_owned(), anchor: None })
    }
}
