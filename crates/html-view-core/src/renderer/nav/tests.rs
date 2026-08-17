//! Renderer navigation, pagination, and note regressions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kurbo::{Point, Size};

use crate::text_backend::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper};
use crate::{FileSystemProvider, RecordingPainter, RendererCore, RendererEvent, RendererHost, RendererInitialConfig, RendererOperation, load};

#[derive(Default)]
struct TestShaper {
    glyphs: HashMap<(char, u32), GlyphId>,
    next_id: GlyphId,
    reject_glyphs: bool,
}

impl GlyphShaper for TestShaper {
    fn reset(&mut self) {
        self.glyphs.clear();
        self.next_id = 0;
    }

    fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _font_weight: u16, _font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, crate::layout::ShapeError> {
        if self.reject_glyphs {
            return Err(crate::layout::ShapeError::unregistered_glyph_id(u32::MAX, glyph_metrics.len()));
        }
        let key = (ch, font_size.to_bits());
        if let Some(&glyph) = self.glyphs.get(&key)
            && glyph_metrics.contains(glyph)
        {
            return Ok(glyph);
        }
        let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(crate::layout::ShapeError::rejected_metric)?;
        let glyph = glyph_metrics.register(metric)?;
        assert_eq!(glyph, self.next_id, "glyph shaper and document registry diverged");
        self.next_id += 1;
        self.glyphs.insert(key, glyph);
        Ok(glyph)
    }
}

fn activate_glyph(core: &mut RendererCore, shaper: &mut TestShaper, glyph: u32) -> bool {
    let doc = core.nav.location().document();
    core.handle_document_click(shaper, crate::DocumentGlyph { doc, glyph }, None)
}

#[derive(Default)]
struct TestHost {
    events: RefCell<Vec<RendererEvent>>,
    repaint_requests: Cell<usize>,
    clipboard_text: RefCell<Option<String>>,
    note_width: Cell<Option<f64>>,
}

impl RendererHost for TestHost {
    fn note_popup_width(&self) -> Option<f64> {
        self.note_width.get()
    }
    fn request_repaint(&self) {
        self.repaint_requests.set(self.repaint_requests.get() + 1);
    }
    fn request_style(&self) {}
    fn schedule(&self, _delay: Duration, callback: Box<dyn FnOnce() + Send>) {
        callback();
    }
    fn schedule_repaint(&self, _delay: Duration) {}
    fn resource_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        None
    }
    fn set_clipboard(&self, text: &str) -> Result<(), String> {
        *self.clipboard_text.borrow_mut() = Some(text.to_owned());
        Ok(())
    }
    fn set_clipboard_image(&self, _width: usize, _height: usize, _rgba: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn emit(&self, event: RendererEvent) {
        self.events.borrow_mut().push(event);
    }
}

#[derive(Default)]
struct CountingProvider {
    documents: HashMap<String, String>,
    read_count: Arc<AtomicUsize>,
}

impl CountingProvider {
    fn new(documents: Vec<(&str, &str)>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let read_count = Arc::new(AtomicUsize::new(0));
        let mut mapped = HashMap::new();
        for (uri, body) in documents {
            mapped.insert(uri.to_owned(), body.to_owned());
        }
        (Arc::new(Self { documents: mapped, read_count: read_count.clone() }), read_count)
    }
}

fn resolve_test_path(base: &str, href: &str) -> String {
    if href.starts_with('/') || href.contains(':') {
        return href.to_owned();
    }
    let joined = match base.rsplit_once('/') {
        Some((directory, _)) => format!("{directory}/{href}"),
        None => href.to_owned(),
    };
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

impl crate::ResourceProvider for CountingProvider {
    fn read_bytes(&self, uri: &str) -> std::io::Result<Vec<u8>> {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        self.documents.get(uri).cloned().map(|value| value.into_bytes()).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, uri.to_owned()))
    }

    fn exists(&self, uri: &str) -> bool {
        self.documents.contains_key(uri)
    }

    fn resolve(&self, base: &str, href: &str) -> String {
        resolve_test_path(base, href)
    }

    fn list_html_candidates(&self, _root: &str) -> std::io::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn pagination_core(source: &str, viewport: Size, max_columns: u8) -> (RendererCore, TestShaper) {
    pagination_core_with_policy(source, viewport, max_columns, html::pipeline::TextCompositionPolicy::WebCompatible)
}

fn pagination_core_with_policy(source: &str, viewport: Size, max_columns: u8, text_composition_policy: html::pipeline::TextCompositionPolicy) -> (RendererCore, TestShaper) {
    pagination_core_with_policy_at_uri("document.html", source, viewport, max_columns, text_composition_policy)
}

fn pagination_core_with_policy_at_uri(uri: &str, source: &str, viewport: Size, max_columns: u8, text_composition_policy: html::pipeline::TextCompositionPolicy) -> (RendererCore, TestShaper) {
    let (provider, _) = CountingProvider::new(vec![(uri, source)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config =
        RendererInitialConfig { font_size: 16.0, column_width: 200.0, max_column_count: Some(max_columns), image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible, text_composition_policy, ..RendererInitialConfig::default() };
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec![uri.to_owned()], 0, Some("0:0"), config);
    core.configure_layout_for_viewport(&mut shaper, viewport);
    core.paint_forward(&mut RecordingPainter::default());
    (core, shaper)
}

#[test]
fn default_reader_column_count_is_derived_only_from_available_width() {
    let (provider, _) = CountingProvider::new(vec![("document.html", "<html><body>Wide reader</body></html>")]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig { column_width: 200.0, max_column_count: None, ..RendererInitialConfig::default() };
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);

    core.configure_layout_for_viewport(&mut shaper, Size::new(1_100.0, 600.0));

    assert_eq!(core.page.view.layout.col_count, 5.0);
}

fn line_index_for_character(core: &RendererCore, wanted: char) -> usize {
    let text = core.page.view.document.render_view().text();
    let glyph = glyph_index_for_character(core, wanted);
    text.line_index_for_glyph(glyph).expect("fixture character must own a line")
}

fn glyph_index_for_character(core: &RendererCore, wanted: char) -> u32 {
    let text = core.page.view.document.render_view().text();
    (0..text.glyph_count() as u32).find(|index| text.glyph_at(*index as usize).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == wanted)).expect("fixture character must exist")
}

