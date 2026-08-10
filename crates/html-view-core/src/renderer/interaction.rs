use std::time::Duration;

use kurbo::{Point, Size};

use crate::prepared_page::PreparedPage;
use crate::{FrameGeometryCacheKey, GlyphShaper, PaintDirection, RendererCore};
use html::pipeline::PipelineInputs;

impl RendererCore {
    fn install_prepared_page(&mut self, prepared: PreparedPage) {
        self.viewport.direction = PaintDirection::Forward;
        self.viewport.start_offset_y = prepared.frame.paint_start_offset_y;
        self.viewport.end_offset_y = prepared.frame.paint_start_offset_y;
        self.viewport.back_start_offset_y = 0.0;
        self.viewport.back_anchor_end_line = None;
        self.viewport.reached_end = prepared.reached_end;
        self.viewport.next_start_offset_y = prepared.next_start_offset_y;
        self.frame = prepared.frame;
    }

    fn restore_prepared_page(&mut self, key: FrameGeometryCacheKey) -> bool {
        let Some(prepared) = self.prepared_pages.take(key) else {
            return false;
        };
        self.install_prepared_page(prepared);
        true
    }

    pub(crate) fn cache_current_prepared_page(&mut self) {
        let Some(key) = self.frame.geometry_key else {
            return;
        };
        let frame = std::mem::take(&mut self.frame);
        self.prepared_pages.insert(PreparedPage { key, frame, reached_end: self.viewport.reached_end, next_start_offset_y: self.viewport.next_start_offset_y });
    }

    pub(crate) fn restore_prepared_page_ending_at(&mut self, end_line: usize) -> bool {
        let current = self.frame_geometry_cache_key();
        let Some(prepared) = self.prepared_pages.take_ending_at(end_line, current) else {
            return false;
        };
        self.install_prepared_page(prepared);
        true
    }

    fn prefetch_next_prepared_page(&mut self) {
        if self.viewport.direction != PaintDirection::Forward || self.viewport.reached_end {
            return;
        }

        let mut next_start_offset_y = self.viewport.next_start_offset_y;
        if let Some(end_idx) = self.frame.current_page_end_line {
            let next_idx = end_idx + 1;
            if let Some(line) = self.document.render_view().text().line(next_idx) {
                next_start_offset_y = line.point().y;
            }
        }
        if next_start_offset_y <= self.viewport.start_offset_y {
            return;
        }

        let saved_viewport =
            (self.viewport.direction, self.viewport.start_offset_y, self.viewport.end_offset_y, self.viewport.back_start_offset_y, self.viewport.back_anchor_end_line, self.viewport.reached_end, self.viewport.next_start_offset_y);
        self.viewport.direction = PaintDirection::Forward;
        self.viewport.start_offset_y = next_start_offset_y;
        self.viewport.end_offset_y = next_start_offset_y;
        self.viewport.back_start_offset_y = 0.0;
        self.viewport.back_anchor_end_line = None;
        self.viewport.reached_end = false;
        self.viewport.next_start_offset_y = next_start_offset_y;
        let key = self.frame_geometry_cache_key();
        if self.prepared_pages.contains(key) {
            (self.viewport.direction, self.viewport.start_offset_y, self.viewport.end_offset_y, self.viewport.back_start_offset_y, self.viewport.back_anchor_end_line, self.viewport.reached_end, self.viewport.next_start_offset_y) =
                saved_viewport;
            return;
        }

        let current_frame = std::mem::take(&mut self.frame);
        self.prepare_forward();
        self.frame.geometry_key = Some(key);
        let prepared = PreparedPage { key, frame: std::mem::take(&mut self.frame), reached_end: self.viewport.reached_end, next_start_offset_y: self.viewport.next_start_offset_y };
        self.frame = current_frame;
        (self.viewport.direction, self.viewport.start_offset_y, self.viewport.end_offset_y, self.viewport.back_start_offset_y, self.viewport.back_anchor_end_line, self.viewport.reached_end, self.viewport.next_start_offset_y) =
            saved_viewport;
        self.prepared_pages.insert(prepared);
    }

    fn schedule_or_prefetch_adjacent_page(&mut self) {
        let Some(current_key) = self.frame.geometry_key else {
            return;
        };
        if self.prepared_pages.claim_scheduled(current_key) {
            self.prefetch_next_prepared_page();
        } else if !self.prepared_pages.is_prefetched(current_key) {
            self.prepared_pages.mark_scheduled(current_key);
            self.host.schedule_repaint(Duration::from_millis(25));
        }
    }

