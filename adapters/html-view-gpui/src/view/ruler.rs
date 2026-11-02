//! Reading guides use the renderer's line geometry without owning selection.

use super::HtmlView;
use gpui::{Bounds, ContentMask, Context, Hsla, Pixels, Window, black, fill, point, px, size};
use html_gpui::backend::GpuiTextCache;
use skrift_column_paginator::{
    ReadingLine, RendererSession,
    cfi::{generate_cfi, parse_cfi},
};
use kurbo::{Rect, Size};
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReadingRulerStyle {
    Band,
    Focus,
    Underline,
    #[default]
    #[serde(other)]
    Off,
}

#[derive(Clone, Default)]
pub(super) struct ReadingRuler {
    style: ReadingRulerStyle,
    line_count: usize,
    accent: Option<Hsla>,
    margins: Size,
    anchor: Option<String>,
    glyph: Option<(usize, u32)>,
    /// The anchor is the last line of a group reached by moving backwards.
    from_end: bool,
    edge: Option<bool>,
    navigation: u64,
    layout_revision: Option<u64>,
}

fn same_region(a: &ReadingLine, b: &ReadingLine) -> bool {
    a.column == b.column && a.cell == b.cell && (a.bounds.x0 - b.bounds.x0).abs() < 1.0 && (a.bounds.x1 - b.bounds.x1).abs() < 1.0
}

impl ReadingRuler {
    fn index(&self, renderer: &RendererSession<GpuiTextCache>, lines: &[ReadingLine]) -> Option<usize> {
        if lines.is_empty() {
            return None;
        }
        if let Some(forward) = self.edge {
            return Some(if forward { 0 } else { lines.len() - 1 });
        }
        let glyph = self
            .anchor
            .as_deref()
            .and_then(|cfi| parse_cfi(renderer.render_view(), cfi))
            .filter(|(document, _)| Some(*document) == renderer.reading_document())
            .or(self.glyph.filter(|(document, _)| Some(*document) == renderer.reading_document()))
            .map(|(_, glyph)| glyph);
        Some(glyph.and_then(|glyph| lines.iter().position(|line| line.glyphs.contains(&glyph))).unwrap_or(0))
    }
}

impl HtmlView {
    /// Extend the focus mask into the surrounding reader padding.
    pub fn set_reading_ruler_margins(&mut self, horizontal: f32, vertical: f32, cx: &mut Context<Self>) {
        let margins = Size::new(f64::from(horizontal.max(0.0)), f64::from(vertical.max(0.0)));
        if self.ruler.margins != margins {
            self.ruler.margins = margins;
            cx.notify();
        }
    }

    /// Supply the reader theme's accent for the band and guide lines.
    pub fn set_reading_ruler_accent(&mut self, accent: Hsla, cx: &mut Context<Self>) {
        if self.ruler.accent != Some(accent) {
            self.ruler.accent = Some(accent);
            cx.notify();
        }
    }

    pub fn set_reading_ruler(&mut self, style: ReadingRulerStyle, lines: usize, cx: &mut Context<Self>) {
        let lines = lines.clamp(1, 5);
        if self.ruler.style != style || self.ruler.line_count != lines {
            self.ruler.style = style;
            self.ruler.line_count = lines;
            cx.notify();
        }
    }

    pub(super) fn reset_ruler(&mut self, forward: bool) {
        self.ruler.anchor = None;
        self.ruler.glyph = None;
        self.ruler.edge = Some(forward);
        self.ruler.from_end = !forward;
        self.ruler.navigation = self.renderer.navigation_revision();
    }

    pub(super) fn ruler_page_turn_completed(&mut self, forward: bool, moved: bool) {
        if moved {
            self.reset_ruler(forward);
        }
        // Even a rejected page turn updates the session's navigation revision.
        self.ruler.navigation = self.renderer.navigation_revision();
    }

    /// Returns false only when the ruler is disabled. At a page boundary the
    /// same input turns the page and resumes at its first/last readable line.
    pub fn step_reading_ruler(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        if self.ruler.style == ReadingRulerStyle::Off {
            return false;
        }
        if self.renderer.reading_document().is_none() {
            return true;
        }
        self.sync_ruler_navigation();
        let lines = self.renderer.visible_reading_lines();
        let index = self.ruler.index(&self.renderer, &lines);
        let next = index.and_then(|index| {
            let group = line_group(&lines, index, self.ruler.line_count, self.ruler.from_end);
            if forward { (group.end < lines.len()).then_some(group.end) } else { group.start.checked_sub(1) }
        });
        if let Some(next) = next {
            self.ruler.from_end = !forward;
            self.anchor_ruler(&lines[next].glyphs);
        } else {
            self.turn_page(forward, cx);
        }
        cx.notify();
        true
    }