fn laid_out_text(document: &crate::LaidOutDocument) -> String {
    let text = document.render_view().text();
    (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect()
}

fn visible_positions(core: &RendererCore) -> Vec<(usize, i32, f64)> {
    core.page.view.frame.page().line_positions().iter().map(|(line, screen)| (line, screen.col_index, screen.point.y)).collect()
}

#[test]
fn optional_vertical_rhythm_only_stretches_existing_block_gaps() {
    let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0 0 10px;line-height:20px'>B</p><p style='margin:0;line-height:20px'>C</p></body></html>";
    let (mut natural, _) = pagination_core_with_policy(html, Size::new(200.0, 75.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let natural_positions = visible_positions(&natural);
    let a = line_index_for_character(&natural, 'A');
    let b = line_index_for_character(&natural, 'B');

    natural.vertical_rhythm = true;
    natural.paint_forward(&mut RecordingPainter::default());
    let refined_positions = visible_positions(&natural);

    assert_eq!(natural_positions.iter().map(|(line, column, _)| (*line, *column)).collect::<Vec<_>>(), refined_positions.iter().map(|(line, column, _)| (*line, *column)).collect::<Vec<_>>());
    assert_eq!(natural.page.view.frame.page().line_positions().get(a).map(|screen| screen.point.y), Some(0.0), "the first line remains anchored");
    assert_eq!(natural.page.view.frame.page().line_positions().get(b).map(|screen| screen.point.y), Some(32.0), "the sole safe block gap receives the 2px cap");
    assert_eq!(natural.page.view.document.render_view().text().line(b).map(|line| line.point().y), Some(30.0), "continuous CSS layout is untouched");
}

#[test]
fn vertical_rhythm_leaves_the_terminal_document_column_ragged() {
    let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (mut core, _) = pagination_core_with_policy(html, Size::new(200.0, 75.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let before = visible_positions(&core);

    core.vertical_rhythm = true;
    core.paint_forward(&mut RecordingPainter::default());

    assert_eq!(visible_positions(&core), before);
}

#[test]
fn web_compatible_pagination_ignores_vertical_rhythm() {
    let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0 0 10px;line-height:20px'>B</p><p style='margin:0;line-height:20px'>C</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(200.0, 75.0), 1);
    let before = visible_positions(&core);

    core.vertical_rhythm = true;
    core.paint_forward(&mut RecordingPainter::default());

    assert_eq!(visible_positions(&core), before, "WPT/browser-compatible geometry must remain greedy and unadjusted");
}

#[test]
fn book_paginator_keeps_a_compact_list_item_intact() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px'>L<br>M</li></ul></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first = line_index_for_character(&core, 'L');
    let second = line_index_for_character(&core, 'M');

    assert_eq!(core.page.view.frame.page().line_positions().get(first).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
}

#[test]
fn book_paginator_keeps_a_term_with_the_first_two_definition_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><dl style='margin:0'><dt style='line-height:20px'>T</dt><dd style='margin:0;line-height:20px'>D<br>E</dd></dl></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let term = line_index_for_character(&core, 'T');
    let definition = line_index_for_character(&core, 'D');
    let definition_second = line_index_for_character(&core, 'E');

    assert_eq!(core.page.view.frame.page().line_positions().get(term).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(definition).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(definition_second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 40.0)));
}

#[test]
fn book_paginator_keeps_compact_preformatted_code_and_splits_oversized_code() {
    let compact = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD</code></pre></body></html>";
    let (compact_core, _) = pagination_core_with_policy(compact, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let compact_first = line_index_for_character(&compact_core, 'C');
    let compact_second = line_index_for_character(&compact_core, 'D');
    let compact_first_position = compact_core.page.view.frame.page().line_positions().get(compact_first).expect("first compact code line");
    let compact_second_position = compact_core.page.view.frame.page().line_positions().get(compact_second).expect("second compact code line");
    assert_eq!((compact_first_position.col_index, compact_second_position.col_index), (1, 1));
    assert!(compact_second_position.point.y > compact_first_position.point.y);

    let oversized = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD\nE\nF</code></pre></body></html>";
    let (oversized_core, _) = pagination_core_with_policy(oversized, Size::new(640.0, 50.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
    let oversized_first = line_index_for_character(&oversized_core, 'C');
    let oversized_last = line_index_for_character(&oversized_core, 'F');
    let first_column = oversized_core.page.view.frame.page().line_positions().get(oversized_first).map(|screen| screen.col_index).expect("first oversized code line");
    let last_column = oversized_core.page.view.frame.page().line_positions().get(oversized_last).map(|screen| screen.col_index).expect("last oversized code line");
    assert!(last_column > first_column, "oversized code must fall back to splitting across columns");
}

#[test]
fn web_paginator_does_not_apply_semantic_keeps() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px'>L<br>M</li></ul></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
    let first = line_index_for_character(&core, 'L');
    let second = line_index_for_character(&core, 'M');

    assert_eq!(core.page.view.frame.page().line_positions().get(first).map(|screen| screen.col_index), Some(0));
    assert_eq!(core.page.view.frame.page().line_positions().get(second).map(|screen| screen.col_index), Some(1));
}

#[test]
fn paginator_keeps_a_line_that_fits_exactly_on_the_column_bottom() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().start_line(), Some(a));
    assert_eq!(core.page.view.frame.page().end_line(), Some(b));
    assert_eq!(core.page.view.frame.page().line_positions().get(a).map(|screen| screen.col_index), Some(0));
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| screen.col_index), Some(0));
    assert!(core.page.view.frame.page().line_positions().get(c).is_none(), "the first line beyond the exact bottom edge belongs to the next page");
}

#[test]
fn paginator_moves_an_overflowing_line_whole_to_the_next_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 39.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let first = core.page.view.frame.page().line_positions().get(a).expect("first line visible");
    let second = core.page.view.frame.page().line_positions().get(b).expect("overflow line visible in the following column");

    assert_eq!(first.col_index, 0);
    assert_eq!(second.col_index, 1);
    assert_eq!(second.point.y, 0.0, "an overflowing line must restart at the column top");
}

#[test]
fn paginator_counts_document_vertical_gaps_when_fitting_a_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='height:10px'></div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let first = core.page.view.frame.page().line_positions().get(a).expect("first line visible");
    let second = core.page.view.frame.page().line_positions().get(b).expect("second line visible");

    assert!(core.page.view.document.render_view().text().line(b).expect("second source line").point().y > core.page.view.document.render_view().text().line(a).expect("first source line").point().y + 20.0);
    assert_eq!((first.col_index, second.col_index), (0, 1));
    assert_eq!(second.point.y, 0.0);
}

#[test]
fn paginator_uses_tall_inline_image_height_for_column_fitting() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A<img src='missing.png' style='width:10px;height:35px'></div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let images = core.page.view.document.render_view().fragments().images_for_line(a);

    assert!(images.iter().any(|image| image.size().height == 35.0), "fixture must retain the 35px replaced-content height");
    assert_eq!(core.page.view.frame.page().line_positions().get(a).map(|screen| screen.col_index), Some(0));
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| screen.col_index), Some(1));
}

#[test]
fn paginator_places_an_oversized_line_once_and_advances_to_the_next_page() {
    let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().start_line(), Some(a));
    assert_eq!(core.page.view.frame.page().end_line(), Some(a));
    assert_eq!(core.page.view.frame.page().line_positions().get(a).map(|screen| screen.point.y), Some(0.0));
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());

    assert_eq!(core.page.view.frame.page().start_line(), Some(b));
    assert!(core.page.view.frame.page().line_positions().get(b).is_some(), "pagination must make progress beyond the oversized line");
}

#[test]
fn paginator_roundtrips_backward_across_an_oversized_line() {
    let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_moves_an_oversized_image_line_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div><img src='missing.png' style='width:10px;height:60px;vertical-align:top'>B</div><div style='line-height:20px'>C</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().line_positions().get(a).map(|screen| screen.col_index), Some(0));
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    assert!(core.page.view.frame.page().line_positions().get(c).is_none(), "the oversized image consumes its destination column");
}

#[test]
fn paginator_honors_forced_modern_and_legacy_css_breaks() {
    for property in ["break-before:page", "page-break-before:always"] {
        let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>");
        let (core, _) = pagination_core(&html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert!(core.page.view.frame.page().line_positions().get(a).is_some());
        assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "{property} must start B in the next fragmentainer");
        assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 40.0);
    }
}

#[test]
fn book_paginator_distinguishes_column_breaks_from_page_breaks() {
    let column_html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;break-before:column'>B</div></body></html>";
    let (column_core, _) = pagination_core_with_policy(column_html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
    let column_b = line_index_for_character(&column_core, 'B');
    assert_eq!(column_core.page.view.frame.page().line_positions().get(column_b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));

    for property in ["break-before:page", "page-break-before:always", "break-before:left", "break-before:right"] {
        let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>");
        let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');

        assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "{property} must skip the remaining reader columns");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "{property} must begin at the next reader page origin");
    }
}

#[test]
fn book_paginator_anchors_forced_breaks_on_empty_boxes_to_following_content() {
    for property in ["break-before:column", "break-after:column"] {
        let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='{property}'></div><div style='line-height:20px'>B</div></body></html>");
        let (core, _) = pagination_core_with_policy(&html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert_eq!(core.page.view.frame.page().line_positions().get(a).map(|screen| screen.col_index), Some(0));
        assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{property} on an empty box must anchor to B");
    }
}

#[test]
fn forced_break_inside_an_avoided_container_takes_precedence() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "avoid must not move content preceding an internal forced break");
    assert_eq!(core.page.view.frame.page().line_positions().get(c).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
}

#[test]
fn forced_column_break_does_not_leave_a_margin_only_empty_column() {
    let html = "<html><body style='margin:0'><div style='height:90px;margin-bottom:20px;line-height:20px'>A</div><div style='break-before:column;line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(700.0, 100.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
    let b = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 110.0, "continuous layout must retain the ordinary block margin");
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
}

#[test]
fn book_paginator_applies_break_after_without_fragmenting_continuous_layout() {
    for (property, first_page_has_b) in [("break-after:column", true), ("break-after:page", false), ("page-break-after:always", false)] {
        let html = format!("<html><body style='margin:0'><div style='height:30px;margin-bottom:10px;line-height:20px;{property}'>A</div><div style='line-height:20px'>B</div></body></html>");
        let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(700.0, 60.0), 3, html::pipeline::TextCompositionPolicy::BookOptimized);
        let b = line_index_for_character(&core, 'B');

        assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 40.0, "forced pagination must not rewrite continuous document geometry");
        if first_page_has_b {
            assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
        } else {
            assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "{property} must skip the remaining columns");
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
        }
    }
}

#[test]
fn paginator_forced_break_wins_over_an_adjoining_avoid() {
    let html = "<html><body style='margin:0'><div style='line-height:20px;break-after:page'>A</div><div style='line-height:20px;break-before:avoid'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(a).expect("A line").point().y, 0.0);
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 40.0);
}

#[test]
fn paginator_keeps_a_break_inside_avoid_block_together() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 40.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 60.0);
}

#[test]
fn book_paginator_applies_break_inside_avoid_without_changing_continuous_geometry() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 40.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 40.0);
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(c).map(|screen| screen.point.y), Some(20.0));
}

#[test]
fn paginator_honors_break_after_avoid_as_keep_with_next() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:avoid'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(heading).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 40.0);
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("following line").point().y, 60.0);
}

#[test]
fn book_paginator_automatically_keeps_a_heading_with_two_following_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D<br>E</p></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let a = line_index_for_character(&core, 'A');
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(heading).is_none(), "the heading must move instead of becoming the last line of the page");
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("first paragraph line").point().y, 40.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("second paragraph line").point().y, 60.0);
}

#[test]
fn web_paginator_leaves_automatic_heading_keep_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let heading = line_index_for_character(&core, 'H');

    assert!(core.page.view.frame.page().line_positions().get(heading).is_some(), "web-compatible pagination must retain browser-style heading placement");
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
}

