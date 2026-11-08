use html::selection::{SelectionExport, SelectionInput, SelectionOutcome};
use html::export::{ExportPayload, ExportRequest};
use kurbo::{Point, Size};
use std::sync::mpsc::{TryRecvError, channel};

use crate::{
    ClipboardWrite, DocumentImage, DocumentUpdate, FrameGeometryCacheKey, GlyphShaper,
    RendererCore, RendererEvent, RendererOperation,
};

impl RendererCore {
    pub fn hit_test_document_image(&self, pos: Point) -> Option<DocumentImage> {
        self.page
            .geometry()
            .hit_test_image(pos)
            .map(|image| DocumentImage {
                doc: self.nav.current_document(),
                image,
            })
    }

    pub(crate) fn open_image(&mut self, hit: DocumentImage) -> bool {
        let Some(request) = self.page.session.image_source_request(hit.image) else {
            return false;
        };
        let read = move || {
            request
                .read()
                .map(|source| RendererEvent::ImageOpened {
                    uri: source.uri,
                    bytes: source.bytes,
                })
                .map_err(|error| error.to_string())
        };
        if let Some(wake) = self.services.resource_waker.clone() {
            let (send, receive) = channel();
            self.pending_file_events.push(receive);
            self.services.blocking_jobs.spawn(Box::new(move || {
                if send.send(read()).is_ok() {
                    wake();
                }
            }));
            return true;
        }
        let result = read();
        let opened = result.is_ok();
        self.effects.emit(file_event(result));
        opened
    }

    pub(crate) fn emit_visible_text(&mut self) {
        let text = self.visible_text();
        if !text.is_empty() {
            self.effects.emit(RendererEvent::SpeakableText(text));
        }
    }

    pub(crate) fn visible_text(&self) -> String {
        self.page.geometry().text()
    }

    /// Incorporates completed image work. This may update the document
    /// pipeline and must be called from an application update, never from a
    /// render-phase viewport preparation.
    pub fn update_resources(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        let effects = &mut self.effects;
        self.pending_file_events.retain(|receiver| {
            match receiver.try_recv() {
                Ok(result) => effects.emit(file_event(result)),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => effects.emit(RendererEvent::OperationFailed {
                    operation: RendererOperation::UpdateDocument,
                    message: "file worker stopped".into(),
                }),
            }
            false
        });
        // Scopes own their engine registrations; page and popup layout can
        // update independently while sharing the caller's native cache.
        let mut note = self.note.take();
        let image_poll = self.page.session.poll_resources();
        if image_poll.decoded {
            self.revisions.invalidate_resources();
        }
        if image_poll.layout_needed
            && let Err(error) =
                self.apply_document_update_without_repaint(DocumentUpdate::default(), glyph_shaper)
        {
            self.emit_operation_failed(RendererOperation::UpdateDocument, error.to_string());
        }
        if let Some(open) = &mut note {
            let result = (|| {
                let decoded = if let Some(owner) = &mut open.owner {
                    let poll = owner.poll_resources();
                    if poll.layout_needed {
                        owner.apply(DocumentUpdate::default(), glyph_shaper)?;
                    }
                    poll.decoded
                } else {
                    image_poll.decoded
                };
                let owner = open.owner.as_mut().unwrap_or(&mut self.page.session);
                let relaid_out = open.view.refresh_resources(owner, glyph_shaper)?;
                if decoded || relaid_out {
                    self.effects
                        .emit(RendererEvent::FootnoteOpened(open.preview.clone()));
                    self.effects.request_repaint();
                }
                Ok::<(), html::Error>(())
            })();
            if let Err(error) = result {
                self.emit_operation_failed(RendererOperation::PreviewNote, error.to_string());
            }
        }
        self.note = note;
    }

    /// Updates viewport composition without touching the document pipeline.
    pub fn prepare_viewport(&mut self, viewport_size: Size) {
        let previous_geometry = self.page.frame.geometry_key();
        self.configure_layout_for_viewport_impl(viewport_size);
        let requested_geometry = self.frame_geometry_cache_key();
        if previous_geometry != Some(requested_geometry) {
            self.interact_selection(SelectionInput::PointerLeave);
            if previous_geometry
                .is_some_and(|previous| can_reproject_width_only(previous, requested_geometry))
            {
                self.page
                    .frame
                    .reproject_columns(self.page.layout.page_geometry());
            } else {
                self.prepare_page(self.viewport.request());
            }
            self.page
                .frame
                .set_geometry_key(self.frame_geometry_cache_key());
        }
        // A cursor request can reuse the page despite a different fallback
        // offset. Publish the installed origin for position reporting too.
        self.viewport.install_page(self.page.frame.page().content());
        self.prefetch_images_for_current_page();
        if self.nav.anchor_update_pending() {
            let view = html_view_navigate::NavView {
                doc: self.page.render_view(),
                viewport: &self.viewport,
                frame: &self.page.frame,
            };
            self.nav.update_nav_anchor_from_layout(view);
            self.nav.mark_anchor_clean();
        }
        let (nav, mut cx) = self.nav_cx();
        nav.emit_reading_position(&mut cx);
    }

