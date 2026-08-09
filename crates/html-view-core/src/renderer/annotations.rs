use crate::doc_query::DocQuery;
use crate::{AnnotationOverlayState, MediaOverlayHighlightState, Painter, RendererCore, RendererHost};
use kurbo::{Point, Rect};
use peniko::Color;

impl<'a> DocQuery<'a> {
    pub(crate) fn glyph_range_for_anchor(self, fragment: &str) -> Option<(u32, u32)> {
        self.view().addressing().glyph_range_for_anchor(fragment).map(|range| (range.start, range.end))
    }

    /// Locates an annotation whose CFI no longer resolves, by matching its
    /// quoted text against the document with the recorded prefix and suffix as
    /// disambiguators. Returns `None` for annotations belonging to another
    /// spine document.
    fn recover_annotation_range(self, annotation: &crate::RendererAnnotation, current_doc: usize) -> Option<(u32, u32)> {
        if crate::cfi::parse_cfi_spine_only(&annotation.cfi_range)? != current_doc {
            return None;
        }
        let view = self.text();
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
}

impl AnnotationOverlayState {
    /// Recomputes which annotations fall in the document now on screen,
    /// preferring the recorded CFI and falling back to text matching.
    pub(crate) fn resolve(&mut self, doc: DocQuery<'_>, current_doc: usize) {
        self.visible.clear();
        for annotation in &self.source {
            let by_cfi = crate::cfi::parse_cfi_range(doc.document(), &annotation.cfi_range).filter(|(document, _, _)| *document == current_doc).map(|(_, start, end)| (start, end));
            if let Some((start, end)) = by_cfi.or_else(|| doc.recover_annotation_range(annotation, current_doc)) {
                self.visible.push((annotation.id.clone(), start, end, annotation.style, annotation.color));
            }
        }
    }

    /// Reports the topmost annotation covering `glyph`. Later annotations win,
    /// matching paint order.
    pub(crate) fn activate_at_glyph(&self, glyph: u32, host: &dyn RendererHost) -> bool {
        let Some((id, ..)) = self.visible.iter().rev().find(|(_, start, end, _, _)| glyph >= *start && glyph < *end) else {
            return false;
        };
        host.emit(crate::RendererEvent::AnnotationActivated { id: id.clone() });
        true
    }
}

impl MediaOverlayHighlightState {
    pub(crate) fn target_href(&self) -> Option<&str> {
        self.target_href.as_deref()
    }

    pub(crate) fn set_visible(&mut self, range: Option<(u32, u32)>) {
        self.visible = range;
    }
}

impl RendererCore {
    pub(crate) fn resolve_media_overlay_highlight(&mut self) {
        self.media_overlay.set_visible(None);
        let Some(href) = self.media_overlay.target_href().map(str::to_owned) else { return };
        let Some((doc, Some(fragment))) = self.resolve_href_target(&href) else { return };
        if doc != self.nav.current_doc_index {
            return;
        }
        let range = self.doc().glyph_range_for_anchor(&fragment);
        self.media_overlay.set_visible(range);
    }

    pub(crate) fn paint_media_overlay_for_line(&self, painter: &mut impl Painter, line: &html::layout::RenderLine, screen: Point) {
        let Some((range_start, range_end)) = self.media_overlay.visible else { return };
        for (start, end) in self.doc().line_text_intersections(line.index(), range_start, range_end) {
            let (x0, x1) = self.glyph_x_span_in_line(line, start, end);
            let (x0, x1) = (screen.x + x0, screen.x + x1);
            if x1 > x0 {
                painter.fill_rect(Rect::new(x0, screen.y, x1, screen.y + line.height()), Color::rgba8(82, 139, 255, 54));
            }
        }
    }

    pub(crate) fn activate_annotation_at_glyph(&self, glyph: u32) -> bool {
        self.annotations.activate_at_glyph(glyph, self.host.as_ref())
    }

    pub(crate) fn resolve_visible_annotations(&mut self) {
        let current_doc = self.nav.current_doc_index;
        let doc = DocQuery::new(&self.document);
        self.annotations.resolve(doc, current_doc);
    }

    pub(crate) fn paint_annotations_for_line(&self, painter: &mut impl Painter, line_idx: usize, line: &html::layout::RenderLine, screen: Point) {
        for (_, range_start, range_end, style, rgba) in &self.annotations.visible {
            for (start, end) in self.doc().line_text_intersections(line_idx, *range_start, *range_end) {
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