#[test]
fn book_paginator_moves_a_large_multiline_heading_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H<br>I</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 80.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let heading = line_index_for_character(&core, 'H');
    let second_heading_line = line_index_for_character(&core, 'I');

    assert!(core.page.view.frame.page().line_positions().get(heading).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(second_heading_line).expect("second heading line").point().y, 40.0);
}

#[test]
fn book_paginator_does_not_move_an_oversized_heading_group() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;widows:1;orphans:1'>H<br>I<br>J</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let heading = line_index_for_character(&core, 'H');

    assert!(core.page.view.frame.page().line_positions().get(heading).is_some(), "a group taller than a full column must use best-effort pagination");
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
}

#[test]
fn authored_forced_break_after_heading_overrides_automatic_keep() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:page'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().line_positions().get(heading).is_some(), "the authored forced break must leave the heading before the break");
    assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "the following paragraph must begin on the next reader page");
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("following line").point().y, 40.0, "forced pagination must not insert a gap into continuous layout");
}

#[test]
fn automatic_heading_keep_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D</p></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn book_paginator_keeps_compact_semantic_sidebars_in_one_column() {
    for (uri, semantic, html) in [
        ("document.html", "aside", "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>"),
        (
            "document.html",
            "ARIA complementary",
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='complementary' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></section></body></html>",
        ),
        (
            "document.xhtml",
            "EPUB sidebar",
            "<html xmlns='http://www.w3.org/1999/xhtml' xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='sidebar' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></section></body></html>",
        ),
    ] {
        let (core, _) = pagination_core_with_policy_at_uri(uri, html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let sidebar = line_index_for_character(&core, 'S');
        let sidebar_end = line_index_for_character(&core, 'T');

        assert_eq!(core.page.view.frame.page().line_positions().get(sidebar).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{semantic} must move as one compact block");
        assert_eq!(core.page.view.frame.page().line_positions().get(sidebar_end).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    }
}

#[test]
fn book_paginator_keeps_bounded_compact_semantic_structures_in_one_column() {
    for (uri, semantic, attribute) in [
        ("document.xhtml", "EPUB dedication", "epub:type='dedication'"),
        ("document.xhtml", "EPUB abstract", "epub:type='abstract'"),
        ("document.xhtml", "EPUB notice", "epub:type='notice'"),
        ("document.xhtml", "EPUB pullquote", "epub:type='pullquote'"),
        ("document.xhtml", "prefixed EPUB theorem", "epub:type='math:theorem'"),
        ("document.xhtml", "EPUB proof", "epub:type='proof'"),
        ("document.xhtml", "EPUB example", "epub:type='example'"),
        ("document.xhtml", "EPUB exercise", "epub:type='exercise'"),
        ("document.html", "ARIA abstract", "role='doc-abstract'"),
        ("document.html", "ARIA example", "role='doc-example'"),
    ] {
        let html = format!(
            "<html xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section {attribute} style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>"
        );
        let (core, _) = pagination_core_with_policy_at_uri(uri, &html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let start = line_index_for_character(&core, 'B');
        let end = line_index_for_character(&core, 'C');

        assert_eq!(core.page.view.frame.page().line_positions().get(start).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)), "{semantic} must move as one compact structure");
        assert_eq!(core.page.view.frame.page().line_positions().get(end).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)), "{semantic} must remain intact");
    }
}

#[test]
fn book_paginator_splits_an_oversized_semantic_structure_normally() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='theorem' style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let start = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(start).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "an oversized semantic structure must use available space instead of moving as one unit");
}

#[test]
fn authored_break_inside_a_compact_semantic_structure_wins() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='abstract' style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let start = line_index_for_character(&core, 'B');
    let after_break = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().line_positions().get(start).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(after_break).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
}

#[test]
fn book_paginator_keeps_a_semantic_statement_label_with_two_content_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>X</div><section epub:type='theorem' style='margin:0'><p epub:type='label' style='margin:0;line-height:20px'>L</p><p style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</p></section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let label = line_index_for_character(&core, 'L');
    let first = line_index_for_character(&core, 'B');
    let second = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().line_positions().get(label).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(first).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(second).map(|screen| (screen.col_index, screen.point.y)), Some((1, 40.0)));
}

#[test]
fn web_paginator_does_not_add_semantic_sidebar_placement() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside epub:type='sidebar' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
    let sidebar = line_index_for_character(&core, 'S');

    assert_eq!(core.page.view.frame.page().line_positions().get(sidebar).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn book_paginator_starts_major_epub_sections_on_a_fresh_reader_page() {
    for semantic in [
        "chapter",
        "part",
        "appendix",
        "prologue",
        "epilogue",
        "foreword",
        "preface",
        "introduction",
        "conclusion",
        "acknowledgments",
        "afterword",
        "bibliography",
        "glossary",
        "index",
        "colophon",
        "titlepage",
        "halftitlepage",
        "copyright-page",
        "frontmatter",
        "bodymatter",
        "backmatter",
    ] {
        let html = format!(
            "<html xmlns='http://www.w3.org/1999/xhtml' xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='landmarks {semantic}' style='margin:0;line-height:20px'>B</section></body></html>"
        );
        let (mut core, mut shaper) = pagination_core_with_policy_at_uri("document.xhtml", &html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let section = line_index_for_character(&core, 'B');

        assert!(core.page.view.frame.page().line_positions().get(section).is_none(), "EPUB {semantic} must skip all remaining columns on the current reader page");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "EPUB {semantic} must start at the first column");
    }
}

#[test]
fn book_paginator_starts_major_dpub_sections_on_a_fresh_reader_page() {
    for semantic in [
        "doc-chapter",
        "doc-part",
        "doc-appendix",
        "doc-prologue",
        "doc-epilogue",
        "doc-foreword",
        "doc-preface",
        "doc-introduction",
        "doc-conclusion",
        "doc-acknowledgments",
        "doc-afterword",
        "doc-bibliography",
        "doc-glossary",
        "doc-index",
        "doc-colophon",
    ] {
        let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='region {semantic}' style='margin:0;line-height:20px'>B</section></body></html>");
        let (mut core, mut shaper) = pagination_core_with_policy(&html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
        let section = line_index_for_character(&core, 'B');

        assert!(core.page.view.frame.page().line_positions().get(section).is_none(), "ARIA {semantic} must skip all remaining columns on the current reader page");
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)), "ARIA {semantic} must start at the first column");
    }
}

#[test]
fn generic_section_remains_in_normal_book_flow() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let section = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn authored_avoid_suppresses_a_semantic_page_start() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px;break-before:avoid'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let section = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn semantic_page_start_at_document_origin_does_not_create_a_blank_page() {
    let html = "<html><body style='margin:0'><section epub:type='titlepage' style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let section = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
}

#[test]
fn web_paginator_does_not_add_expanded_semantic_section_breaks() {
    for (semantic, html) in [
        ("EPUB appendix", "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px'>B</section></body></html>"),
        ("ARIA bibliography", "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='doc-bibliography' style='margin:0;line-height:20px'>B</section></body></html>"),
    ] {
        let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
        let section = line_index_for_character(&core, 'B');

        assert_eq!(core.page.view.frame.page().line_positions().get(section).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)), "{semantic} must remain ordinary flow in web mode");
    }
}

#[test]
fn authored_column_break_overrides_a_semantic_chapter_page_break() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px;break-before:column'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(420.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let chapter = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(chapter).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
}

#[test]
fn web_paginator_does_not_add_semantic_chapter_breaks() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
    let chapter = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().line_positions().get(chapter).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn book_paginator_moves_a_compact_figure_and_caption_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(image_label).is_none(), "the compact figure must not be split across columns");
    assert_eq!(core.page.view.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 40.0);
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(image_label).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(caption).map(|screen| screen.point.y), Some(20.0));
}

#[test]
fn book_paginator_keeps_an_image_only_figure_whole() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 30.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let image_line = core.page.view.document.render_view().fragments().images().iter().next().expect("fixture image fragment").line_idx();

    assert!(core.page.view.frame.page().line_positions().get(image_line).is_none(), "the image-only figure must move intact");
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(image_line).map(|screen| screen.point.y), Some(0.0));
}

#[test]
fn book_paginator_keeps_a_replaced_image_with_its_multiline_caption() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'><figcaption style='line-height:20px'>C<br>D</figcaption></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let image_line = core.page.view.document.render_view().fragments().images().iter().next().expect("fixture image fragment").line_idx();
    let caption_first = line_index_for_character(&core, 'C');
    let caption_second = line_index_for_character(&core, 'D');

    assert!(core.page.view.frame.page().line_positions().get(image_line).is_none(), "the image must move instead of leaving its caption behind");
    assert!(core.page.view.frame.page().line_positions().get(caption_first).is_none());
    assert!(core.page.view.frame.page().line_positions().get(caption_second).is_none());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(image_line).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(caption_first).map(|screen| screen.point.y), Some(20.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(caption_second).map(|screen| screen.point.y), Some(40.0));
}

#[test]
fn web_paginator_leaves_automatic_figure_placement_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 50.0), 1);
    let image_label = line_index_for_character(&core, 'I');

    assert!(core.page.view.frame.page().line_positions().get(image_label).is_some(), "web-compatible pagination must preserve normal flow placement");
    assert_eq!(core.page.view.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
}