    /// Incorporates resources and then prepares viewport composition.
    /// Application adapters should prefer the two explicit operations so the
    /// pipeline-capable resource update cannot leak into a render phase.
    pub fn prepare_frame(&mut self, glyph_shaper: &mut impl GlyphShaper, viewport_size: Size) {
        self.update_resources(glyph_shaper);
        self.prepare_viewport(viewport_size);
    }

    #[cfg(test)]
    pub(crate) fn configure_layout_for_viewport(
        &mut self,
        _glyph_shaper: &mut impl GlyphShaper,
        viewport_size: Size,
    ) {
        self.configure_layout_for_viewport_impl(viewport_size);
    }

    fn configure_layout_for_viewport_impl(&mut self, viewport_size: Size) {
        self.page.layout.configure_viewport(viewport_size);
    }

    /// Returns whether a point in renderer-layout coordinates is over the
    /// painted bounds of a linked glyph.
    pub fn link_at(&self, position: Point) -> bool {
        self.page.geometry().link_at(position).is_some()
    }

    pub(crate) fn interact_selection(&mut self, input: SelectionInput) -> SelectionOutcome {
        let outcome = self.page.interact_selection(input);
        if outcome.changed {
            self.request_overlay_repaint();
        }
        if outcome.finished {
            self.publish_document_selection();
        }
        outcome
    }

    pub(crate) fn publish_document_selection(&mut self) {
        let doc = self.nav.current_document();
        let view = &self.page;
        if let Some(range) = view
            .selection
            .selection(view.render_view())
            .map(|selection| selection.range())
            && let Some(cfi_range) =
                crate::cfi::generate_cfi_range(view, doc, range.start, range.end)
                    .and_then(|cfi| self.nav.cfi.to_publication(&cfi))
        {
            self.effects
                .emit(RendererEvent::SelectionFinished { doc, cfi_range });
        }
    }

    pub fn copy_selection_to_clipboard(&mut self) -> bool {
        if let Some(text) = self
            .note_parts()
            .and_then(|(note, owner)| note.selection(owner)?.markdown())
        {
            self.effects.clipboard.push(ClipboardWrite::Text(text));
            return true;
        }
        let Some(text) = self
            .page
            .geometry()
            .export_selection(&self.page.selection, SelectionExport::Markdown)
        else {
            return false;
        };
        self.effects.clipboard.push(ClipboardWrite::Text(text));
        true
    }

    pub(crate) fn copy_document_image_to_clipboard(
        &mut self,
        hit: DocumentImage,
    ) -> Result<(), String> {
        if hit.doc != self.nav.current_document() {
            return Err(format!("document {} is not visible", hit.doc));
        }
        let export = self.page.session.export().ok_or("document is not loaded")?;
        let payload = export
            .request(ExportRequest::Image(hit.image))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("image {} is not decoded yet", hit.image.index()))?;
        let ExportPayload::Image(payload) = payload else {
            return Err("expected an image export".into());
        };
        match payload {
            html::export::ImagePayload::Raster {
                width,
                height,
                rgba,
            } => self.effects.clipboard.push(ClipboardWrite::Image {
                width,
                height,
                rgba: rgba.to_vec(),
            }),
            html::export::ImagePayload::Svg { bytes } => self
                .effects
                .clipboard
                .push(ClipboardWrite::Svg(bytes.to_vec())),
        }
        Ok(())
    }
}

fn can_reproject_width_only(
    previous: FrameGeometryCacheKey,
    requested: FrameGeometryCacheKey,
) -> bool {
    previous.layout_revision == requested.layout_revision
        && previous.viewport_height_bits == requested.viewport_height_bits
        && previous.column_width_bits == requested.column_width_bits
        && previous.column_count == requested.column_count
        && previous.scale_bits == requested.scale_bits
        && previous.page_request == requested.page_request
}

fn file_event(result: Result<RendererEvent, String>) -> RendererEvent {
    result.unwrap_or_else(|message| RendererEvent::OperationFailed {
        operation: RendererOperation::UpdateDocument,
        message,
    })
}
