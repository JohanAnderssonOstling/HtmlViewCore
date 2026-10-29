use html::export::{ExportFormat, ExportPayload, ExportTarget};
use kurbo::{Point, Size};

use crate::prepared_page::PreparedPage;
use crate::spine_item::SpineItem;

use crate::{FrameGeometryCacheKey, GlyphShaper, PaintDirection, RendererCore};

impl RendererCore {
    fn visible_view_for_doc(&self, doc: usize) -> Option<&SpineItem> {
        (doc == self.nav.location().document()).then_some(&self.page)
    }

    fn visible_view_mut_for_doc(&mut self, doc: usize) -> Option<&mut SpineItem> {
        (doc == self.nav.location().document()).then_some(&mut self.page)
    }

    /// Hit-tests the current spine item.
    pub fn hit_test_document_glyph(&self, pos: Point) -> Option<crate::DocumentGlyph> {
        self.page
            .hit_test_glyph(pos)
            .map(|glyph| crate::DocumentGlyph {
                doc: self.nav.location().document(),
                glyph,
            })
    }

    pub fn hit_test_document_image(&self, pos: Point) -> Option<crate::DocumentImage> {
        self.page
            .geometry()
            .hit_test_image(pos)
            .map(|image| crate::DocumentImage {
                doc: self.nav.location().document(),
                image,
            })
    }

    fn install_prepared_page(&mut self, prepared: PreparedPage) {
        prepared.install(&mut self.page, &mut self.viewport);
    }

    fn restore_prepared_page(&mut self, key: FrameGeometryCacheKey) -> bool {
        let Some(prepared) = self.prepared_pages.take(key) else {
            return false;
        };
        self.install_prepared_page(prepared);
        true
    }

    pub(crate) fn cache_current_prepared_page(&mut self) {
        let Some(key) = self.page.frame.geometry_key() else {
            return;
        };
        let frame = std::mem::take(&mut self.page.frame);
        let continuation = self.viewport.continuation();
        self.prepared_pages.insert(PreparedPage {
            key,
            frame,
            continuation,
        });
    }

    pub(crate) fn restore_prepared_page_ending_at(&mut self, end_line: usize) -> bool {
        let current = self.frame_geometry_cache_key();
        let Some(prepared) = self.prepared_pages.take_ending_at(end_line, current) else {
            return false;
        };
        self.install_prepared_page(prepared);
        true
    }

    pub(crate) fn open_image_at(&self, position: Point) -> bool {
        let Some(hit) = self.hit_test_document_image(position) else {
            return false;
        };
        let Some(view) = self.visible_view_for_doc(hit.doc) else {
            return false;
        };
        let Some(request) = view.session.image_source_request(hit.image) else {
            return false;
        };
        if let Some(wake) = self.host.resource_waker() {
            let (send, receive) = std::sync::mpsc::channel();
            self.pending_file_events.borrow_mut().push(receive);
            self.host.blocking_job_spawner().spawn(Box::new(move || {
                let result = request
                    .read()
                    .map(|source| crate::RendererEvent::ImageOpened {
                        uri: source.uri,
                        bytes: source.bytes,
                    })
                    .map_err(|error| error.to_string());
                if send.send(result).is_ok() {
                    wake();
                }
            }));
            return true;
        }
        let Ok(source) = request.read() else {
            return false;
        };
        self.host.emit(crate::RendererEvent::ImageOpened {
            uri: source.uri,
            bytes: source.bytes,
        });
        true
    }

    pub(crate) fn emit_visible_text(&self) {
        let text = self.visible_text();
        if !text.is_empty() {
            self.host.emit(crate::RendererEvent::SpeakableText(text));
        }
    }

    pub(crate) fn visible_text(&self) -> String {
        Self::visible_text_for_view(&self.page)
    }

    fn visible_text_for_view(view: &SpineItem) -> String {
        html::content::Content::new(view.render_view())
            .text_for_lines(
                view.frame
                    .page()
                    .line_positions()
                    .iter()
                    .map(|(index, _)| index),
            )
            .unwrap_or_default()
    }

    fn apply_image_poll(
        &mut self,
        glyph_shaper: &mut impl GlyphShaper,
        poll: html::engine::ResourcePoll,
    ) {
        if poll.decoded {
            self.revisions.invalidate_resources();
        }
        if let Some(changes) = poll.changes {
            if let Err(error) = self.apply_pipeline_changes_without_repaint(changes, glyph_shaper) {
                self.emit_operation_failed(
                    crate::RendererOperation::UpdateDocument,
                    error.to_string(),
                );
            }
        }
    }