#[test]
fn book_paginator_keeps_a_bottom_caption_with_oversized_figure_content() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.document.render_view().text().line(image_label).expect("figure content line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 40.0);
    assert!(core.page.view.frame.page().line_positions().get(image_label).is_none());
    assert!(core.page.view.frame.page().line_positions().get(caption).is_none());
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(image_label).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(caption).map(|screen| screen.point.y), Some(20.0));
}

#[test]
fn book_paginator_keeps_a_top_caption_with_oversized_figure_content() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><figcaption style='line-height:20px'>C</figcaption><div style='line-height:20px'>I</div><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let caption = line_index_for_character(&core, 'C');
    let image_label = line_index_for_character(&core, 'I');

    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(image_label).expect("figure content line").point().y, 40.0);
    assert!(core.page.view.frame.page().line_positions().get(caption).is_none());
    assert!(core.page.view.frame.page().line_positions().get(image_label).is_none());
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(caption).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(image_label).map(|screen| screen.point.y), Some(20.0));
}

#[test]
fn authored_forced_break_between_figure_content_and_caption_wins() {
    let html = "<html><body style='margin:0'><figure style='margin:0'><div style='line-height:20px;break-after:page'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.document.render_view().text().line(image_label).expect("figure content line").point().y, 0.0);
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "forced pagination must not insert a gap into continuous layout");
    assert!(core.page.view.frame.page().line_positions().get(caption).is_none(), "the caption must begin on the next reader page");
}

#[test]
fn automatic_figure_placement_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure><div style='line-height:20px'>D</div></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn book_paginator_prefers_a_compact_table_in_one_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let caption = line_index_for_character(&core, 'C');
    let first_row = line_index_for_character(&core, 'B');
    let second_row = line_index_for_character(&core, 'D');

    assert!(core.page.view.frame.page().line_positions().get(caption).is_none(), "the table caption must move with the compact table");
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "layout geometry remains continuous");
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(caption).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(first_row).map(|screen| screen.point.y), Some(20.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(second_row).map(|screen| screen.point.y), Some(40.0));
}

#[test]
fn web_paginator_leaves_automatic_table_placement_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 70.0), 1);
    let caption = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(caption).is_some());
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 20.0);
}

#[test]
fn book_paginator_uses_remaining_space_for_a_table_taller_than_a_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let caption = line_index_for_character(&core, 'C');
    let first_row = line_index_for_character(&core, 'B');
    let second_row = line_index_for_character(&core, 'D');

    assert_eq!(core.page.view.frame.page().line_positions().get(caption).map(|screen| screen.point.y), Some(20.0), "an oversized table should use the remainder after preceding content");
    assert_eq!(core.page.view.frame.page().line_positions().get(first_row).map(|screen| screen.point.y), Some(40.0));
    assert!(core.page.view.frame.page().line_positions().get(second_row).is_none(), "the next complete row should move when it no longer fits");
    assert_eq!(core.page.view.document.render_view().text().line(caption).expect("caption line").point().y, 20.0, "layout geometry remains continuous");
}

#[test]
fn book_paginator_breaks_an_oversized_table_between_rows() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_row = line_index_for_character(&core, 'B');
    let first_row_second_line = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(first_row).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(first_row).expect("first row line").point().y, 20.0, "layout geometry remains continuous");
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(first_row).map(|screen| screen.point.y), Some(0.0));
    let first_y = core.page.view.frame.page().line_positions().get(first_row).expect("first row line is visible").point.y;
    let second_y = core.page.view.frame.page().line_positions().get(first_row_second_line).expect("the paginator must not split a row").point.y;
    assert!(second_y > first_y);
}

#[test]
fn book_paginator_repeats_explicit_table_header_on_a_continuation_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></tbody></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 70.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let continued_row = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().repeated_table_headers().len(), 1);
    assert_eq!(core.page.view.frame.page().repeated_table_headers()[0].col_index, 1);
    assert_eq!(core.page.view.frame.page().line_positions().get(continued_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
}

#[test]
fn book_paginator_repeats_a_leading_all_th_row_without_thead() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><th style='padding:0;line-height:20px'>H</th></tr><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 70.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let continued_row = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().repeated_table_headers().len(), 1);
    assert_eq!(core.page.view.frame.page().line_positions().get(continued_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 20.0)));
}

#[test]
fn book_paginator_omits_a_repeated_header_when_no_body_row_would_fit() {
    let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H<br>I</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr></tbody></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 60.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let body_row = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().repeated_table_headers().is_empty());
    assert_eq!(core.page.view.frame.page().line_positions().get(body_row).map(|screen| (screen.col_index, screen.point.y)), Some((1, 0.0)));
}

#[test]
fn table_row_boundary_does_not_move_the_previous_rows_last_line() {
    let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>A<br>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(500.0, 50.0), 2, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first = line_index_for_character(&core, 'A');
    let first_last = line_index_for_character(&core, 'B');
    let second = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.frame.page().line_positions().get(first).map(|screen| screen.col_index), Some(0));
    assert_eq!(core.page.view.frame.page().line_positions().get(first_last).map(|screen| screen.col_index), Some(0), "a line ending exactly at the next row's top still belongs to the previous row");
    assert_eq!(core.page.view.frame.page().line_positions().get(second).map(|screen| screen.col_index), Some(1));
}

#[test]
fn book_paginator_does_not_break_through_a_rowspan_group() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>R</td><td style='padding:0;line-height:20px'>Q</td></tr><tr><td rowspan='2' style='padding:0;line-height:20px'>S</td><td style='padding:0;line-height:20px'>T</td></tr><tr><td style='padding:0;line-height:20px'>U</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_group_row = line_index_for_character(&core, 'T');
    let second_group_row = line_index_for_character(&core, 'U');

    for _ in 0..4 {
        if core.page.view.frame.page().line_positions().get(first_group_row).is_some() {
            break;
        }
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
    }
    assert!(core.page.view.frame.page().line_positions().get(first_group_row).is_some(), "the rowspan group must eventually be visible");
    assert!(core.page.view.frame.page().line_positions().get(second_group_row).is_some(), "rows connected by a rowspan must stay on the same page");
}

#[test]
fn book_paginator_keeps_a_bottom_table_caption_with_the_last_row() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><caption style='caption-side:bottom;line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(html, Size::new(200.0, 50.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let last_row = line_index_for_character(&core, 'B');
    let caption = line_index_for_character(&core, 'C');

    let row_y = core.page.view.document.render_view().text().line(last_row).expect("last row line").point().y;
    let caption_line = core.page.view.document.render_view().text().line(caption).expect("bottom caption line");
    assert_eq!(row_y, 40.0, "layout geometry remains continuous");
    let (group_top, group_bottom) = core.paginator().table_row_group_bounds_containing(row_y).expect("the final row owns a pagination group");
    assert_eq!(group_top, row_y);
    assert!(group_bottom >= caption_line.point().y + caption_line.height(), "the final row's pagination group must include its bottom caption");
}

#[test]
fn authored_forced_break_between_table_rows_wins() {
    let html =
        "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr style='break-after:page'><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 60.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_row = line_index_for_character(&core, 'A');
    let second_row = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.document.render_view().text().line(first_row).expect("first row line").point().y, 0.0);
    assert_eq!(core.page.view.document.render_view().text().line(second_row).expect("second row line").point().y, 20.0, "forced pagination does not alter layout geometry");
    assert!(core.page.view.frame.page().line_positions().get(second_row).is_none());
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert!(core.page.view.frame.page().repeated_table_headers().is_empty(), "ordinary td rows must not be inferred as table headers");
    assert_eq!(core.page.view.frame.page().line_positions().get(second_row).map(|screen| screen.point.y), Some(0.0));
}

#[test]
fn automatic_table_placement_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 70.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_applies_default_widow_and_orphan_limits() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "a 1/1 paragraph split violates the default 2-line limits");
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 40.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 60.0);
}

#[test]
fn book_paginator_applies_line_limits_without_changing_continuous_geometry() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(html, Size::new(200.0, 40.0), 1, html::pipeline::TextCompositionPolicy::BookOptimized);
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 40.0);
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| screen.point.y), Some(0.0));
    assert_eq!(core.page.view.frame.page().line_positions().get(c).map(|screen| screen.point.y), Some(20.0));
}

#[test]
fn paginator_honors_authored_widow_and_orphan_limits() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(b).is_some(), "an authored one-line orphan allowance should permit B at the column bottom");
    assert!(core.page.view.frame.page().line_positions().get(c).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0);
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 40.0);
}

#[test]
fn paginator_roundtrips_backward_across_a_semantic_break() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_keeps_table_cell_lines_with_equal_document_y_together() {
    let html = "<html><body style='margin:0'><table style='border-spacing:0'><tr><td style='padding:0;line-height:20px'>A</td><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 20.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let source_a = core.page.view.document.render_view().text().line(a).expect("first cell line").point().y;
    let source_b = core.page.view.document.render_view().text().line(b).expect("second cell line").point().y;
    let screen_a = core.page.view.frame.page().line_positions().get(a).expect("first cell visible");
    let screen_b = core.page.view.frame.page().line_positions().get(b).expect("second cell visible");

    assert_eq!(source_a, source_b, "table cell lines must exercise the equal-document-y path");
    assert_eq!(screen_a.col_index, screen_b.col_index);
    assert_eq!(screen_a.point.y, screen_b.point.y);
}

