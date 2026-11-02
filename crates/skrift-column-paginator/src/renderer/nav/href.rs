//! Href resolution and transactional location-history navigation.

use super::*;

impl RendererCore {
    pub(crate) fn resolve_href_target(&self, href: &str) -> Option<(usize, Option<String>)> {
        let resolved = self.page.session.resolve_link(href)?;
        let document = if href.is_empty() || href.starts_with('#') {
            self.nav.location().document()
        } else {
            let mut matches = self
                .nav
                .documents()
                .uris()
                .iter()
                .enumerate()
                .filter(|(_, uri)| *uri == &resolved.resource)
                .map(|(index, _)| index);
            let index = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            index
        };
        Some((document, resolved.fragment))
    }

    /// An explicit chapter jump, unlike crossing a chapter while page turning.
    pub(crate) fn navigate_document_jump(&mut self, shaper: &mut impl GlyphShaper, step: i8) {
        let current = self.nav.location().document();
        let Some(doc) = (if step < 0 {
            current.checked_sub(1)
        } else {
            current.checked_add(1)
        })
        .filter(|doc| *doc < self.nav.documents().uris().len()) else {
            return;
        };
        if self.defer_document_read(doc, NavigationRequest::DocumentJump(step)) {
            return;
        }
        let from = self.nav.current_cfi(self.nav_view());
        self.load_document_at(
            shaper,
            doc,
            if step < 0 {
                DocAnchor::End
            } else {
                DocAnchor::Start
            },
        );
        if let Some(from) = from {
            self.nav.begin_jump(from);
        }
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.resolve_href_target(href)
    }

    pub fn navigate_to_href(&mut self, glyph_shaper: &mut impl GlyphShaper, href: &str) -> bool {
        let from = self.nav.current_location(self.nav_view());
        let from_cfi = self.nav.current_cfi(self.nav_view());
        let Some((doc, anchor)) = self.resolve_href_target(href) else {
            return false;
        };
        if self.defer_document_read(doc, NavigationRequest::Href(href.to_owned())) {
            return true;
        }
        let to = html_view_navigate::Location::new(doc, anchor);
        if !self.navigate_to_location(glyph_shaper, to.clone()) {
            return false;
        }
        if let Some(from) = from_cfi {
            self.nav.begin_jump(from);
        }
        if from != to {
            self.nav.history.push(from, self.host.as_ref());
            self.nav.history.push(to, self.host.as_ref());
        }
        true
    }

    pub(super) fn navigate_history_step(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        step: isize,
    ) -> bool {
        let Some(target) = self.nav.history.peek_step(step) else {
            self.nav.history.emit_availability(self.host.as_ref());
            return false;
        };
        if self.defer_document_read(target.doc, NavigationRequest::History(step)) {
            return true;
        }
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
        let html_view_navigate::Location {
            doc: doc_index,
            anchor,
        } = target;
        if doc_index >= self.nav.documents().uris().len()
            || anchor.as_deref().is_some_and(|anchor| {
                !self.ensure_navigation_anchor(glyph_shaper, doc_index, anchor)
            })
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

    fn document_has_anchor(document: html::layout::RenderView<'_>, anchor: &str) -> bool {
        html::targets::Targets::new(document).has_element(anchor)
    }

    /// Ensures a target anchor exists before navigation mutates the current
    /// document or history cursor. Its preparation is consumed by the same
    /// navigation after the anchor has been validated.
    fn ensure_navigation_anchor(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        doc_index: usize,
        anchor: &str,
    ) -> bool {
        if doc_index == self.nav.location().document() {
            return Self::document_has_anchor(self.page.render_view(), anchor);
        }
        if let Some((index, loaded)) = &self.prepared_navigation {
            if *index == doc_index {
                return Self::document_has_anchor(
                    loaded
                        .render_view()
                        .expect("loaded document has glyph resources"),
                    anchor,
                );
            }
        }

        let loaded = self.load_document_for_index(glyph_shaper, doc_index);
        let valid = Self::document_has_anchor(
            loaded
                .render_view()
                .expect("loaded document has glyph resources"),
            anchor,
        );
        self.prepared_navigation = valid.then_some((doc_index, loaded));
        self.host.set_glyph_document(self.nav.location().document());
        valid
    }
}