    pub(super) fn sync_ruler_navigation(&mut self) {
        if self.ruler.navigation != self.renderer.navigation_revision() {
            self.reset_ruler(true);
        }
    }

    /// Relayout can move the selected passage off the page retained by the
    /// viewport. Follow its source anchor once, rather than selecting line zero.
    pub(super) fn preserve_ruler_after_reflow(&mut self, geometry_changed: bool) -> bool {
        let revision = self.renderer.revisions().layout;
        let layout_changed = self.ruler.layout_revision.replace(revision).is_some_and(|previous| previous != revision);
        if (!geometry_changed && !layout_changed) || self.ruler.style == ReadingRulerStyle::Off || self.ruler.edge.is_some() {
            return false;
        }
        let Some(anchor) = self.ruler.anchor.as_deref() else { return false };
        let Some((document, glyph)) = parse_cfi(self.renderer.render_view(), anchor).filter(|(document, _)| Some(*document) == self.renderer.reading_document()) else { return false };
        let lines = self.renderer.visible_reading_lines();
        if lines.is_empty() || lines.iter().any(|line| line.glyphs.contains(&glyph)) {
            return false;
        }
        let restored = self.renderer.set_cfi_position(anchor);
        self.ruler.navigation = self.renderer.navigation_revision();
        if restored {
            self.ruler.glyph = Some((document, glyph));
        }
        restored
    }

    fn anchor_ruler(&mut self, range: &Range<u32>) {
        self.ruler.anchor = self.renderer.reading_document().and_then(|document| generate_cfi(self.renderer.render_view(), document, range.start));
        self.ruler.glyph = self.renderer.reading_document().map(|document| (document, range.start));
        self.ruler.edge = None;
    }

    pub(super) fn prepare_ruler(&mut self) -> Vec<(Rect, Hsla)> {
        if self.ruler.style == ReadingRulerStyle::Off {
            return Vec::new();
        }
        self.sync_ruler_navigation();
        let lines = self.renderer.visible_reading_lines();
        let Some(index) = self.ruler.index(&self.renderer, &lines) else { return Vec::new() };
        if self.ruler.edge.is_some() || self.ruler.glyph.is_none() {
            self.anchor_ruler(&lines[index].glyphs);
        }
        let band = focus_bounds(&lines, index, self.ruler.line_count, self.ruler.from_end);
        let accent = self.ruler.accent.unwrap_or_else(black);
        match self.ruler.style {
            ReadingRulerStyle::Off => Vec::new(),
            ReadingRulerStyle::Band => vec![(band, accent.opacity(0.22))],
            ReadingRulerStyle::Underline => vec![(Rect::new(band.x0, band.y0, band.x1, band.y0 + 2.0), accent.opacity(0.9)), (Rect::new(band.x0, band.y1 - 2.0, band.x1, band.y1), accent.opacity(0.9))],
            ReadingRulerStyle::Focus => {
                // Navigation follows cell boundaries, but the clear strip spans
                // the page column so overlapping text in adjacent cells shows.
                let column = lines[index].column_bounds;
                let band = Rect::new(column.x0, band.y0, column.x1, band.y1);
                let size = self.prepared_viewport.unwrap_or(Size::ZERO);
                let margins = self.ruler.margins;
                let viewport = Rect::new(-margins.width, -margins.height, size.width + margins.width, size.height + margins.height);
                mask_rects(band, viewport).into_iter().map(|rect| (rect, black().opacity(0.55))).collect()
            }
        }
    }
}

/// Groups stop at cell/column/page edges, retaining a shorter group before the
/// next navigation request crosses that edge. Direction changes start outside
/// the entire current group, so adjacent groups never share a text line.
fn line_group(lines: &[ReadingLine], index: usize, count: usize, from_end: bool) -> Range<usize> {
    let current = &lines[index];
    let region_start = (0..index).rev().find(|&i| !same_region(current, &lines[i])).map_or(0, |i| i + 1);
    let region_end = (index + 1..lines.len()).find(|&i| !same_region(current, &lines[i])).unwrap_or(lines.len());
    if from_end { (index + 1).saturating_sub(count.max(1)).max(region_start)..index + 1 } else { index..index.saturating_add(count.max(1)).min(region_end) }
}

fn focus_bounds(lines: &[ReadingLine], index: usize, count: usize, from_end: bool) -> Rect {
    line_group(lines, index, count, from_end).fold(lines[index].bounds, |bounds, i| bounds.union(lines[i].bounds))
}