#[test]
fn text_hit_testing_distinguishes_adjacent_table_cells_at_the_same_y() {
    let html = "<html><body style='margin:0'><table style='border-spacing:0;table-layout:fixed;width:200px'><tr><td style='padding:0;line-height:20px'>LEFT</td><td style='padding:0;line-height:20px'>RIGHT</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let left_glyph = glyph_index_for_character(&core, 'L');
    let right_glyph = glyph_index_for_character(&core, 'R');
    let left_line_idx = core.page.view.document.render_view().text().line_index_for_glyph(left_glyph).expect("left cell line");
    let right_line_idx = core.page.view.document.render_view().text().line_index_for_glyph(right_glyph).expect("right cell line");
    let left_line = core.page.view.document.render_view().text().line(left_line_idx).expect("left cell geometry");
    let right_line = core.page.view.document.render_view().text().line(right_line_idx).expect("right cell geometry");
    let left_screen = core.page.view.frame.page().line_positions().get(left_line_idx).expect("left cell visible");
    let right_screen = core.page.view.frame.page().line_positions().get(right_line_idx).expect("right cell visible");
    let left_point = Point::new(left_screen.point.x + core.text_geometry().glyph_x_in_line(&left_line, left_glyph) + 0.1, left_screen.point.y + left_line.height() / 2.0);
    let right_point = Point::new(right_screen.point.x + core.text_geometry().glyph_x_in_line(&right_line, right_glyph) + 0.1, right_screen.point.y + right_line.height() / 2.0);

    assert_eq!(left_screen.point.y, right_screen.point.y, "fixture must exercise vertically overlapping cell lines");
    assert_eq!(core.text_geometry().hit_test_glyph(left_point), Some(left_glyph));
    assert_eq!(core.text_geometry().hit_test_glyph(right_point), Some(right_glyph));
}

