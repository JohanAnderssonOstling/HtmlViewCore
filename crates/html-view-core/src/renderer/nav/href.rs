//! Href resolution and transactional location-history navigation.

use super::*;

impl RendererCore {
    fn resolve_doc_index_for_path(&self, path: &str) -> Option<usize> {
        let base_uri = self
            .nav
            .documents()
            .uris()
            .get(self.nav.location().document())
            .map(String::as_str)?;
        let resolved = self.provider.resolve(base_uri, path);

        // The provider owns resource identity and URI resolution. Navigation
        // accepts only one exact, unambiguous occurrence of that identifier in
        // the spine. It never guesses from the raw href or a partial path.
        let mut matches = self
            .nav
            .documents()
            .uris()
            .iter()
            .enumerate()
            .filter(|(_, uri)| *uri == &resolved)
            .map(|(index, _)| index);
        let index = matches.next()?;
        matches.next().is_none().then_some(index)
    }

    pub(crate) fn resolve_href_target(&self, href: &str) -> Option<(usize, Option<String>)> {
        if let Some(anchor) = href.strip_prefix('#') {
            return Some((
                self.nav.location().document(),
                NavigationState::fragment_part(anchor),
            ));
        }

        let (path_part, fragment) = href.split_once('#').unwrap_or((href, ""));
        let fragment = NavigationState::fragment_part(fragment);
        if path_part.is_empty() {
            return Some((self.nav.location().document(), fragment));
        }

        let idx = self.resolve_doc_index_for_path(path_part)?;
        Some((idx, fragment))
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.resolve_href_target(href)
    }

    pub fn navigate_to_href(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str) -> bool {
        let from = self.nav.current_location(self.nav_view());
        let Some((doc, anchor)) = self.resolve_href_target(href) else {
            return false;
        };
        let to = html_view_navigate::Location::new(doc, anchor);
        if !self.navigate_to_location(glyph_shaper, to.clone()) {
            return false;
        }
        if from != to {
            self.nav.history.push(from, self.host.as_ref());
            self.nav.history.push(to, self.host.as_ref());
        }
        true
    }

    fn navigate_history_step(&mut self, glyph_shaper: &mut impl GlyphShaper, step: isize) -> bool {
        let Some(target) = self.nav.history.peek_step(step) else {
            self.nav.history.emit_availability(self.host.as_ref());
            return false;
        };
        if !self.navigate_to_location(glyph_shaper, target) {
            return false;
        }
        let committed = self.nav.history.step(step, self.host.as_ref()).is_some();
        self.nav.history.emit_availability(self.host.as_ref());
        committed
    }

    pub fn navigate_history_back(&mut self, glyph_shaper: &mut impl GlyphShaper) -> bool {
        self.navigate_history_step(glyph_shaper, -1)
    }

    pub fn navigate_history_forward(&mut self, glyph_shaper: &mut impl GlyphShaper) -> bool {
        self.navigate_history_step(glyph_shaper, 1)
    }

    fn navigate_to_location(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        target: html_view_navigate::Location,
    ) -> bool {
        let html_view_navigate::Location { doc: doc_index, anchor } = target;
        if doc_index >= self.nav.documents().uris().len()
            || anchor
                .as_deref()
                .is_some_and(|anchor| !self.ensure_navigation_anchor(glyph_shaper, doc_index, anchor))
        {
            return false;
        }
        if doc_index != self.nav.location().document() {
            self.load_document_at(glyph_shaper, doc_index, DocAnchor::Start);
        }
        if let Some(anchor) = anchor.as_deref() {
            let (nav, mut cx) = self.nav_cx();
            nav.jump_to_id(&mut cx, anchor)
        } else {
            let (nav, mut cx) = self.nav_cx();
            nav.restore_forward_offset(&mut cx, 0.0);
            true
        }
    }

    fn document_has_anchor(document: &LaidOutDocument, anchor: &str) -> bool {
        let view = document.render_view();
        let Some(id) = view.lookup_string(anchor) else {
            return false;
        };
        let addressing = view.addressing();
        addressing.anchor_glyph(id).is_some() || addressing.anchor_position(id).is_some()
    }

    /// Ensures a target anchor exists before navigation mutates the current
    /// document or history cursor. A newly inspected document is retained in
    /// the normal cache for the subsequent committed navigation.
    fn ensure_navigation_anchor(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
        anchor: &str,
    ) -> bool {
        if doc_index == self.nav.location().document() {
            return Self::document_has_anchor(&self.page.view.document, anchor);
        }
        if let Some(item) = self.document_cache.document(doc_index) {
            return Self::document_has_anchor(&item.view.document, anchor);
        }

        let loaded = self.load_document_for_index(glyph_shaper, doc_index);
        let valid = Self::document_has_anchor(&loaded.document, anchor);
        let item = self.spine_item(loaded);
        self.document_cache.insert_document(doc_index, item);
        self.host.set_glyph_document(self.nav.location().document());
        valid
    }
}