    /// Incorporates completed image work. This may update the document
    /// pipeline and must be called from an application update, never from a
    /// render-phase viewport preparation.
    pub fn update_resources(&mut self, glyph_shaper: &mut impl GlyphShaper) {
        self.pending_file_events.borrow_mut().retain(|receiver| {
            match receiver.try_recv() {
                Ok(Ok(event)) => self.host.emit(event),
                Ok(Err(message)) => {
                    self.emit_operation_failed(crate::RendererOperation::UpdateDocument, message)
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.emit_operation_failed(
                    crate::RendererOperation::UpdateDocument,
                    "file worker stopped".into(),
                ),
            }
            false
        });
        let image_poll = self.page.session.poll_resources();
        self.apply_image_poll(glyph_shaper, image_poll);
    }

    /// Updates viewport composition without touching the document pipeline.
    pub fn prepare_viewport(&mut self, viewport_size: Size) {
        let previous_geometry = self.page.frame.geometry_key();
        self.configure_layout_for_viewport_impl(viewport_size);
        let requested_geometry = self.frame_geometry_cache_key();
        if previous_geometry != Some(requested_geometry) {
            if previous_geometry
                .is_some_and(|previous| can_reproject_width_only(previous, requested_geometry))
            {
                let previous =
                    previous_geometry.expect("width-only reprojection requires installed geometry");
                self.page.frame.reproject_columns(
                    crate::ColumnLayout::leading_offset_for(
                        f64::from_bits(previous.viewport_width_bits),
                        f64::from_bits(previous.column_width_bits),
                        f64::from(previous.column_count),
                    ),
                    f64::from_bits(previous.column_width_bits),
                    f64::from_bits(previous.column_gap_bits),
                    crate::ColumnLayout::leading_offset_for(
                        f64::from_bits(requested_geometry.viewport_width_bits),
                        f64::from_bits(requested_geometry.column_width_bits),
                        f64::from(requested_geometry.column_count),
                    ),
                    f64::from_bits(requested_geometry.column_width_bits),
                    f64::from_bits(requested_geometry.column_gap_bits),
                );
            } else if !self.restore_prepared_page(requested_geometry) {
                match self.viewport.composition().direction() {
                    PaintDirection::Forward => self.prepare_forward(),
                    PaintDirection::Backward => self.prepare_backward(),
                }
            }
            self.page
                .frame
                .set_geometry_key(self.frame_geometry_cache_key());
        }
        self.prefetch_images_for_current_page();
        if self.nav.location().anchor_update_pending() {
            let view = html_view_navigate::NavView {
                doc: html_view_doc::DocQuery::from_view(self.page.render_view()),
                viewport: &self.viewport,
                frame: &self.page.frame,
            };
            self.nav.update_nav_anchor_from_layout(view);
            self.update_nav_signal();
            self.nav.mark_anchor_clean();
        }
        self.update_nav_signal();
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
        self.page
            .layout
            .begin_viewport_reconfiguration(viewport_size);
        // Window dimensions affect fragmentainer composition only. Document
        // layout uses the reader's explicit column-width setting and is
        // updated only by an explicit renderer command or resource change.
        self.page.layout.finish_viewport_reconfiguration(None);
    }

    /// Returns whether a point in renderer-layout coordinates is over the
    /// painted bounds of a linked glyph.
    pub fn link_at(&self, position: Point) -> bool {
        self.page.geometry().link_at(position).is_some()
    }

    pub fn begin_selection_at(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        let Some(hit) = self.hit_test_document_glyph(position) else {
            return false;
        };
        self.page.selection.cancel_interaction();
        let Some(view) = self.visible_view_mut_for_doc(hit.doc) else {
            return false;
        };
        if !view.begin_selection_at(position, mode) {
            return false;
        }
        view.update_selection_text();
        self.request_overlay_repaint();
        true
    }

    pub fn update_selection_at(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        let active_doc = self
            .page
            .selection
            .interaction()
            .is_selecting()
            .then(|| self.nav.location().document());
        let Some(doc) = active_doc else { return false };
        if self
            .hit_test_document_glyph(position)
            .is_none_or(|hit| hit.doc != doc)
        {
            return false;
        }
        let Some(view) = self.visible_view_mut_for_doc(doc) else {
            return false;
        };
        if !view.update_selection_at(position, mode) {
            return false;
        }
        view.update_selection_text();
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn finish_document_selection(&mut self) -> Option<crate::DocumentGlyph> {
        if !self.page.selection.interaction().is_selecting() {
            return None;
        }
        let doc = self.nav.location().document();

        let click = {
            let view = self.visible_view_mut_for_doc(doc)?;
            let click = view.finish_selection();
            view.update_selection_text();
            click
        };

        self.publish_document_selection(doc);
        click.map(|glyph| crate::DocumentGlyph { doc, glyph })
    }

    pub(crate) fn publish_document_selection(&self, doc: usize) {
        let Some(view) = self.visible_view_for_doc(doc) else {
            return;
        };
        if let (Some((start, end)), Some(exact_text)) = (
            view.selection_view().range(),
            view.selection.content().plain().map(str::to_owned),
        ) && let Some(cfi_range) = crate::cfi::generate_cfi_range(view, doc, start, end)
        {
            let quote = html::targets::Targets::new(view.render_view()).quote(start..end, 48);
            let (prefix, suffix) = quote
                .map(|quote| (quote.prefix, quote.suffix))
                .unwrap_or_default();
            self.host.emit(crate::RendererEvent::SelectionFinished {
                doc,
                cfi_range,
                exact_text,
                prefix,
                suffix,
            });
        }
    }

    pub fn copy_selection_to_clipboard(&self) -> Result<bool, String> {
        if let Some(text) = self
            .note_parts()
            .and_then(|(note, owner)| note.selection_text(owner)?.markdown)
        {
            self.host.set_clipboard(&text)?;
            return Ok(true);
        }
        if self.page.table_selection.is_active() {
            return self.copy_table_selection();
        }
        let Some(range) = self.page.selection.range() else {
            return Ok(false);
        };
        let target = if self.page.selection.interaction().is_semantic() {
            ExportTarget::SemanticText(range)
        } else {
            ExportTarget::Text(range)
        };
        let Some(export) = self.page.session.export() else {
            return Ok(false);
        };
        let payload = export.request(target, ExportFormat::Markdown)?;
        let Some(text) = payload.and_then(ExportPayload::into_text) else {
            return Ok(false);
        };
        self.host.set_clipboard(&text)?;
        Ok(true)
    }

    pub(crate) fn copy_document_image_to_clipboard(
        &self,
        hit: crate::DocumentImage,
    ) -> Result<(), String> {
        if hit.doc != self.nav.location().document() {
            return Err(format!("document {} is not visible", hit.doc));
        }
        let export = self.page.session.export().ok_or("document is not loaded")?;
        let payload = export
            .request(ExportTarget::Image(hit.image), ExportFormat::Image)?
            .ok_or_else(|| format!("image {} is not decoded yet", hit.image))?;
        let ExportPayload::Image(payload) = payload else {
            return Err("expected an image export".into());
        };
        match payload {
            html::export::ImagePayload::Raster {
                width,
                height,
                rgba,
            } => self.host.set_clipboard_image(width, height, rgba.to_vec()),
            html::export::ImagePayload::Svg { bytes } => {
                self.host.set_clipboard_svg(bytes.to_vec())
            }
        }
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
        && previous.page_offset_bits == requested.page_offset_bits
        && previous.direction == requested.direction
}

#[cfg(test)]
mod tests {
    use html::resources::image_metric_changes as apply_image_poll_updates;

    fn test_resources() -> Vec<(u32, u32, Option<u32>, Option<u32>)> {
        vec![
            (64, 32, None, None),
            (12, 12, Some(12), Some(12)),
            (40, 80, Some(40), None),
        ]
    }

    fn base_inputs() -> html::pipeline::PipelineInputs {
        html::pipeline::PipelineInputs {
            source: "<html />".into(),
            markup_syntax: html::pipeline::MarkupSyntax::Html,
            user_styles: Vec::new(),
            reader_overrides: Default::default(),
            note_flow: Default::default(),
            source_revision: html::pipeline::SourceRevision::INITIAL,
            base_uri: "doc.html".to_string(),
            resource_revision: html::pipeline::ResourceRevision::INITIAL,
            stylesheet_revision: html::pipeline::StylesheetRevision::INITIAL,
            style_environment: html::pipeline::StyleEnvironment::default(),
            font_environment: html::pipeline::FontEnvironmentRevision::INITIAL,
            image_metrics_revision: html::pipeline::ImageMetricsRevision::INITIAL,
            layout: html::pipeline::LayoutConstraints {
                viewport_width: 600.0,
                viewport_height: None,
                line_height: 16.0,
                image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
                text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
            },
            image_metrics: html::layout::ImageMetrics::default(),
            paint: html::pipeline::PaintSettingsRevision::INITIAL,
        }
    }

    #[test]
    fn image_poll_no_layout_needed_when_dimensions_do_not_change() {
        let resources = test_resources();
        let mut next_inputs = base_inputs();
        next_inputs.image_metrics = html::layout::ImageMetrics::default();

        let (updated, layout_needed) = apply_image_poll_updates(
            &next_inputs,
            &[0, 1, 2],
            |idx| resources.get(idx as usize).copied(),
            |idx| match idx {
                0 => Some((64, 32)),
                1 => Some((16, 16)),
                2 => Some((40, 80)),
                _ => None,
            },
        );

        assert!(
            !layout_needed,
            "intrinsic-only image changes should not force layout if no intrinsic dimensions changed"
        );
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(0),
            Some((64, 32))
        );
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(1),
            Some((16, 16))
        );
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(2),
            Some((40, 80))
        );
    }