#[test]
fn semantic_selection_promotes_table_cells_and_embeds_a_markdown_table() {
    let html = "<html><body style='margin:0'><p>Before</p><table><tr><th>Head A</th><th>Head B</th></tr><tr><td>One</td><td>Two</td></tr></table><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let glyph_count = core.page.view.document.render_view().text().glyph_count() as u32;

    core.page.view.selection.select_range(0, glyph_count, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    let markdown = core.page.view.selection.content().markdown().expect("semantic Markdown selection");
    assert!(markdown.contains("Before"));
    assert!(markdown.contains("| Head A | Head B |\n| --- | --- |\n| One | Two |"));
    assert!(markdown.contains("After"));
    assert_eq!(core.selection_view().semantic_cell_glyph_ranges(0, glyph_count).len(), 4);

    let one = glyph_index_for_character(&core, 'O');
    core.page.view.selection.select_range(one, one + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(core.page.view.selection.content().plain(), Some("O"), "annotation text remains tied to the actual glyph range");
    assert_eq!(core.page.view.selection.content().markdown(), Some("|  |\n| --- |\n| One |"));
    assert_eq!(core.selection_view().semantic_cell_glyph_ranges(one, one + 1).len(), 1);
}

#[test]
fn semantic_selection_promotes_complete_nested_list_items() {
    let html = "<html><body style='margin:0'><p>Before</p><ol start='5'><li>Alpha<ul><li>Beta</li></ul></li><li>Gamma</li></ol><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let alpha = glyph_index_for_character(&core, 'p');

    core.page.view.selection.select_range(alpha, alpha + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(core.page.view.selection.content().plain(), Some("p"), "annotation text remains tied to the actual glyph range");
    assert_eq!(core.page.view.selection.content().markdown(), Some("5. Alpha\n    - Beta"));
    let promoted = core.selection_view().semantic_promoted_glyph_ranges(alpha, alpha + 1);
    assert!(promoted.len() >= 2, "the complete parent and nested item text should be selected");

    let beta = glyph_index_for_character(&core, 't');
    core.page.view.selection.select_range(beta, beta + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();
    assert_eq!(core.page.view.selection.content().markdown(), Some("- Beta"), "a nested item selected alone should remain a portable standalone list item");
}

#[test]
fn semantic_selection_promotes_code_blocks_only_when_crossing_their_boundary() {
    let html = "<html><body style='margin:0'><p>Before</p><pre><code class='language-rust'>let x = ```;\nnext();</code></pre><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let selected = glyph_index_for_character(&core, 'x');

    core.page.view.selection.select_range(selected, selected + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(core.page.view.selection.content().plain(), Some("x"));
    assert_eq!(core.page.view.selection.content().markdown(), Some("`x`"));
    assert!(core.selection_view().semantic_promoted_glyph_ranges(selected, selected + 1).is_empty(), "a selection contained by the code block must remain granular");

    let before = glyph_index_for_character(&core, 'B');
    core.page.view.selection.select_range(before, selected + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    let markdown = core.page.view.selection.content().markdown().expect("cross-boundary Markdown selection");
    assert!(markdown.contains("Before"));
    assert!(markdown.contains("````rust\nlet x = ```;\nnext();\n````"));
    assert!(!core.selection_view().semantic_promoted_glyph_ranges(before, selected + 1).is_empty(), "crossing into the code block must promote it completely");
}

#[test]
fn paginator_forward_then_uncached_backward_restores_the_same_page_geometry() {
    let html =
        "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div><div style='line-height:20px'>D</div><div style='line-height:20px'>E</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line());

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_ne!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range, "forward navigation must reach a different page");

    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    assert!(matches!(core.viewport.composition().direction(), crate::PaintDirection::Backward));
    core.prepare_backward();

    assert_eq!((core.page.view.frame.page().start_line(), core.page.view.frame.page().end_line()), first_range);
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn core_loads_and_navigates_between_documents_without_a_window_backend() {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let fixture = std::env::temp_dir().join(format!("html-view-core-nav-{nonce}"));
    std::fs::create_dir_all(&fixture).unwrap();
    let first = fixture.join("one.html");
    let second = fixture.join("two.html");
    std::fs::write(&first, "<html><body><a href=\"two.html#target\">Next</a></body></html>").unwrap();
    std::fs::write(&second, "<html><body><h1 id=\"target\">Target</h1></body></html>").unwrap();

    let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
    let uris = vec![first.to_string_lossy().into_owned(), second.to_string_lossy().into_owned()];
    let mut shaper = TestShaper::default();
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

    core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
    core.paint_forward(&mut RecordingPainter::default());
    let view = core.page.view.document.render_view();
    let text = view.text();
    let addressing = view.addressing();
    let linked_glyph = (0..text.glyph_count() as u32).find(|glyph| addressing.link_for_glyph(*glyph).is_some()).expect("fixture should contain a linked glyph");
    let line_idx = core.doc().find_line_for_glyph(linked_glyph).expect("linked glyph should be laid out");
    let screen = core.page.view.frame.page().line_positions().get(line_idx).expect("linked line should be painted");
    let line = core.page.view.document.render_view().text().line(line_idx).expect("linked line should be present");
    let position = Point::new(screen.point.x + core.text_geometry().glyph_x_in_line(&line, linked_glyph) + 0.1, screen.point.y + line.height() / 2.0);
    assert!(core.link_at(position));
    assert!(core.begin_selection_at(position, crate::SelectionMode::Plain));
    assert_eq!(core.finish_document_selection().map(|hit| hit.glyph), Some(linked_glyph));

    assert!(activate_glyph(&mut core, &mut shaper, linked_glyph));
    assert_eq!(core.nav.location().document(), 1);
    assert!(core.nav.current_anchor_id(core.nav_view()).as_deref() == Some("target") || core.nav.location().anchor().is_some());
    assert!(core.navigate_history_back(&mut shaper));
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.navigate_history_forward(&mut shaper));
    assert_eq!(core.nav.location().document(), 1);
    assert!(host.events.borrow().iter().any(|event| matches!(event, RendererEvent::TitleChanged(_))));

    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn href_resolution_prefers_the_base_resolved_document_over_an_earlier_basename() {
    let (core, _) = spine_core(
        vec![("part-a/current.html", "<html><body>Current</body></html>"), ("other/chapter.html", "<html><body>Wrong</body></html>"), ("part-a/chapter.html", "<html><body>Right</body></html>")],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    assert_eq!(core.resolve_href("chapter.html"), Some((2, None)));
}

#[test]
fn href_resolution_does_not_guess_from_a_matching_basename() {
    let (core, _) = spine_core(vec![("part-a/current.html", "<html><body>Current</body></html>"), ("other/chapter.html", "<html><body>Other chapter</body></html>")], Size::new(600.0, 800.0), 600.0, 1);

    // The provider resolves this to part-a/chapter.html, which is not in the
    // spine. The same basename in another directory is a different resource.
    assert_eq!(core.resolve_href("chapter.html"), None);
}

#[test]
fn href_resolution_rejects_an_ambiguous_resolved_spine_identifier() {
    let (core, _) = spine_core(
        vec![("part-a/current.html", "<html><body>Current</body></html>"), ("part-a/chapter.html", "<html><body>First occurrence</body></html>"), ("part-a/chapter.html", "<html><body>Second occurrence</body></html>")],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    assert_eq!(core.resolve_href("chapter.html"), None);
}

#[test]
fn a_missing_cross_document_anchor_does_not_change_document_or_history() {
    let (mut core, mut shaper) = spine_core(vec![("current.html", "<html><body>Current</body></html>"), ("target.html", "<html><body><h1 id='present'>Target</h1></body></html>")], Size::new(600.0, 800.0), 600.0, 1);

    assert!(!core.navigate_to_href(&mut shaper, "target.html#missing"));
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.nav.history.current().is_none());
}

#[test]
fn a_failed_history_target_does_not_advance_the_history_cursor() {
    let (mut core, mut shaper) = spine_core(vec![("current.html", "<html><body>Current</body></html>"), ("target.html", "<html><body><h1 id='present'>Target</h1></body></html>")], Size::new(600.0, 800.0), 600.0, 1);
    let host = core.host.clone();
    let current = html_view_navigate::Location::new(0, None);
    let invalid = html_view_navigate::Location::new(1, Some("missing".to_owned()));
    core.nav.history.push(current.clone(), host.as_ref());
    core.nav.history.push(invalid, host.as_ref());
    assert_eq!(core.nav.history.step(-1, host.as_ref()), Some(current.clone()));

    assert!(!core.navigate_history_forward(&mut shaper));
    assert_eq!(core.nav.location().document(), 0);
    assert_eq!(core.nav.history.current(), Some(&current));
}

#[test]
fn commands_preserve_search_selection_and_settings_without_a_window_backend() {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let fixture = std::env::temp_dir().join(format!("html-view-core-commands-{nonce}"));
    std::fs::create_dir_all(&fixture).unwrap();
    let document_path = fixture.join("index.html");
    std::fs::write(&document_path, "<html><head><title>Commands</title></head><body><p id=\"spoken\"><strong>Needle</strong> and needle.</p></body></html>").unwrap();

    let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
    let uris = vec![document_path.to_string_lossy().into_owned()];
    let mut shaper = TestShaper::default();
    let loaded = load::load_document(provider.clone(), &uris[0], 16.0, 600.0, &mut shaper);
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::new(host.clone(), loaded, provider, uris, 0, RendererInitialConfig::default());
    assert_eq!(core.page.inputs.layout.image_sizing_policy, html::pipeline::ImageSizingPolicy::SmartStandalone, "reader UI documents should enable smart standalone image sizing at initial layout");

    core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
    core.apply(&mut shaper, crate::RendererCommand::SetScale(1.5));
    assert_eq!(core.page.view.layout.base_col_width, 480.0);
    assert_eq!(core.root_font_size, 18.0);
    assert_eq!(core.page.view.layout.scale, 1.5);
    let reader_overrides_before_theme = core.page.inputs.reader_overrides.clone();
    let repaint_before_theme = host.repaint_requests.get();
    core.apply(&mut shaper, crate::RendererCommand::SetReaderPaintPalette(crate::ReaderPaintPalette { foreground: Some(0xe7e2d8ff), background: Some(0x171916ff) }));
    assert_eq!(core.page.inputs.reader_overrides, reader_overrides_before_theme, "theme changes must not rerun the style pipeline");
    assert!(host.repaint_requests.get() > repaint_before_theme);
    let width_event_count = host.events.borrow().iter().filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_))).count();
    let repaint_count = host.repaint_requests.get();
    core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
    assert_eq!(host.events.borrow().iter().filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_))).count(), width_event_count,);
    assert_eq!(host.repaint_requests.get(), repaint_count);

    core.apply(&mut shaper, crate::RendererCommand::ActivateSearch);
    core.apply(&mut shaper, crate::RendererCommand::SetSearchQuery("needle".to_string()));
    assert_eq!(core.highlight.matches().len(), 2);
    assert_eq!(core.highlight.current_match_index(), 0);
    core.apply(&mut shaper, crate::RendererCommand::NavigateSearch(1));
    assert_eq!(core.highlight.current_match_index(), 1);

    let (start, end) = core.highlight.matches()[0];
    core.page.view.selection.select_range(start, end, crate::SelectionMode::Plain);
    core.page.view.update_selection_text();
    assert_eq!(core.page.view.selection.content().plain(), Some("Needle"));
    assert_eq!(core.page.view.selection.content().markdown(), Some("**Needle**"));
    let range_cfi = crate::cfi::generate_cfi_range(&core.page.view.document, 0, start, end).expect("selection has a range CFI");
    assert_eq!(crate::cfi::parse_cfi_range(&core.page.view.document, &range_cfi), Some((0, start, end)));
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetAnnotations(vec![crate::RendererAnnotation {
            id: "annotation-1".to_owned(),
            cfi_range: range_cfi,
            exact_text: "Needle".to_owned(),
            prefix: None,
            suffix: None,
            style: crate::AnnotationStyle::Highlight,
            color: [255, 220, 0, 96],
        }]),
    );
    assert!(core.annotations.activate_at_glyph(start, core.host.as_ref()));
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetAnnotations(vec![crate::RendererAnnotation {
            id: "annotation-recovered".to_owned(),
            cfi_range: "epubcfi(/6/2!/999,/1:0,/1:6)".to_owned(),
            exact_text: "Needle".to_owned(),
            prefix: None,
            suffix: Some(" and".to_owned()),
            style: crate::AnnotationStyle::Underline,
            color: [255, 220, 0, 96],
        }]),
    );
    assert!(core.annotations.activate_at_glyph(start, core.host.as_ref()));
    core.apply(&mut shaper, crate::RendererCommand::SetMediaOverlayTarget(Some("#spoken".to_owned())));
    assert!(core.media_overlay.visible().is_some_and(|(overlay_start, overlay_end)| overlay_start <= start && overlay_end > end));
    core.apply(&mut shaper, crate::RendererCommand::SetMediaOverlayTarget(None));
    assert_eq!(core.media_overlay.visible(), None);
    assert_eq!(core.copy_selection_to_clipboard(), Ok(true));
    assert_eq!(host.clipboard_text.borrow().as_deref(), Some("**Needle**"));

    core.configure_layout_for_viewport(&mut shaper, Size::new(1_600.0, 800.0));
    assert_eq!(core.page.view.layout.col_count, 2.0);
    assert!(core.page.view.layout.col_gap >= 0.0);

    let events = host.events.borrow();
    assert!(events.iter().any(|event| matches!(event, RendererEvent::ColumnWidthChanged(width) if *width == 480.0)));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 18.0)));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::ScaleChanged(scale) if *scale == 1.5)));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::SearchActiveChanged(true))));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::MatchInfo { current: 2, total: 2 })));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-1")));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-recovered")));
    assert!(host.repaint_requests.get() > 0);
    drop(events);

    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn failed_pipeline_update_keeps_committed_settings_and_reports_the_error() {
    let (provider, _) = CountingProvider::new(vec![("doc.html", "<html><body><p>Transactional settings</p></body></html>")]);
    let provider: Arc<dyn crate::ResourceProvider> = provider;
    let mut shaper = TestShaper::default();
    let loaded = load::load_document(provider.clone(), "doc.html", 16.0, 600.0, &mut shaper);
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::new(host.clone(), loaded, provider, vec!["doc.html".to_owned()], 0, RendererInitialConfig::default());
    let committed_size = core.root_font_size;
    let committed_input_size = core.page.inputs.style_environment.root_font_size;
    host.events.borrow_mut().clear();

    shaper.reject_glyphs = true;
    core.set_root_font_size(&mut shaper, 24.0);

    assert_eq!(core.root_font_size, committed_size);
    assert_eq!(core.page.inputs.style_environment.root_font_size, committed_input_size);
    let events = host.events.borrow();
    assert!(!events.iter().any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 24.0)));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::OperationFailed { operation: RendererOperation::UpdateDocument, message } if !message.is_empty())));
}

#[test]
fn pre_frame_relayout_preserves_the_document_start_before_the_first_glyph() {
    let (provider, _) = CountingProvider::new(vec![("document.html", "<html><body style='margin:0'><div style='height:100px;background:green'></div><p style='margin:0'>First text</p></body></html>")]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, None, RendererInitialConfig::default());

    assert_eq!(core.viewport.composition().start_offset_y(), 0.0);
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
    assert_eq!(core.viewport.composition().start_offset_y(), 0.0, "relayout before the first frame must retain leading non-text content");

    core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
    core.paint_forward(&mut RecordingPainter::default());
    let document_y = core.page.view.document.render_view().text().line(0).expect("fixture has a text line").point().y;
    let screen_y = core.page.view.frame.page().line_positions().get(0).expect("first line is visible").point.y;
    assert_eq!(screen_y, document_y, "the first frame must not scroll directly to its first glyph");
}

