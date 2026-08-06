use crate::{Painter, RendererCore};
use kurbo::{Point, Rect};
use peniko::Color;

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.visible = None;
        let Some(href) = self.media_overlay.target_href.as_deref() else { return };
        let Some((doc, Some(fragment))) = self.resolve_href_target(href) else { return };
        if doc != self.nav.current_doc_index {
            return;
        }
        self.media_overlay.visible = self.document.render_view().addressing().glyph_range_for_anchor(&fragment).map(|range| (range.start, range.end));
    }

    pub(crate) fn paint_media_overlay_for_line(&self, painter: &mut impl Painter, line: &html::layout::RenderLine, screen: Point) {
        let Some((range_start, range_end)) = self.media_overlay.visible else { return };
        for (start, end) in self.line_text_intersections(line.index(), range_start, range_end) {
            let (x0, x1) = self.glyph_x_span_in_line(line, start, end);
            let (x0, x1) = (screen.x + x0, screen.x + x1);
            if x1 > x0 {
                painter.fill_rect(Rect::new(x0, screen.y, x1, screen.y + line.height()), Color::rgba8(82, 139, 255, 54));
            }
        }
    }

    fn recover_annotation_range(&self, annotation: &crate::RendererAnnotation) -> Option<(u32, u32)> {
        if crate::cfi::parse_cfi_spine_only(&annotation.cfi_range)? != self.nav.current_doc_index {
            return None;
        }
        let view = self.document.render_view().text();
        let document = view.glyph_slice(0..view.glyph_count() as u32)?.iter().map(|glyph| view.glyph_metric(*glyph).map(|metric| metric.ch())).collect::<Option<Vec<_>>>()?;
        let exact: Vec<char> = annotation.exact_text.chars().collect();
        if exact.is_empty() || exact.len() > document.len() {
            return None;
        }
        let prefix: Vec<char> = annotation.prefix.as_deref().unwrap_or_default().chars().collect();
        let suffix: Vec<char> = annotation.suffix.as_deref().unwrap_or_default().chars().collect();
        (0..=document.len() - exact.len()).find_map(|start| {
            let end = start + exact.len();
            if document[start..end] != exact {
                return None;
            }
            let prefix_matches = prefix.is_empty() || document[..start].ends_with(&prefix);
            let suffix_matches = suffix.is_empty() || document[end..].starts_with(&suffix);
            (prefix_matches && suffix_matches).then_some((start as u32, end as u32))
        })
    }

    pub(crate) fn activate_annotation_at_glyph(&self, glyph: u32) -> bool {
        let Some((id, ..)) = self.annotations.visible.iter().rev().find(|(_, start, end, _, _)| glyph >= *start && glyph < *end) else {
            return false;
        };
        self.host.emit(crate::RendererEvent::AnnotationActivated { id: id.clone() });
        true
    }

    pub(crate) fn resolve_visible_annotations(&mut self) {
        self.annotations.visible.clear();
        for annotation in &self.annotations.source {
            let resolved =
                crate::cfi::parse_cfi_range(&self.document, &annotation.cfi_range).filter(|(doc, _, _)| *doc == self.nav.current_doc_index).map(|(_, start, end)| (start, end)).or_else(|| self.recover_annotation_range(annotation));
            if let Some((start, end)) = resolved {
                self.annotations.visible.push((annotation.id.clone(), start, end, annotation.style, annotation.color));
            }
        }
    }

    pub(crate) fn paint_annotations_for_line(&self, painter: &mut impl Painter, line_idx: usize, line: &html::layout::RenderLine, screen: Point) {
        for (_, range_start, range_end, style, rgba) in &self.annotations.visible {
            for (start, end) in self.line_text_intersections(line_idx, *range_start, *range_end) {
                let (x0, x1) = self.glyph_x_span_in_line(line, start, end);
                let (x0, x1) = (screen.x + x0, screen.x + x1);
                if x1 <= x0 {
                    continue;
                }
                let color = Color::rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
                let thickness = 2.0;
                let rect = match style {
                    crate::AnnotationStyle::Highlight => Rect::new(x0, screen.y, x1, screen.y + line.height()),
                    crate::AnnotationStyle::Underline | crate::AnnotationStyle::Squiggly => Rect::new(x0, screen.y + line.height() - thickness, x1, screen.y + line.height()),
                    crate::AnnotationStyle::Strikethrough => Rect::new(x0, screen.y + line.height() / 2.0, x1, screen.y + line.height() / 2.0 + thickness),
                };
                painter.fill_rect(rect, color);
            }
        }
    }
}