fn mask_rects(band: Rect, viewport: Rect) -> Vec<Rect> {
    let band = band.intersect(viewport);
    [Rect::new(viewport.x0, viewport.y0, viewport.x1, band.y0), Rect::new(viewport.x0, band.y1, viewport.x1, viewport.y1), Rect::new(viewport.x0, band.y0, band.x0, band.y1), Rect::new(band.x1, band.y0, viewport.x1, band.y1)]
        .into_iter()
        .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
        .collect()
}

pub(super) fn paint_ruler(quads: &[(Rect, Hsla)], bounds: Bounds<Pixels>, window: &mut Window) {
    let paint_bounds = |rect: &Rect| Bounds::new(bounds.origin + point(px(rect.x0 as f32), px(rect.y0 as f32)), size(px(rect.width() as f32), px(rect.height() as f32)));
    // Focus quads may extend into the reader's padding. The parent's content
    // mask still clips them to the reading surface during page transitions.
    let mask = quads.iter().fold(bounds, |mask, (rect, _)| mask.union(&paint_bounds(rect)));
    window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
        for (rect, color) in quads {
            window.paint_quad(fill(paint_bounds(rect), *color));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use skrift_column_paginator::{RendererInitialConfig, ResourceProvider};
    use std::{io, sync::Arc};

    struct RulerBook;

    struct StructuredBook(&'static str);

    impl ResourceProvider for StructuredBook {
        fn read_bytes(&self, _: &str) -> io::Result<Vec<u8>> {
            Ok(format!("<html><head><style>body {{ margin: 0 }} p {{ margin: 0; line-height: 28px }}</style></head><body>{}</body></html>", self.0).into_bytes())
        }
        fn exists(&self, uri: &str) -> bool {
            RulerBook.exists(uri)
        }
        fn resolve(&self, base: &str, href: &str) -> String {
            RulerBook.resolve(base, href)
        }
        fn list_html_candidates(&self, base: &str) -> io::Result<Vec<String>> {
            RulerBook.list_html_candidates(base)
        }
    }

    impl ResourceProvider for RulerBook {
        fn read_bytes(&self, _: &str) -> io::Result<Vec<u8>> {
            let paragraphs = (0..100).map(|index| format!("<p>Reading line {index} with enough words to test reflow.</p>")).collect::<String>();
            Ok(format!("<html><head><style>body {{ margin: 0 }} p {{ margin: 0; line-height: 28px }}</style></head><body>{paragraphs}</body></html>").into_bytes())
        }

        fn exists(&self, _: &str) -> bool {
            true
        }
        fn resolve(&self, _: &str, href: &str) -> String {
            href.to_owned()
        }
        fn list_html_candidates(&self, _: &str) -> io::Result<Vec<String>> {
            Ok(vec!["book.html".into()])
        }
    }

    #[gpui::test]
    fn focus_reveals_dialogue_beside_the_selected_speaker(cx: &mut TestAppContext) {
        let markup = "<table style='width:100%;table-layout:fixed;border-collapse:collapse'><tr><td style='width:20%;vertical-align:top'><p>Bottom</p></td><td style='vertical-align:top'><p>little voice, Thisne, Thisne</p><p>my lover dear!</p></td></tr><tr><td><p>Quince</p></td><td><p>No, no; you must play Pyramus</p></td></tr></table>";
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(StructuredBook(markup)), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            let lines = view.renderer.visible_reading_lines();
            let speaker = &lines[0];
            let dialogue = &lines[1];
            assert_ne!(speaker.cell, dialogue.cell);
            assert!(speaker.bounds.x1 <= dialogue.bounds.x0);
            view.set_reading_ruler(ReadingRulerStyle::Focus, 1, cx);
            let mask = view.prepare_ruler();
            assert_eq!(view.ruler.glyph.unwrap().1, speaker.glyphs.start);
            assert!(mask.iter().all(|(rect, _)| rect.intersect(dialogue.bounds).area() == 0.0), "the speaker's focus must also reveal the adjacent dialogue");
            assert!(mask.iter().all(|(rect, _)| rect.intersect(speaker.column_bounds).area() == 0.0));
            assert!(mask.iter().any(|(rect, _)| rect.intersect(lines[2].bounds).area() > 0.0), "later dialogue remains dimmed below the selected line");
            view.step_reading_ruler(true, cx);
            assert_eq!(view.prepare_ruler(), mask, "moving into the dialogue at the same height can reuse the same clear strip");
            assert_eq!(view.ruler.glyph.unwrap().1, dialogue.glyphs.start, "each cell's line must still count as its own navigation step");
            view.step_reading_ruler(true, cx);
            assert_ne!(view.prepare_ruler(), mask);
            view.step_reading_ruler(false, cx);
            assert_eq!(view.prepare_ruler(), mask);
        });
    }

    #[gpui::test]
    fn reading_ruler_finishes_cells_and_authored_columns_in_both_directions(cx: &mut TestAppContext) {
        let fixtures: [(&str, &[usize], bool); 3] = [
            (
                "<table style='width:100%;table-layout:fixed;border-collapse:collapse'><tr><td style='vertical-align:top;padding:8px'><p>A1</p><p>A2</p><p>A3</p></td><td style='vertical-align:top;padding:8px'><p>B1</p><p>B2</p></td></tr><tr><td colspan='2'><p>C1</p><p>C2</p></td></tr></table>",
                &[3, 2, 2],
                true,
            ),
            (
                "<div style='display:table;width:100%;table-layout:fixed'><div style='display:table-row'><section style='display:table-cell;vertical-align:top'><p>A1</p><p>A2</p><p>A3</p></section><section style='display:table-cell;vertical-align:top'><p>B1</p><p>B2</p></section></div></div>",
                &[3, 2],
                true,
            ),
            ("<div style='display:flex'><div style='width:50%'><p>A1</p><p>A2</p><p>A3</p></div><div style='width:50%'><p>B1</p><p>B2</p></div></div>", &[3, 2], false),
        ];
        for (markup, regions, cells) in fixtures {
            let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
            let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(StructuredBook(markup)), vec!["book.html".into()], 0, None, config, window, cx));
            visual.simulate_resize(size(px(600.0), px(500.0)));
            visual.run_until_parked();
            let anchor = document.update(visual, |view, cx| {
                let lines = view.renderer.visible_reading_lines();
                assert_eq!(lines.len(), regions.iter().sum::<usize>());
                assert!(lines.iter().all(|line| line.cell.is_some() == cells));
                let right = &lines[regions[0]];
                assert!(lines[0].bounds.x1 <= right.bounds.x0 + 0.01, "the guide must fit within its own cell or column: {:?} / {:?}", lines[0], right);
                assert!(lines[0].bounds.y0 < right.bounds.y1 && right.bounds.y0 < lines[0].bounds.y1, "the fixture must contain vertically overlapping independent lines: {lines:?}");
                let page = view.current_cfi();
                for count in 1..=5 {
                    view.set_reading_ruler(ReadingRulerStyle::Band, count, cx);
                    view.reset_ruler(true);
                    let mut start = 0;
                    for region_length in regions {
                        let region_end = start + region_length;
                        while start < region_end {
                            let end = (start + count).min(region_end);
                            let expected = lines[start..end].iter().fold(lines[start].bounds, |bounds, line| bounds.union(line.bounds));
                            assert_eq!(view.prepare_ruler()[0].0, expected, "forward groups must stop at cell/column boundaries");
                            assert_eq!(view.current_cfi(), page);
                            if end < lines.len() {
                                view.step_reading_ruler(true, cx);
                            }
                            start = end;
                        }
                    }
                    view.reset_ruler(false);
                    let mut end = lines.len();
                    for region_length in regions.iter().rev() {
                        let region_start = end - region_length;
                        while end > region_start {
                            let start = end.saturating_sub(count).max(region_start);
                            let expected = lines[start..end].iter().fold(lines[start].bounds, |bounds, line| bounds.union(line.bounds));
                            assert_eq!(view.prepare_ruler()[0].0, expected, "backward groups must stop at cell/column boundaries");
                            assert_eq!(view.current_cfi(), page);
                            if start > 0 {
                                view.step_reading_ruler(false, cx);
                            }
                            end = start;
                        }
                    }
                }
                view.anchor_ruler(&lines[1].glyphs);
                view.ruler.anchor.clone().unwrap()
            });
            visual.simulate_resize(size(px(480.0), px(500.0)));
            visual.run_until_parked();
            document.update(visual, |view, _| {
                view.prepare_ruler();
                let (_, glyph) = parse_cfi(view.renderer.render_view(), &anchor).unwrap();
                let lines = view.renderer.visible_reading_lines();
                let selected = view.ruler.index(&view.renderer, &lines).unwrap();
                assert!(lines[selected].glyphs.contains(&glyph), "keep the passage anchored inside its cell after resize");
            });
        }
    }

    #[gpui::test]
    fn reading_ruler_steps_then_turns_pages_in_both_directions(cx: &mut TestAppContext) {
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, RendererInitialConfig { max_column_count: Some(1), ..Default::default() }, window, cx));
        visual.simulate_resize(gpui::size(gpui::px(600.0), gpui::px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            assert!(!view.step_reading_ruler(true, cx), "disabled ruler leaves ordinary navigation available");
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            view.prepare_ruler();
            let first_page = view.current_cfi();
            let lines = view.renderer.visible_reading_lines();
            assert!(lines.len() > 3);
            view.step_reading_ruler(false, cx);
            view.prepare_ruler();
            assert_eq!(view.ruler.glyph.unwrap().1, lines[0].glyphs.start, "backward at the book start must keep the first line");
            for line in lines.iter().skip(1) {
                view.step_reading_ruler(true, cx);
                view.prepare_ruler();
                assert_eq!(view.ruler.glyph.unwrap().1, line.glyphs.start);
                assert_eq!(view.current_cfi(), first_page, "line stepping must not scroll the page");
            }
            view.step_reading_ruler(true, cx);
            view.prepare_ruler();
            assert_ne!(view.current_cfi(), first_page);
            assert_eq!(view.ruler.glyph.unwrap().1, view.renderer.visible_reading_lines()[0].glyphs.start);
            view.step_reading_ruler(false, cx);
            view.prepare_ruler();
            assert_eq!(view.current_cfi(), first_page);
            assert_eq!(view.ruler.glyph.unwrap().1, lines.last().unwrap().glyphs.start);
            for _ in 0..30 {
                if !view.next_page(cx) {
                    break;
                }
            }
            let last_page = view.current_cfi();
            let last_lines = view.renderer.visible_reading_lines();
            view.anchor_ruler(&last_lines.last().unwrap().glyphs);
            view.step_reading_ruler(true, cx);
            view.prepare_ruler();
            assert_eq!(view.current_cfi(), last_page);
            assert_eq!(view.ruler.glyph.unwrap().1, last_lines.last().unwrap().glyphs.start, "forward at the book end must keep the last line");
        });
    }

    #[gpui::test]
    fn reading_ruler_reflows_and_does_not_rebuild_page_paint(cx: &mut TestAppContext) {
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, RendererInitialConfig { max_column_count: Some(1), ..Default::default() }, window, cx));
        visual.simulate_resize(gpui::size(gpui::px(600.0), gpui::px(400.0)));
        visual.run_until_parked();
        let anchor = document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            view.prepare_ruler();
            let paint = view.paint_stats();
            view.step_reading_ruler(true, cx);
            view.prepare_ruler();
            assert_eq!(view.paint_stats(), paint, "moving the guide must retain page paint commands");
            view.ruler.anchor.clone().unwrap()
        });
        visual.simulate_resize(gpui::size(gpui::px(420.0), gpui::px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, _| {
            let (_, glyph) = skrift_column_paginator::cfi::parse_cfi(view.renderer.render_view(), &anchor).unwrap();
            let lines = view.renderer.visible_reading_lines();
            let selected = view.ruler.index(&view.renderer, &lines).unwrap();
            assert!(lines[selected].glyphs.contains(&glyph), "the ruler must follow its passage after reflow");
            assert_eq!(view.prepare_ruler()[0].0, lines[selected].bounds);
        });
    }

    #[gpui::test]
    fn reading_ruler_stays_painted_before_settings_repaint_messages_arrive(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Focus, 3, cx);
            view.set_reading_ruler_accent(gpui::rgb(0xeddba2).into(), cx);
            let mask = view.prepare_ruler();
            assert!(!mask.is_empty());
            assert!(mask.iter().all(|(_, color)| *color == black().opacity(0.55)), "focus must always dim with translucent black, independently of the theme accent");
            assert!(!view.display_dirty);
            assert!(!view.host.repaint_pending.get());
            let viewport = view.prepared_viewport.unwrap();
            let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(viewport.width as f32), px(viewport.height as f32)));
            let commands = [
                skrift_column_paginator::RendererCommand::SetFontSize(28.0),
                skrift_column_paginator::RendererCommand::SetColumnWidth(420.0),
                skrift_column_paginator::RendererCommand::SetColumnViewportPlan { available_width: viewport.width, column_width: 420.0, column_count: 1, column_gap: 48.0 },
            ];
            for command in commands {
                view.apply(command, cx);
                assert!(view.host.repaint_pending.get());
                // The settings panel can redraw the parent before its child's
                // queued repaint event is dispatched. That frame needs both
                // the updated page and its reading guide.
                view.prepared_paint_for_bounds(bounds, cx);
                assert!(!view.prepare_ruler().is_empty(), "settings must never expose a frame without the focus mask");
            }
        });
    }

    #[gpui::test]
    fn reading_ruler_crosses_columns_before_turning_the_page(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { column_width: 400.0, max_column_count: Some(2), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(900.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            let lines = view.renderer.visible_reading_lines();
            let boundary = lines.windows(2).position(|pair| pair[0].column != pair[1].column).expect("two visible columns");
            let page = view.current_cfi();
            view.anchor_ruler(&lines[boundary].glyphs);
            view.step_reading_ruler(true, cx);
            assert_eq!(view.prepare_ruler()[0].0, lines[boundary + 1].bounds);
            assert_eq!(view.current_cfi(), page);
            view.step_reading_ruler(false, cx);
            assert_eq!(view.prepare_ruler()[0].0, lines[boundary].bounds);
            view.set_reading_ruler(ReadingRulerStyle::Focus, 1, cx);
            view.reset_ruler(true);
            let mask = view.prepare_ruler();
            let other_column = lines[boundary + 1].bounds;
            let covered = mask.iter().map(|(rect, _)| rect.intersect(other_column).area()).sum::<f64>();
            assert!((covered - other_column.area()).abs() < 0.01, "expanding focus across cells must keep the other page column dimmed");
        });
    }

    #[gpui::test]
    fn reading_ruler_crosses_chapters_in_both_directions(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["first.html".into(), "second.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            // Reach the end of the first chapter using ordinary page turns.
            for _ in 0..30 {
                if view.renderer.reading_document() != Some(0) {
                    break;
                }
                let lines = view.renderer.visible_reading_lines();
                view.anchor_ruler(&lines.last().unwrap().glyphs);
                view.step_reading_ruler(true, cx);
            }
        });
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            assert_eq!(view.renderer.reading_document(), Some(1));
            view.prepare_ruler();
            assert_eq!(view.ruler.glyph.unwrap().1, view.renderer.visible_reading_lines()[0].glyphs.start);
            view.step_reading_ruler(false, cx);
        });
        visual.run_until_parked();
        document.update(visual, |view, _| {
            assert_eq!(view.renderer.reading_document(), Some(0));
            view.prepare_ruler();
            assert_eq!(view.ruler.glyph.unwrap().1, view.renderer.visible_reading_lines().last().unwrap().glyphs.start);
        });
    }

    #[gpui::test]
    fn reading_ruler_finishes_partial_groups_before_turning_pages(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            for count in 1..=5 {
                view.set_reading_ruler(ReadingRulerStyle::Band, count, cx);
                view.reset_ruler(true);
                let page = view.current_cfi();
                let lines = view.renderer.visible_reading_lines();
                let mut consumed = 0;
                while consumed < lines.len() {
                    let end = (consumed + count).min(lines.len());
                    let expected = lines[consumed..end].iter().fold(lines[consumed].bounds, |bounds, line| bounds.union(line.bounds));
                    assert_eq!(view.prepare_ruler()[0].0, expected, "forward group must contain only unread lines");
                    assert_eq!(view.current_cfi(), page, "a partial final group must remain on the page");
                    consumed = end;
                    view.step_reading_ruler(true, cx);
                }
                assert_ne!(view.current_cfi(), page, "turn only after the final group has been displayed");
                view.step_reading_ruler(false, cx);
                assert_eq!(view.current_cfi(), page);
                let mut remaining = lines.len();
                while remaining > 0 {
                    let start = remaining.saturating_sub(count);
                    let expected = lines[start..remaining].iter().fold(lines[start].bounds, |bounds, line| bounds.union(line.bounds));
                    assert_eq!(view.prepare_ruler()[0].0, expected, "backward group must contain only unread lines");
                    assert_eq!(view.current_cfi(), page, "a partial first group must remain on the page");
                    remaining = start;
                    view.step_reading_ruler(false, cx);
                }
                assert_eq!(view.current_cfi(), page, "book-start boundary must reject a backward turn");
            }
        });
    }

    fn test_line(glyphs: Range<u32>, bounds: Rect, column: i32, cell: Option<usize>) -> ReadingLine {
        ReadingLine { glyphs, bounds, column_bounds: bounds, column, cell }
    }

    #[test]
    fn overlapping_lines_are_counted_independently() {
        let lines = vec![test_line(0..10, Rect::new(0.0, 0.0, 100.0, 20.0), 0, None), test_line(10..20, Rect::new(0.0, 10.0, 100.0, 30.0), 0, None), test_line(20..30, Rect::new(0.0, 15.0, 100.0, 25.0), 0, None)];
        assert_eq!(line_group(&lines, 0, 1, false), 0..1);
        assert_eq!(line_group(&lines, 1, 1, false), 1..2);
        assert_eq!(line_group(&lines, 2, 1, true), 2..3);
        assert_eq!(line_group(&lines, 0, 2, false), 0..2);
        assert_eq!(focus_bounds(&lines, 1, 1, false), lines[1].bounds);
    }

    #[test]
    fn groups_stop_at_cell_identity_even_when_bounds_match() {
        let lines = vec![
            test_line(0..10, Rect::new(0.0, 0.0, 100.0, 20.0), 0, Some(1)),
            test_line(10..20, Rect::new(0.0, 20.0, 100.0, 40.0), 0, Some(1)),
            test_line(20..30, Rect::new(0.0, 40.0, 100.0, 60.0), 0, Some(2)),
            test_line(30..40, Rect::new(0.0, 60.0, 100.0, 80.0), 0, Some(2)),
        ];
        assert_eq!(line_group(&lines, 0, 3, false), 0..2);
        assert_eq!(line_group(&lines, 3, 3, true), 2..4);
        assert_eq!(line_group(&lines, 1, 3, false), 1..2);
    }

    #[test]
    fn reading_ruler_groups_do_not_overlap_when_direction_changes() {
        let lines = (0..8).map(|i| test_line(i..i + 1, Rect::new(0.0, f64::from(i) * 20.0, 100.0, f64::from(i + 1) * 20.0), 0, None)).collect::<Vec<_>>();
        assert_eq!(line_group(&lines, 0, 3, false), 0..3);
        assert_eq!(line_group(&lines, 3, 3, false), 3..6);
        assert_eq!(line_group(&lines, 6, 3, false), 6..8);
        // Reverse from the short final group, then return to it.
        assert_eq!(line_group(&lines, 5, 3, true), 3..6);
        assert_eq!(line_group(&lines, 6, 3, false), 6..8);
        // Entering backwards starts from the bottom and leaves a short group at the top.
        assert_eq!(line_group(&lines, 7, 3, true), 5..8);
        assert_eq!(line_group(&lines, 4, 3, true), 2..5);
        assert_eq!(line_group(&lines, 1, 3, true), 0..2);
    }

    #[test]
    fn focus_window_stays_in_its_column() {
        let lines = vec![test_line(0..5, Rect::new(0.0, 0.0, 100.0, 20.0), 0, None), test_line(5..10, Rect::new(0.0, 20.0, 100.0, 40.0), 0, None), test_line(10..15, Rect::new(120.0, 0.0, 220.0, 20.0), 1, None)];
        assert_eq!(focus_bounds(&lines, 0, 3, false), Rect::new(0.0, 0.0, 100.0, 40.0));
        assert_eq!(focus_bounds(&lines, 1, 3, true), Rect::new(0.0, 0.0, 100.0, 40.0));
        assert_eq!(focus_bounds(&lines, 2, 3, false), lines[2].bounds);
        assert_eq!(focus_bounds(&lines, 2, 3, true), lines[2].bounds);
    }

    #[test]
    fn focus_mask_leaves_only_the_selected_window_clear() {
        let band = Rect::new(20.0, 100.0, 120.0, 140.0);
        for viewport in [Rect::new(0.0, 0.0, 240.0, 400.0), Rect::new(-40.0, -24.0, 280.0, 424.0)] {
            let mask = mask_rects(band, viewport);
            assert_eq!(mask.len(), 4);
            assert_eq!(mask.iter().map(|rect| rect.area()).sum::<f64>() + band.area(), viewport.area());
            assert!(mask.iter().all(|rect| rect.intersect(band).area() == 0.0));
            assert!(mask.iter().all(|rect| rect.intersect(viewport) == *rect));
        }
    }
    #[gpui::test]
    fn reading_ruler_resize_keeps_selected_passage(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        let anchor = document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            view.prepare_ruler();
            for _ in 0..8 {
                view.step_reading_ruler(true, cx);
            }
            view.prepare_ruler();
            view.ruler.anchor.clone().unwrap()
        });
        visual.simulate_resize(size(px(600.0), px(140.0)));
        visual.run_until_parked();
        document.update(visual, |view, _| {
            view.prepare_ruler();
            let (_, glyph) = parse_cfi(view.renderer.render_view(), &anchor).unwrap();
            let lines = view.renderer.visible_reading_lines();
            let index = view.ruler.index(&view.renderer, &lines).unwrap();
            assert!(lines[index].glyphs.contains(&glyph), "shrinking the reader silently returned the ruler to its first visible line");
        });
    }

    #[gpui::test]
    fn hidden_text_does_not_consume_a_ruler_step(cx: &mut TestAppContext) {
        let markup = "<p>Visible first</p><p style='visibility:hidden'>Hidden text</p><p>Visible last</p>";
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(StructuredBook(markup)), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            let lines = view.renderer.visible_reading_lines();
            let last_visible = lines.last().unwrap().bounds;
            view.set_reading_ruler(ReadingRulerStyle::Band, 1, cx);
            view.prepare_ruler();
            view.step_reading_ruler(true, cx);
            assert_eq!(view.prepare_ruler()[0].0, last_visible, "the first step must reach the next painted text rather than a hidden line");
        });
    }

    #[gpui::test]
    fn visible_text_inside_a_hidden_line_still_counts(cx: &mut TestAppContext) {
        let markup = "<p>Before</p><p style='visibility:hidden'>Hidden <span style='visibility:visible'>Visible</span> hidden tail</p><p>After</p>";
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(StructuredBook(markup)), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        document.update(visual, |view, _| {
            let lines = view.renderer.visible_reading_lines();
            assert_eq!(lines.len(), 3);
            let text = view.renderer.render_view().text();
            let glyph = text.glyph_at(lines[1].glyphs.start as usize).unwrap();
            assert_eq!(text.glyph_metric(glyph).unwrap().ch(), 'V', "anchor the line on its visible text, skipping the hidden prefix");
        });
    }

    #[gpui::test]
    fn explicit_page_navigation_during_resize_replaces_the_ruler_anchor(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        let anchor = document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Focus, 1, cx);
            view.prepare_ruler();
            view.step_reading_ruler(true, cx);
            view.prepare_ruler();
            let anchor = view.ruler.anchor.clone().unwrap();
            assert!(view.next_page(cx));
            anchor
        });
        visual.simulate_resize(size(px(600.0), px(140.0)));
        visual.run_until_parked();
        document.update(visual, |view, _| {
            view.prepare_ruler();
            assert_ne!(view.ruler.anchor.as_deref(), Some(anchor.as_str()), "resize must not undo an explicit page turn");
            let lines = view.renderer.visible_reading_lines();
            assert_eq!(view.ruler.glyph.unwrap().1, lines[0].glyphs.start);
        });
    }

    #[gpui::test]
    fn reducing_column_count_keeps_the_selected_passage(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { column_width: 400.0, max_column_count: Some(2), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(900.0), px(400.0)));
        visual.run_until_parked();
        let anchor = document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Focus, 1, cx);
            let lines = view.renderer.visible_reading_lines();
            let target = lines.iter().find(|line| line.column == 1).expect("two visible columns");
            view.anchor_ruler(&target.glyphs);
            view.prepare_ruler();
            let anchor = view.ruler.anchor.clone().unwrap();
            view.apply(skrift_column_paginator::RendererCommand::SetMaxColumnCount(Some(1)), cx);
            anchor
        });
        visual.run_until_parked();
        document.update(visual, |view, _| {
            view.prepare_ruler();
            let (_, glyph) = parse_cfi(view.renderer.render_view(), &anchor).unwrap();
            let lines = view.renderer.visible_reading_lines();
            let selected = view.ruler.index(&view.renderer, &lines).unwrap();
            assert!(lines[selected].glyphs.contains(&glyph));
            assert!(lines.iter().all(|line| line.column == 0));
        });
    }

    #[gpui::test]
    fn reading_ruler_keeps_narration_page_after_resize(cx: &mut TestAppContext) {
        let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
        let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(RulerBook), vec!["book.html".into()], 0, None, config, window, cx));
        visual.simulate_resize(size(px(600.0), px(400.0)));
        visual.run_until_parked();
        let target_start = document.update(visual, |view, cx| {
            view.set_reading_ruler(ReadingRulerStyle::Focus, 1, cx);
            view.prepare_ruler();
            view.step_reading_ruler(true, cx);
            view.prepare_ruler();
            let (_, _, _, items) = view.renderer.reading_items().unwrap();
            let target = items[40].clone();
            let start = target.glyph_ranges[0].start;
            let revision = view.renderer.navigation_revision();
            view.follow_reading_target(target, cx);
            assert_eq!(view.renderer.navigation_revision(), revision, "following narration must not trigger a manual-navigation restart");
            cx.notify();
            start
        });
        visual.run_until_parked();
        document.update(visual, |view, _| {
            assert!(view.renderer.visible_reading_lines().iter().any(|line| line.glyphs.contains(&target_start)), "narration must first navigate to its target");
        });
        visual.simulate_resize(size(px(560.0), px(300.0)));
        visual.run_until_parked();
        document.update(visual, |view, _| {
            assert!(view.renderer.visible_reading_lines().iter().any(|line| line.glyphs.contains(&target_start)), "resize must not undo narration navigation");
        });
    }
}