#[test]
fn omitted_nav_state_stays_at_document_start_when_semantic_relayout_moves_a_later_paragraph() {
    let html = "<html><body style='margin:10px'><h1 style='margin:0;line-height:20px'>Heading</h1><div style='height:30px'></div><p style='margin:0;line-height:20px;widows:2;orphans:2'>B<br>C</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, None, RendererInitialConfig::default());

    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 80.0));
    core.paint_forward(&mut RecordingPainter::default());

    let heading = line_index_for_character(&core, 'H');
    let moved = line_index_for_character(&core, 'B');
    assert_eq!(core.viewport.composition().start_offset_y(), 0.0, "a publication without saved state must retain the true document origin");
    assert_eq!(core.page.view.frame.page().start_line(), Some(heading));
    assert!(core.page.view.frame.page().line_positions().get(heading).is_some());
    assert!(core.page.view.frame.page().line_positions().get(moved).is_none(), "the protected paragraph should move without becoming the restored startup anchor");
    assert_eq!(core.page.view.document.render_view().text().line(moved).expect("moved line").point().y, 60.0, "continuous layout must not contain a pagination gap");
}

#[test]
fn navigation_reuses_cached_documents_between_adjacent_spine_entries() {
    let (provider, read_count) = CountingProvider::new(vec![("doc0.html", "<html><body><a href=\"doc1.html\">next</a></body></html>"), ("doc1.html", "<html><body><a href=\"doc0.html\">prev</a></body></html>")]);

    let host = Rc::new(TestHost::default());
    let uris = vec!["doc0.html".to_owned(), "doc1.html".to_owned()];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

    let indexed_baseline = read_count.load(Ordering::Relaxed);
    assert_eq!(indexed_baseline, 1);

    core.next_document(&mut shaper);
    assert_eq!(core.nav.location().document(), 1);
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);

    core.prev_document(&mut shaper);
    assert_eq!(core.nav.location().document(), 0);
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);

    core.next_document(&mut shaper);
    assert_eq!(core.nav.location().document(), 1);
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 1);
}

#[test]
fn navigation_cache_uses_eviction_when_capacity_is_exceeded() {
    let (provider, read_count) = CountingProvider::new(vec![
        ("doc0.html", "<html><body><p>doc0</p></body></html>"),
        ("doc1.html", "<html><body><p>doc1</p></body></html>"),
        ("doc2.html", "<html><body><p>doc2</p></body></html>"),
        ("doc3.html", "<html><body><p>doc3</p></body></html>"),
        ("doc4.html", "<html><body><p>doc4</p></body></html>"),
    ]);

    let host = Rc::new(TestHost::default());
    let uris = vec!["doc0.html".to_owned(), "doc1.html".to_owned(), "doc2.html".to_owned(), "doc3.html".to_owned(), "doc4.html".to_owned()];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());

    let indexed_baseline = read_count.load(Ordering::Relaxed);
    assert_eq!(indexed_baseline, 1);

    for _ in 0..4 {
        core.next_document(&mut shaper);
    }
    assert_eq!(core.nav.location().document(), 4);
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 4);

    while core.nav.location().document() > 0 {
        core.prev_document(&mut shaper);
    }
    assert_eq!(core.nav.location().document(), 0);

    assert!(core.nav.history.is_at_newest(), "walking back to the first document should leave the history cursor at its newest entry");
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 6);
}

#[test]
fn publication_search_stops_between_spine_documents_when_cancelled() {
    let (provider, read_count) = CountingProvider::new(vec![("doc0.html", "<p>needle first</p>"), ("doc1.html", "<p>needle second</p>"), ("doc2.html", "<p>needle third</p>")]);
    let documents = vec!["doc0.html".to_owned(), "doc1.html".to_owned(), "doc2.html".to_owned()];
    let cancellation_checks = Cell::new(0usize);

    let mut streamed = Vec::new();
    let matches = crate::search_publication_streaming(
        provider.as_ref(),
        &documents,
        "needle",
        crate::SearchOptions::default(),
        || {
            let checks = cancellation_checks.get();
            cancellation_checks.set(checks + 1);
            checks > 1
        },
        |batch| streamed.extend_from_slice(batch),
    );

    assert_eq!(matches.len(), 1);
    assert_eq!(streamed.len(), 1);
    assert_eq!(matches[0].doc, 0);
    assert_eq!(read_count.load(Ordering::Relaxed), 1);
}

#[test]
fn progress_weights_reuse_the_search_index_text() {
    let source = "<html><body><p>one two</p><aside role='doc-footnote'>hidden note</aside><script>ignored</script></body></html>";
    let (provider, _) = CountingProvider::new(vec![("doc.html", source)]);
    let documents = vec!["doc.html".to_owned()];

    assert_eq!(crate::publication_text_lengths(provider.as_ref(), &documents), vec![6]);
    assert_eq!(crate::search_publication(provider.as_ref(), &documents, "two", crate::SearchOptions::default()).len(), 1);
    assert!(crate::search_publication(provider.as_ref(), &documents, "hidden", crate::SearchOptions::default()).is_empty());
}

#[test]
fn publication_search_joins_to_layout_by_source_position_not_ordinal() {
    let source = "<html><body><p id='a'>needle A</p><p style='display:none'>needle hidden</p><p id='b'>needle B</p><p id='c'>needle C</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("doc.html", source)]);
    let documents = vec!["doc.html".to_owned()];
    let options = crate::SearchOptions { scope: crate::SearchScope::WholeBook, ..Default::default() };
    let results = crate::search_publication(provider.as_ref(), &documents, "needle", options);
    assert_eq!(results.len(), 4, "parse-level search sees text hidden later by style");

    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, documents, 0, None, RendererInitialConfig::default());
    core.apply(&mut shaper, crate::RendererCommand::SetSearchResults { query: "needle".to_owned(), options, results: vec![results[2].clone()] });

    let expected = core.doc().glyph_range_for_anchor("b").expect("the visible target has glyphs").0;
    assert_eq!(core.highlight.current_local_match_start(), Some(expected));
}

#[test]
fn footnote_preview_requires_note_semantics_and_resolves_cross_spine_targets() {
    let (provider, read_count) = CountingProvider::new(vec![
        ("chapter.html", "<p><a epub:type=\"noteref\" href=\"notes.html#n1\">1</a><a href=\"notes.html#ordinary\">more</a></p>"),
        ("notes.html", "<aside id=\"n1\" epub:type=\"footnote\"><p><em>Structured</em> note text</p><ul><li>First source</li></ul></aside><section id=\"ordinary\">Ordinary section</section>"),
    ]);
    let host = Rc::new(TestHost::default());
    let uris = vec!["chapter.html".to_owned(), "notes.html".to_owned()];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), RendererInitialConfig::default());
    let initial_reads = read_count.load(Ordering::Relaxed);

    assert!(core.footnote_preview(&mut shaper, "notes.html#n1").is_some());
    let rendered_note = laid_out_text(&core.note.as_ref().expect("open note").document);
    assert!(rendered_note.contains("Structured note text"));
    assert!(rendered_note.contains("First source"));
    let indexed_reads = read_count.load(Ordering::Relaxed);
    assert_eq!(indexed_reads, initial_reads + 1, "the target spine document should be indexed once on first use");
    assert_eq!(core.footnote_preview(&mut shaper, "notes.html#ordinary"), None);
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_reads, "subsequent targets in the same spine document must reuse its semantic index");
}

#[test]
fn note_reference_classification_is_per_link_even_when_hrefs_match() {
    let html = "<html><body><p><a href='#target'>R</a><a role='doc-noteref' href='#target'>N</a></p><div id='target'><p>Popup body</p></div></body></html>";
    let (provider, read_count) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
    let regular = glyph_index_for_character(&core, 'R');
    let noteref = glyph_index_for_character(&core, 'N');
    let addressing = core.page.view.document.render_view().addressing();

    assert!(!addressing.is_note_reference(regular));
    assert!(addressing.is_note_reference(noteref));
    let reads_after_preparation = read_count.load(Ordering::Relaxed);

    assert!(activate_glyph(&mut core, &mut shaper, regular));
    assert!(!host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(_))), "an ordinary link sharing the href must navigate normally");
    host.events.borrow_mut().clear();

    assert!(activate_glyph(&mut core, &mut shaper, noteref));
    assert!(host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(preview) if preview.href == "#target")));
    assert_eq!(read_count.load(Ordering::Relaxed), reads_after_preparation, "same-document previews must use the index built during preparation");
}

#[test]
fn semantic_footnotes_are_popup_only_and_absent_from_paginated_text() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);
    let text = core.page.view.document.render_view().text();
    let noteref = glyph_index_for_character(&core, '1');
    assert!(core.page.view.document.render_view().addressing().is_note_reference(noteref));
    let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();

    assert!(visible_source.contains("Reading"));
    assert!(visible_source.contains("Continues"));
    assert!(!visible_source.contains("zebra"), "the footnote body must not consume paginated layout space");
    assert!(core.footnote_preview(&mut shaper, "#note").is_some());
    assert!(laid_out_text(&core.note.as_ref().expect("open note").document).contains("Popup-only zebra"));
}

#[test]
fn clicked_footnote_emits_a_viewport_overlay_anchor() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup</p></aside></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());
    let noteref = glyph_index_for_character(&core, '1');
    let line_idx = core.doc().find_line_for_glyph(noteref).expect("note reference line");
    let screen = core.page.view.frame.page().line_positions().get(line_idx).expect("painted note reference line");
    let line = core.page.view.document.render_view().text().line(line_idx).expect("laid out note reference line");
    let click = Point::new(screen.point.x + core.text_geometry().glyph_x_in_line(&line, noteref) + 0.1, screen.point.y + line.height() / 2.0);
    assert!(click.x < 100.0 && click.y < 100.0, "fixture click is in the top-left viewport quadrant");

    let mut session = crate::RendererSession::from_core(core, shaper);
    assert!(session.pointer_down(click, crate::PointerDownOptions::default()));
    assert!(session.pointer_up());

    assert!(host.events.borrow().iter().any(|event| {
        matches!(
            event,
            RendererEvent::FootnoteOpened(preview)
                if preview.anchor == Some(crate::FootnotePopupAnchor {
                    point: click,
                    corner: crate::PopupAnchorCorner::TopLeft,
                })
        )
    }));
}

