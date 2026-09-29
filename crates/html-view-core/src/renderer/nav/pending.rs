//! Background file access for navigation. The active page and history remain
//! intact until preparation succeeds; a later navigation replaces pending work.
use super::*;
use std::sync::mpsc::{Receiver, TryRecvError};

pub(crate) enum NavigationRequest {
    Document(usize, DocAnchor),
    Href(String),
    History(isize),
    Click(crate::DocumentGlyph, Option<kurbo::Point>),
    Preview(String),
    Search(String),
}
pub(crate) struct PendingNavigation {
    doc: usize,
    action: NavigationRequest,
    settings: load::DocumentLoadSettings,
    receive: Receiver<Result<load::PreparedRenderDocument, String>>,
}

impl RendererCore {
    fn document_settings(&self) -> load::DocumentLoadSettings {
        load::DocumentLoadSettings {
            root_font_size: self.root_font_size,
            column_width: self.page.inputs.layout.viewport_width,
            reader_overrides: self.page.inputs.reader_overrides.clone(),
            image_sizing_policy: self.page.inputs.layout.image_sizing_policy,
            text_composition_policy: self.page.inputs.layout.text_composition_policy,
            note_display: self.note_display,
        }
    }

    pub(crate) fn defer_document_read(&mut self, doc: usize, action: NavigationRequest) -> bool {
        let Some(wake) = self.host.resource_waker() else {
            return false;
        };
        if self
            .prepared_navigation
            .as_ref()
            .is_some_and(|(index, _)| *index == doc)
        {
            return false;
        }
        if doc == self.nav.location().document() || self.document_cache.document(doc).is_some() {
            self.pending_navigation = None;
            return false;
        }
        let Some(uri) = self.nav.documents().uris().get(doc).cloned() else {
            return false;
        };
        if let Some(pending) = &mut self.pending_navigation {
            if pending.doc == doc {
                pending.action = action;
                return true;
            }
        }
        let settings = self.document_settings();
        let work_settings = settings.clone();
        let provider = self.provider.clone();
        let (send, receive) = std::sync::mpsc::channel();
        self.pending_navigation = Some(PendingNavigation {
            doc,
            action,
            settings,
            receive,
        });
        self.host.blocking_job_spawner().spawn(Box::new(move || {
            let result = load::prepare_document_with_settings(provider, &uri, work_settings);
            if send.send(result).is_ok() {
                wake();
            }
        }));
        true
    }

    pub(crate) fn finish_pending_navigation(&mut self, shaper: &mut impl GlyphShaper) {
        let Some(pending) = &self.pending_navigation else {
            return;
        };
        let result = match pending.receive.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("document preparation worker stopped".into()),
        };
        let pending = self.pending_navigation.take().unwrap();
        let settings = self.document_settings();
        if settings.root_font_size != pending.settings.root_font_size
            || settings.column_width != pending.settings.column_width
            || settings.reader_overrides != pending.settings.reader_overrides
            || settings.image_sizing_policy != pending.settings.image_sizing_policy
            || settings.text_composition_policy != pending.settings.text_composition_policy
            || settings.note_display != pending.settings.note_display
        {
            self.defer_document_read(pending.doc, pending.action);
            return;
        }
        self.host.set_glyph_document(pending.doc);
        let loaded =
            match result.and_then(|prepared| load::finish_prepared_document(prepared, shaper)) {
                Ok(loaded) => loaded,
                Err(message) => {
                    self.host.set_glyph_document(self.nav.location().document());
                    self.emit_operation_failed(crate::RendererOperation::UpdateDocument, message);
                    return;
                }
            };
        self.prepared_navigation = Some((pending.doc, loaded));
        match pending.action {
            NavigationRequest::Document(doc, anchor) => self.load_document_at(shaper, doc, anchor),
            NavigationRequest::Href(href) => {
                self.navigate_to_href(shaper, &href);
            }
            NavigationRequest::History(step) => {
                self.navigate_history_step(shaper, step);
            }
            NavigationRequest::Click(hit, point) => {
                self.handle_document_click(shaper, hit, point);
            }
            NavigationRequest::Search(query) => {
                if self.highlight.query() == query {
                    self.navigate_to_book_match(shaper);
                }
            }
            NavigationRequest::Preview(href) => {
                if let Some(preview) = self.footnote_preview(shaper, &href) {
                    self.host.emit(RendererEvent::FootnoteOpened(preview));
                }
            }
        }
        // Invalid anchors need not activate the document, but its completed
        // preparation can still serve a subsequent request.
        if let Some((doc, loaded)) = self.prepared_navigation.take() {
            let item = self.spine_item(loaded);
            self.document_cache.insert_document(doc, item);
        }
        self.host.set_glyph_document(self.nav.location().document());
        self.host.request_repaint();
    }
}