    #[test]
    fn image_poll_out_of_order_batch_has_stable_result_and_single_revision_bump() {
        let resources = test_resources();
        let mut base = base_inputs();
        base.image_metrics.set(0, 10, 10);
        base.image_metrics.set(1, 11, 12);
        base.image_metrics.set(2, 13, 14);

        let (updated_ab, layout_ab) = apply_image_poll_updates(
            &base,
            &[2, 0, 1],
            |idx| resources.get(idx as usize).copied(),
            |idx| match idx {
                0 => Some((20, 10)),
                1 => Some((11, 12)),
                2 => Some((13, 99)),
                _ => None,
            },
        );

        let (updated_ba, layout_ba) = apply_image_poll_updates(
            &base,
            &[1, 2, 0],
            |idx| resources.get(idx as usize).copied(),
            |idx| match idx {
                0 => Some((20, 10)),
                1 => Some((11, 12)),
                2 => Some((13, 99)),
                _ => None,
            },
        );

        assert!(layout_ab);
        assert!(layout_ba);

        assert_eq!(
            updated_ab.image_metrics.as_ref().unwrap().get(0),
            updated_ba.image_metrics.as_ref().unwrap().get(0)
        );
        assert_eq!(
            updated_ab.image_metrics.as_ref().unwrap().get(1),
            updated_ba.image_metrics.as_ref().unwrap().get(1)
        );
        assert_eq!(
            updated_ab.image_metrics.as_ref().unwrap().get(2),
            updated_ba.image_metrics.as_ref().unwrap().get(2)
        );

        // A caller applying this batch should only need one revision bump.
        assert_eq!(
            updated_ab.image_metrics_revision,
            Some(base.image_metrics_revision.next())
        );
        assert_eq!(
            updated_ba.image_metrics_revision,
            Some(base.image_metrics_revision.next())
        );
    }

    #[test]
    fn image_poll_with_out_of_order_batch_applies_one_revision_bump() {
        let resources = test_resources();
        let mut base = base_inputs();
        base.image_metrics.set(0, 10, 10);
        base.image_metrics.set(1, 11, 12);
        base.image_metrics.set(2, 13, 14);

        let (updated, layout_needed) = apply_image_poll_updates(
            &base,
            &[2, 0, 1],
            |idx| resources.get(idx as usize).copied(),
            |idx| match idx {
                0 => Some((20, 10)),
                1 => Some((11, 12)),
                2 => Some((13, 99)),
                _ => None,
            },
        );

        assert!(layout_needed);
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(0),
            Some((20, 10))
        );
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(1),
            Some((11, 12))
        );
        assert_eq!(
            updated.image_metrics.as_ref().unwrap().get(2),
            Some((13, 99))
        );
        assert_eq!(
            updated.image_metrics_revision,
            Some(base.image_metrics_revision.next())
        );
    }
}