    pub(crate) fn open_image_at(&self, position: Point) -> bool {
        let Some(image_idx) = self.hit_test_image(position) else { return false };
        let Some(uri) = self.document.render_view().image_uri(image_idx) else { return false };
        let Ok(bytes) = self.provider.read_bytes(uri) else { return false };
        self.host.emit(crate::RendererEvent::ImageOpened { uri: uri.to_owned(), bytes });
        true
    }

    pub(crate) fn emit_visible_text(&self) {
        let text = self.visible_text();
        if !text.is_empty() {
            self.host.emit(crate::RendererEvent::SpeakableText(text));
        }
    }

    pub(crate) fn visible_text(&self) -> String {
        let text_view = self.document.render_view().text();
        let mut visible_glyphs = Vec::new();
        for (line_index, _) in self.frame.last_line_positions.iter() {
            let Some(fragments) = text_view.line_text_fragments(line_index) else { continue };
            for fragment in fragments {
                visible_glyphs.extend(fragment.glyphs());
            }
        }
        visible_glyphs.sort_unstable();
        visible_glyphs.dedup();

        let mut text = String::new();
        let mut previous = None;
        for glyph_index in visible_glyphs {
            if let Some(previous_index) = previous {
                let previous_line = text_view.line_index_for_glyph(previous_index);
                let current_line = text_view.line_index_for_glyph(glyph_index);
                if previous_line != current_line {
                    let lines_overlap = previous_line
                        .zip(current_line)
                        .and_then(|(previous_line, current_line)| Some((text_view.line(previous_line)?, text_view.line(current_line)?)))
                        .is_some_and(|(previous_line, current_line)| previous_line.start() < current_line.end() && current_line.start() < previous_line.end());
                    if !lines_overlap {
                        text.push('\n');
                    }
                }
            }
            if let Some(glyph) = text_view.glyph_at(glyph_index as usize)
                && let Some(metric) = text_view.glyph_metric(glyph)
            {
                text.push(metric.ch());
            }
            previous = Some(glyph_index);
        }
        text.trim().to_owned()
    }

    fn apply_image_poll(&mut self, glyph_shaper: &mut impl GlyphShaper, image_poll: crate::resources::ImagePipelinePoll) {
        if image_poll.decoded {
            self.request_resource_repaint();
        }

        if image_poll.dimensions_changed.is_empty() {
            return;
        }

        let images = self.document.render_view().images();
        let (next_inputs, layout_needed) = apply_image_poll_updates(
            &self.pipeline_inputs,
            &image_poll.dimensions_changed,
            |image_idx| images.get(image_idx as usize).map(|resource| (resource.width, resource.height, resource.width_attr, resource.height_attr)),
            |image_idx| self.image_pipeline.get_decoded(image_idx).map(html::resources::DecodedImage::dimensions),
        );

        if !layout_needed {
            return;
        }

        let mut next_inputs = next_inputs;
        next_inputs.image_metrics_revision = next_inputs.image_metrics_revision.next();
        if let Err(error) = self.apply_pipeline_inputs(next_inputs, glyph_shaper) {
            self.emit_operation_failed(crate::RendererOperation::UpdateDocument, error.to_string());
        }
    }

    /// Updates viewport-dependent layout and incorporates completed image work.
    pub fn prepare_frame(&mut self, glyph_shaper: &mut impl GlyphShaper, viewport_size: Size) {
        let image_poll = self.image_pipeline.poll();
        self.apply_image_poll(glyph_shaper, image_poll);
        self.configure_layout_for_viewport(glyph_shaper, viewport_size);
        let requested_geometry = self.frame_geometry_cache_key();
        if self.frame.geometry_key != Some(requested_geometry) {
            if !self.restore_prepared_page(requested_geometry) {
                match self.viewport.direction {
                    PaintDirection::Forward => self.prepare_forward(),
                    PaintDirection::Backward => self.prepare_backward(),
                }
                self.frame.geometry_key = Some(self.frame_geometry_cache_key());
            }
        }
        self.prefetch_images_for_current_page();
        if self.nav.pending_nav_anchor_update {
            self.update_nav_anchor_from_layout();
            self.update_nav_signal();
            self.nav.pending_nav_anchor_update = false;
        }
        self.update_nav_signal();
        self.schedule_or_prefetch_adjacent_page();
    }