#[test]
fn as_authored_notes_read_inline_instead_of_being_held_for_a_popup() {
    // The same fixture as the popup test above, differing only in the
    // display mode. Under `AsAuthored` the reader injects no suppression
    // rule, so the note reads where its author placed it.
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width: 200.0,
        max_column_count: Some(1),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
        note_display: crate::NoteDisplay::AsAuthored,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let text = core.page.view.document.render_view().text();
    let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();
    assert!(visible_source.contains("zebra"), "an as-authored note occupies the reading flow rather than being held back for a popup");

    // The engine's note semantics are untouched by the display mode: the
    // target is still a note, which is what a later mode switch relies on.
    assert!(core.page.view.document.render_view().addressing().is_note_target("note"));
}

#[test]
fn switching_note_display_relays_out_the_document() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);

    let flow_text = |core: &RendererCore| {
        let text = core.page.view.document.render_view().text();
        (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>()
    };
    assert!(!flow_text(&core).contains("zebra"), "the note starts held back for a popup");

    core.apply(&mut shaper, crate::RendererCommand::SetNoteDisplay(crate::NoteDisplay::AsAuthored));

    assert!(flow_text(&core).contains("zebra"), "switching to as-authored must rebuild the document with the note in flow");
    assert_eq!(core.footnote_preview(&mut shaper, "#note"), None, "and the reference stops opening a popup");

    core.apply(&mut shaper, crate::RendererCommand::SetNoteDisplay(crate::NoteDisplay::Popup));
    assert!(!flow_text(&core).contains("zebra"), "switching back holds the note out of the flow again");
}

#[test]
fn a_previewed_note_paints_its_own_content() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);
    core.footnote_preview(&mut shaper, "#note").expect("the note must preview");

    let scene = core.note_scene().expect("a previewed note must form a scene");
    let mut painter = RecordingPainter::default();
    scene.paint(&mut painter);

    // The note is drawn from its own layout through the engine's fragment
    // renderer, rather than through the page's viewport, which holds no
    // glyphs for it at all.
    assert!(!painter.glyphs.is_empty(), "a previewed note must paint its own glyphs");
    assert!(scene.content_height() > 0.0, "a note's scene must have height");
}

/// A reader over a spine, laid out in columns of `column_width`. The
/// spine is the HTML; anything else a fixture lists is a resource the
/// documents refer to.
fn spine_core(documents: Vec<(&str, &str)>, viewport: Size, column_width: f64, max_columns: u8) -> (RendererCore, TestShaper) {
    let uris = documents.iter().filter(|(uri, _)| uri.ends_with(".html")).map(|(uri, _)| (*uri).to_owned()).collect::<Vec<_>>();
    let (provider, _) = CountingProvider::new(documents);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width,
        max_column_count: Some(max_columns),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), config);
    core.prepare_frame(&mut shaper, viewport);
    (core, shaper)
}

#[test]
fn a_page_turn_opens_the_next_document() {
    let one = "<html><body style='margin:0'><p>First document</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("one.html", one), ("two.html", one)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width: 200.0,
        max_column_count: Some(2),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
        ..RendererInitialConfig::default()
    };
    let uris = vec!["one.html".to_owned(), "two.html".to_owned()];
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, uris, 0, Some("0:0"), config);
    core.prepare_frame(&mut shaper, Size::new(420.0, 300.0));

    core.next_page(&mut shaper);

    assert_eq!(core.nav.location().document(), 1);
    assert_eq!(core.viewport.composition().start_offset_y(), 0.0, "and it opens at the start, none of it having been shown");
}

#[test]
fn text_can_be_selected_inside_an_open_note() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Selectable note body</p></aside></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(300.0, 300.0), 1);
    core.footnote_preview(&mut shaper, "#note").expect("the note must preview");

    // A note is a document view, so selection is the page's code applied
    // to it rather than a second implementation.
    let note = core.note.as_ref().expect("an open note is held as a view");
    let last_line = note.doc().text().line_count().saturating_sub(1);
    let bottom = note.doc().text().line(last_line).map(|line| line.point().y + line.height()).unwrap_or(10.0);

    assert!(core.note.as_mut().expect("open note").begin_selection_at(Point::new(1.0, 1.0), crate::SelectionMode::Plain), "a press inside the note starts a selection");
    assert!(core.note.as_mut().expect("open note").update_selection_at(Point::new(280.0, bottom - 1.0), crate::SelectionMode::Plain), "dragging extends it");
    core.note.as_mut().expect("open note").finish_selection();

    let selected = core.note.as_ref().expect("open note").selection_text().expect("the note yields selected text");
    assert!(selected.contains("Selectable"), "the note's own text is selected, not the page's: {selected:?}");

    core.close_note();
    assert!(core.note.as_ref().is_none(), "closing the note drops what was selected in it");
}

#[test]
fn note_selection_uses_the_shared_clipboard_operation() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Clipboard note body</p></aside></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
    core.configure_layout_for_viewport(&mut shaper, Size::new(300.0, 300.0));
    core.footnote_preview(&mut shaper, "#note").expect("the note must preview");
    let mut session = crate::RendererSession::from_core(core, shaper);
    let bottom = session.note_scene().expect("open note scene").content_height();

    assert!(session.note_pointer_down(Point::new(1.0, 1.0), crate::SelectionMode::Plain));
    assert!(session.note_pointer_move(Point::new(280.0, bottom - 1.0), crate::SelectionMode::Plain));
    assert!(session.note_pointer_up());
    assert!(session.copy_selection());
    assert!(host.clipboard_text.borrow().as_deref().is_some_and(|text| text.contains("Clipboard note body")));
}

#[test]
fn a_note_is_laid_out_to_the_width_its_host_will_show_it_at() {
    // The reader hands back a note ready to paint, so the host's popup
    // width has to shape it here -- a note laid out to the page and shown
    // in a narrow panel would break its lines in the wrong places.
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>a note long enough that the width it is laid out to decides how many lines it occupies</p></aside></body></html>";

    let lines_at = |width: Option<f64>| {
        let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
        let host = Rc::new(TestHost::default());
        host.note_width.set(width);
        let mut shaper = TestShaper::default();
        let config = RendererInitialConfig { font_size: 16.0, column_width: 600.0, max_column_count: Some(1), ..RendererInitialConfig::default() };
        let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);
        core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 400.0));
        core.footnote_preview(&mut shaper, "#note").expect("the note must preview");
        core.note.as_ref().expect("open note").document.render_view().text().line_count()
    };

    let wide = lines_at(None);
    let narrow = lines_at(Some(120.0));

    assert!(narrow > wide, "a narrower popup must wrap the note into more lines ({narrow} at 120px vs {wide} at the column width)");
}

#[test]
fn an_as_authored_note_reference_navigates_rather_than_opening_a_popup() {
    // The note reads in place under this mode, so a popup would repeat
    // text already on the page. The reference resolves as a plain link.
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width: 200.0,
        max_column_count: Some(1),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
        note_display: crate::NoteDisplay::AsAuthored,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), config);
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let noteref = glyph_index_for_character(&core, '1');
    assert!(core.page.view.document.render_view().addressing().is_note_reference(noteref), "the reference keeps its note semantics; only the presentation differs");

    assert!(activate_glyph(&mut core, &mut shaper, noteref));

    assert!(!host.events.borrow().iter().any(|event| matches!(event, RendererEvent::FootnoteOpened(_))), "a note that reads in place must not also be shown as a popup");
    assert_eq!(core.footnote_preview(&mut shaper, "#note"), None);
}

#[test]
fn namespaced_xhtml_footnotes_are_popup_only_and_absent_from_paginated_text() {
    let html = r##"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><p>Reading<a epub:type="noteref" href="#note">1</a></p><aside id="note" epub:type="footnote"><p>Popup-only magenta</p></aside><p>Continues</p></body>
</html>"##;
    let (provider, _) = CountingProvider::new(vec![("document.xhtml", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.xhtml".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let text = core.page.view.document.render_view().text();
    let noteref = glyph_index_for_character(&core, '1');
    assert!(core.page.view.document.render_view().addressing().is_note_reference(noteref), "namespaced EPUB noteref semantics must survive preparation");
    let visible_source = (0..text.glyph_count()).filter_map(|index| text.glyph_at(index).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>();
    assert!(visible_source.contains("Reading"));
    assert!(visible_source.contains("Continues"));
    assert!(!visible_source.contains("magenta"), "the namespaced XHTML footnote body must not consume paginated layout space");
    assert!(core.footnote_preview(&mut shaper, "#note").is_some());
    assert!(laid_out_text(&core.note.as_ref().expect("open note").document).contains("Popup-only magenta"));
}