    pub(crate) fn configure_layout_for_viewport(&mut self, glyph_shaper: &mut impl GlyphShaper, viewport_size: Size) {
        const MIN_GAP: f64 = 20.0;
        self.layout.size = viewport_size;
        self.layout.size /= self.layout.scale;
        let effective_col_width = if self.layout.size.width > 0.0 { self.layout.base_col_width.min(self.layout.size.width) } else { self.layout.base_col_width };
        let column_width_changed = (effective_col_width - self.layout.col_width).abs() > 24.0;
        let requested_col_width = if column_width_changed { effective_col_width } else { self.layout.col_width };
        let viewport_height = (self.layout.size.height.is_finite() && self.layout.size.height > 0.0).then_some(self.layout.size.height);
        let media_environment = html::pipeline::MediaEnvironment::screen(requested_col_width, viewport_height).expect("effective viewport dimensions must be finite and positive");
        let media_environment_changed = self.pipeline_inputs.style_environment.media != media_environment;
        if column_width_changed || media_environment_changed {
            match self.relayout_for_column_width(glyph_shaper, requested_col_width) {
                Ok(()) => self.layout.col_width = requested_col_width,
                Err(error) => self.emit_operation_failed(crate::RendererOperation::UpdateDocument, error.to_string()),
            }
        }
        let raw_col_count = ((self.layout.size.width + MIN_GAP) / (self.layout.col_width + MIN_GAP)).floor();
        self.layout.col_count = self.layout.max_col_count.map_or(raw_col_count.max(1.0), |maximum| raw_col_count.max(1.0).min(f64::from(maximum)));
        self.layout.col_gap = ((self.layout.size.width - self.layout.col_count * self.layout.col_width) / (self.layout.col_count + 1.0)).max(0.0);
    }

    /// Returns whether a point in renderer-layout coordinates is over the
    /// painted bounds of a linked glyph.
    pub fn link_at(&self, position: Point) -> bool {
        let Some(glyph_idx) = self.hit_test_glyph(position) else {
            return false;
        };
        if self.document.render_view().addressing().link_for_glyph(glyph_idx).is_none() {
            return false;
        }
        let Some(line_idx) = self.doc().find_line_for_glyph(glyph_idx) else {
            return false;
        };
        let Some(screen) = self.frame.last_line_positions.get(line_idx) else {
            return false;
        };
        let Some(line) = self.document.render_view().text().line(line_idx) else {
            return false;
        };
        let (x0, x1) = self.glyph_x_span_in_line(&line, glyph_idx, glyph_idx.saturating_add(1).min(line.end()));
        let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
        position.x >= x0 && position.x <= x1
    }

    pub fn begin_selection_at(&mut self, position: Point, semantic_selection: bool) -> bool {
        let Some(glyph_idx) = self.hit_test_glyph(position) else {
            return false;
        };
        self.selection.selection_anchor = Some(glyph_idx);
        self.selection.selection_active = Some(glyph_idx);
        self.selection.is_selecting = true;
        self.selection.semantic_selection = semantic_selection;
        self.selection.pending_link_glyph = Some(glyph_idx);
        self.update_selection_text();
        self.request_overlay_repaint();
        true
    }

    pub fn update_selection_at(&mut self, position: Point, semantic_selection: bool) -> bool {
        if !self.selection.is_selecting {
            return false;
        }
        self.selection.semantic_selection |= semantic_selection;
        let Some(glyph_idx) = self.hit_test_glyph(position) else {
            return false;
        };
        self.selection.selection_active = Some(glyph_idx);
        if self.selection.selection_anchor != self.selection.selection_active {
            self.selection.pending_link_glyph = None;
        }
        self.update_selection_text();
        self.request_overlay_repaint();
        true
    }

    /// Completes selection and returns the glyph that still qualifies as a
    /// click. The UI backend decides whether and how that click is activated.
    pub fn finish_selection(&mut self) -> Option<u32> {
        self.selection.is_selecting = false;
        self.update_selection_text();
        if let (Some((start, end)), Some(exact_text)) = (self.selection_range(), self.selection.selected_text.clone())
            && let Some(cfi_range) = crate::cfi::generate_cfi_range(&self.document, self.nav.current_doc_index, start, end)
        {
            let prefix_start = start.saturating_sub(48);
            let glyph_count = self.document.render_view().text().glyph_count() as u32;
            let suffix_end = end.saturating_add(48).min(glyph_count);
            let prefix = self.doc().build_selection_text(prefix_start, start).0.filter(|value| !value.is_empty());
            let suffix = self.doc().build_selection_text(end, suffix_end).0.filter(|value| !value.is_empty());
            self.host.emit(crate::RendererEvent::SelectionFinished { doc: self.nav.current_doc_index, cfi_range, exact_text, prefix, suffix });
        }
        let click = match (self.selection.selection_anchor, self.selection.selection_active, self.selection.pending_link_glyph) {
            (Some(anchor), Some(active), Some(pending)) if anchor == active && pending == anchor => Some(anchor),
            _ => None,
        };
        self.selection.pending_link_glyph = None;
        click
    }

    pub fn copy_selection_to_clipboard(&self) -> Result<bool, String> {
        let Some(text) = self.selection.selected_text_markdown.as_deref() else {
            return Ok(false);
        };
        self.host.set_clipboard(text)?;
        Ok(true)
    }

    pub fn copy_image_to_clipboard(&self, image_idx: u32) -> Result<(), String> {
        let decoded = self.image_pipeline.get_decoded(image_idx).ok_or_else(|| format!("image {image_idx} is not decoded yet"))?;
        match decoded {
            html::resources::DecodedImage::Raster { image, .. } => self.host.set_clipboard_image(image.width as usize, image.height as usize, image.data.data().to_vec()),
            html::resources::DecodedImage::Svg { bytes, .. } => self.host.set_clipboard_svg(bytes.to_vec()),
        }
    }
}

fn apply_image_poll_updates(
    pipeline_inputs: &PipelineInputs, dimensions_changed: &[u32], image_metadata: impl Fn(u32) -> Option<(u32, u32, Option<u32>, Option<u32>)>, decode_sizes: impl Fn(u32) -> Option<(u32, u32)>,
) -> (PipelineInputs, bool) {
    let mut next_inputs = pipeline_inputs.clone();
    let mut layout_needed = false;

    for image_idx in dimensions_changed {
        let (resource_width, resource_height, width_attr, height_attr) = match image_metadata(*image_idx) {
            Some((resource_width, resource_height, width_attr, height_attr)) => (resource_width, resource_height, width_attr, height_attr),
            None => continue,
        };
        let Some((decoded_width, decoded_height)) = decode_sizes(*image_idx) else {
            continue;
        };

        let uses_intrinsic_width = width_attr.is_none();
        let uses_intrinsic_height = height_attr.is_none();
        let previous = next_inputs.image_metrics.get(*image_idx).unwrap_or((resource_width, resource_height));
        if (uses_intrinsic_width && previous.0 != decoded_width) || (uses_intrinsic_height && previous.1 != decoded_height) {
            layout_needed = true;
        }
        next_inputs.image_metrics.set(*image_idx, decoded_width, decoded_height);
    }

    (next_inputs, layout_needed)
}

#[cfg(test)]
mod tests {
    use super::apply_image_poll_updates;
    use crate::text_backend::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
        }

        fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, crate::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).unwrap();
            let glyph = glyph_metrics.register(metric).unwrap();
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn test_resources() -> Vec<(u32, u32, Option<u32>, Option<u32>)> {
        vec![(64, 32, None, None), (12, 12, Some(12), Some(12)), (40, 80, Some(40), None)]
    }

    fn base_inputs() -> html::pipeline::PipelineInputs {
        html::pipeline::PipelineInputs {
            source: "<html />".to_string(),
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

        assert!(!layout_needed, "intrinsic-only image changes should not force layout if no intrinsic dimensions changed");
        assert_eq!(updated.image_metrics.get(0), Some((64, 32)));
        assert_eq!(updated.image_metrics.get(1), Some((16, 16)));
        assert_eq!(updated.image_metrics.get(2), Some((40, 80)));
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

        assert_eq!(updated_ab.image_metrics.get(0), updated_ba.image_metrics.get(0));
        assert_eq!(updated_ab.image_metrics.get(1), updated_ba.image_metrics.get(1));
        assert_eq!(updated_ab.image_metrics.get(2), updated_ba.image_metrics.get(2));

        // A caller applying this batch should only need one revision bump.
        assert_eq!(updated_ab.image_metrics_revision.next(), base.image_metrics_revision.next());
        assert_eq!(updated_ba.image_metrics_revision.next(), base.image_metrics_revision.next());
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
        assert_eq!(updated.image_metrics.get(0), Some((20, 10)));
        assert_eq!(updated.image_metrics.get(1), Some((11, 12)));
        assert_eq!(updated.image_metrics.get(2), Some((13, 99)));
        assert_eq!(updated.image_metrics.get(0), Some((20, 10)));
    }
}
