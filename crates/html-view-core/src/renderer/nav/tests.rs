//! Renderer navigation, pagination, and note regressions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kurbo::{Point, Size};

use crate::text_backend::{FontSlant, GlyphId, GlyphMetric, GlyphResourceStore, GlyphShaper};
use crate::{
    FileSystemProvider, RecordingPainter, RendererCore, RendererEvent, RendererHost,
    RendererInitialConfig, RendererOperation, load,
};

#[derive(Default)]
struct TestShaper {
    glyphs: HashMap<(char, u32), GlyphId>,
    glyph_store: GlyphResourceStore,
    append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceStore)>,
    next_id: GlyphId,
    reject_glyphs: bool,
}

impl GlyphShaper for TestShaper {
    fn reset(&mut self) {
        self.glyphs.clear();
        self.glyph_store.clear();
        self.next_id = 0;
    }

    fn glyph_resources(&mut self) -> &mut GlyphResourceStore {
        &mut self.glyph_store
    }

    fn begin_append_shaping(&mut self) -> Result<(), crate::layout::ShapeError> {
        assert!(self.append_checkpoint.is_none());
        self.append_checkpoint = Some((self.glyphs.clone(), self.glyph_store.clone()));
        Ok(())
    }

    fn commit_append_shaping(&mut self) {
        self.append_checkpoint = None;
    }

    fn rollback_append_shaping(&mut self) {
        if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
            self.glyphs = glyphs;
            self.glyph_store = glyph_store;
            self.next_id = self.glyph_store.len() as GlyphId;
        }
    }

    fn shape_glyph(
        &mut self,
        ch: char,
        font_size: f32,
        _font_weight: u16,
        _font_slant: FontSlant,
        _color: u32,
        _family: Option<&str>,
    ) -> Result<GlyphId, crate::layout::ShapeError> {
        if self.reject_glyphs {
            return Err(crate::layout::ShapeError::unregistered_glyph_id(
                u32::MAX,
                self.glyph_store.len(),
            ));
        }
        let key = (ch, font_size.to_bits());
        if let Some(&glyph) = self.glyphs.get(&key)
            && self.glyph_store.contains(glyph)
        {
            return Ok(glyph);
        }
        let metric = GlyphMetric::try_new(
            ch,
            font_size * 0.5,
            font_size * 0.75,
            font_size * 0.25,
            font_size * 0.75,
        )
        .map_err(crate::layout::ShapeError::rejected_metric)?;
        let glyph = self.glyph_store.register(metric)?;
        assert_eq!(
            glyph, self.next_id,
            "glyph shaper and document registry diverged"
        );
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
    font_preparations: RefCell<Vec<Vec<crate::FontFace>>>,
    events: RefCell<Vec<RendererEvent>>,
    repaint_requests: Cell<usize>,
    clipboard_text: RefCell<Option<String>>,
    note_width: Cell<Option<f64>>,
    jobs: Option<Arc<DeferredFileJobs>>,
}

impl RendererHost for TestHost {
    fn prepare_font_faces(&self, faces: &[crate::FontFace], _: Arc<dyn crate::ResourceProvider>) {
        self.font_preparations.borrow_mut().push(faces.to_vec());
    }

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
        self.jobs.as_ref().map(|_| Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>)
    }
    fn blocking_job_spawner(&self) -> Arc<dyn html::resources::BlockingJobSpawner> {
        self.jobs.clone().map(|jobs| jobs as Arc<dyn html::resources::BlockingJobSpawner>).unwrap_or_else(|| Arc::new(html::resources::InlineJobSpawner))
    }
    fn set_clipboard(&self, text: &str) -> Result<(), String> {
        *self.clipboard_text.borrow_mut() = Some(text.to_owned());
        Ok(())
    }
    fn set_clipboard_image(
        &self,
        _width: usize,
        _height: usize,
        _rgba: Vec<u8>,
    ) -> Result<(), String> {
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
        (
            Arc::new(Self {
                documents: mapped,
                read_count: read_count.clone(),
            }),
            read_count,
        )
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
        self.documents
            .get(uri)
            .cloned()
            .map(|value| value.into_bytes())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, uri.to_owned()))
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
    pagination_core_with_policy(
        source,
        viewport,
        max_columns,
        html::pipeline::TextCompositionPolicy::WebCompatible,
    )
}

fn pagination_core_with_policy(
    source: &str,
    viewport: Size,
    max_columns: u8,
    text_composition_policy: html::pipeline::TextCompositionPolicy,
) -> (RendererCore, TestShaper) {
    pagination_core_with_policy_at_uri(
        "document.html",
        source,
        viewport,
        max_columns,
        text_composition_policy,
    )
}

fn pagination_core_with_policy_at_uri(
    uri: &str,
    source: &str,
    viewport: Size,
    max_columns: u8,
    text_composition_policy: html::pipeline::TextCompositionPolicy,
) -> (RendererCore, TestShaper) {
    let (provider, _) = CountingProvider::new(vec![(uri, source)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width: 200.0,
        max_column_count: Some(max_columns),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec![uri.to_owned()],
        0,
        Some("0:0"),
        config,
    );
    core.configure_layout_for_viewport(&mut shaper, viewport);
    core.paint_forward(&mut RecordingPainter::default());
    (core, shaper)
}

#[test]
fn default_reader_column_count_is_derived_only_from_available_width() {
    let (provider, _) = CountingProvider::new(vec![(
        "document.html",
        "<html><body>Wide reader</body></html>",
    )]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        column_width: 200.0,
        max_column_count: None,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        config,
    );

    core.configure_layout_for_viewport(&mut shaper, Size::new(1_100.0, 600.0));

    assert_eq!(core.page.view.layout.col_count, 5.0);
}

#[test]
fn column_viewport_plan_does_not_repaint_unchanged_frames() {
    for (viewport, width, columns, gap, scale) in [
        (Size::new(360.0, 640.0), 320.0, 1, 24.0, 1.0),
        (Size::new(900.0, 640.0), 320.0, 2, 24.0, 1.0),
        (Size::new(900.0, 640.0), 320.0, 2, 24.0, 1.25),
    ] {
        let (mut core, mut shaper) = pagination_core(
            "<html><body>Idle reader</body></html>",
            viewport,
            columns,
        );
        let host = Rc::new(TestHost::default());
        core.host = host.clone();
        core.set_scale(scale);
        // The final element bounds may be narrower than the shell's plan.
        let bounds = Size::new(viewport.width - 2.0, viewport.height);
        core.set_column_viewport_plan(&mut shaper, viewport.width, width, columns, gap);
        core.prepare_viewport(bounds);
        let geometry = core.page.view.layout.geometry();
        host.repaint_requests.set(0);

        // Painting derives the actual gap from the available space, rather
        // than retaining the minimum gap supplied by the reader shell.
        assert_ne!(geometry.gap, gap);
        for _ in 0..5 {
            core.set_column_viewport_plan(&mut shaper, viewport.width, width, columns, gap);
            core.prepare_viewport(bounds);
        }

        assert_eq!(host.repaint_requests.get(), 0, "an unchanged frame must settle");
        assert_eq!(core.page.view.layout.geometry(), geometry);
    }
}

#[test]
fn column_viewport_plan_repaints_changes_and_restores_changed_layout() {
    let (mut core, mut shaper) = pagination_core(
        "<html><body>Resizing reader</body></html>",
        Size::new(900.0, 640.0),
        2,
    );
    let host = Rc::new(TestHost::default());
    core.host = host.clone();
    for (available, width, columns, gap) in [
        (900.0, 320.0, 2, 24.0),
        (920.0, 320.0, 2, 24.0),
        (920.0, 340.0, 2, 24.0),
        (920.0, 340.0, 1, 24.0),
        (920.0, 340.0, 1, 32.0),
    ] {
        let before = host.repaint_requests.get();
        core.set_column_viewport_plan(&mut shaper, available, width, columns, gap);
        assert!(host.repaint_requests.get() > before);
        core.prepare_viewport(Size::new(available, 640.0));
    }

    core.set_col_width(&mut shaper, 400.0);
    let before = host.repaint_requests.get();
    core.set_column_viewport_plan(&mut shaper, 920.0, 340.0, 1, 32.0);
    assert!(host.repaint_requests.get() > before);
    assert_eq!(core.page.view.layout.base_col_width, 340.0);
    assert_eq!(core.page.view.layout.col_width, 340.0);
}

fn line_index_for_character(core: &RendererCore, wanted: char) -> usize {
    let text = core.page.view.document.render_view().text();
    let glyph = glyph_index_for_character(core, wanted);
    text.line_index_for_glyph(glyph)
        .expect("fixture character must own a line")
}

fn glyph_index_for_character(core: &RendererCore, wanted: char) -> u32 {
    let text = core.page.view.document.render_view().text();
    (0..text.glyph_count() as u32)
        .find(|index| {
            text.glyph_at(*index as usize)
                .and_then(|glyph| text.glyph_metric(glyph))
                .is_some_and(|metric| metric.ch() == wanted)
        })
        .expect("fixture character must exist")
}

fn laid_out_text(document: &crate::LaidOutDocument) -> String {
    let text = document.render_view().text();
    (0..text.glyph_count())
        .filter_map(|index| {
            text.glyph_at(index)
                .and_then(|glyph| text.glyph_metric(glyph))
                .map(|metric| metric.ch())
        })
        .collect()
}

fn visible_positions(core: &RendererCore) -> Vec<(usize, i32, f64)> {
    core.page
        .view
        .frame
        .page()
        .line_positions()
        .iter()
        .map(|(line, screen)| (line, screen.col_index, screen.point.y))
        .collect()
}

#[test]
fn optional_vertical_rhythm_only_stretches_existing_block_gaps() {
    let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0 0 10px;line-height:20px'>B</p><p style='margin:0;line-height:20px'>C</p></body></html>";
    let (mut natural, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 75.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let natural_positions = visible_positions(&natural);
    let a = line_index_for_character(&natural, 'A');
    let b = line_index_for_character(&natural, 'B');

    natural.vertical_rhythm = true;
    natural.paint_forward(&mut RecordingPainter::default());
    let refined_positions = visible_positions(&natural);

    assert_eq!(
        natural_positions
            .iter()
            .map(|(line, column, _)| (*line, *column))
            .collect::<Vec<_>>(),
        refined_positions
            .iter()
            .map(|(line, column, _)| (*line, *column))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        natural
            .page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .map(|screen| screen.point.y),
        Some(0.0),
        "the first line remains anchored"
    );
    assert_eq!(
        natural
            .page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| screen.point.y),
        Some(32.0),
        "the sole safe block gap receives the 2px cap"
    );
    assert_eq!(
        natural
            .page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .map(|line| line.point().y),
        Some(30.0),
        "continuous CSS layout is untouched"
    );
}

#[test]
fn vertical_rhythm_leaves_the_terminal_document_column_ragged() {
    let html = "<html><body style='margin:0'><p style='margin:0 0 10px;line-height:20px'>A</p><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (mut core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 75.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
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

    assert_eq!(
        visible_positions(&core),
        before,
        "WPT/browser-compatible geometry must remain greedy and unadjusted"
    );
}

#[test]
fn book_paginator_keeps_a_compact_list_item_intact() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px'>L<br>M</li></ul></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 50.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first = line_index_for_character(&core, 'L');
    let second = line_index_for_character(&core, 'M');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 20.0))
    );
}

#[test]
fn book_paginator_keeps_a_term_with_the_first_two_definition_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><dl style='margin:0'><dt style='line-height:20px'>T</dt><dd style='margin:0;line-height:20px'>D<br>E</dd></dl></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let term = line_index_for_character(&core, 'T');
    let definition = line_index_for_character(&core, 'D');
    let definition_second = line_index_for_character(&core, 'E');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(term)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(definition)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 20.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(definition_second)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 40.0))
    );
}

#[test]
fn book_paginator_keeps_compact_preformatted_code_and_splits_oversized_code() {
    let compact = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD</code></pre></body></html>";
    let (compact_core, _) = pagination_core_with_policy(
        compact,
        Size::new(420.0, 50.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let compact_first = line_index_for_character(&compact_core, 'C');
    let compact_second = line_index_for_character(&compact_core, 'D');
    let compact_first_position = compact_core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(compact_first)
        .expect("first compact code line");
    let compact_second_position = compact_core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(compact_second)
        .expect("second compact code line");
    assert_eq!(
        (
            compact_first_position.col_index,
            compact_second_position.col_index
        ),
        (1, 1)
    );
    assert!(compact_second_position.point.y > compact_first_position.point.y);

    let oversized = "<html><body style='margin:0'><div style='line-height:20px'>A</div><pre style='margin:0;line-height:20px'><code>C\nD\nE\nF</code></pre></body></html>";
    let (oversized_core, _) = pagination_core_with_policy(
        oversized,
        Size::new(640.0, 50.0),
        3,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let oversized_first = line_index_for_character(&oversized_core, 'C');
    let oversized_last = line_index_for_character(&oversized_core, 'F');
    let first_column = oversized_core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(oversized_first)
        .map(|screen| screen.col_index)
        .expect("first oversized code line");
    let last_column = oversized_core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(oversized_last)
        .map(|screen| screen.col_index)
        .expect("last oversized code line");
    assert!(
        last_column > first_column,
        "oversized code must fall back to splitting across columns"
    );
}

#[test]
fn web_paginator_does_not_apply_semantic_keeps() {
    // Disable CSS line limits so this isolates the optional list-item keep.
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><ul style='margin:0;padding:0;list-style:none'><li style='line-height:20px;widows:1;orphans:1'>L<br>M</li></ul></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
    let first = line_index_for_character(&core, 'L');
    let second = line_index_for_character(&core, 'M');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second)
            .map(|screen| screen.col_index),
        Some(1)
    );
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
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .is_none(),
        "the first line beyond the exact bottom edge belongs to the next page"
    );
}

#[test]
fn paginator_moves_an_overflowing_line_whole_to_the_next_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 39.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let first = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(a)
        .expect("first line visible");
    let second = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(b)
        .expect("overflow line visible in the following column");

    assert_eq!(first.col_index, 0);
    assert_eq!(second.col_index, 1);
    assert_eq!(
        second.point.y, 0.0,
        "an overflowing line must restart at the column top"
    );
}

#[test]
fn paginator_counts_document_vertical_gaps_when_fitting_a_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='height:10px'></div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let first = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(a)
        .expect("first line visible");
    let second = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(b)
        .expect("second line visible");

    assert!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("second source line")
            .point()
            .y
            > core
                .page
                .view
                .document
                .render_view()
                .text()
                .line(a)
                .expect("first source line")
                .point()
                .y
                + 20.0
    );
    assert_eq!((first.col_index, second.col_index), (0, 1));
    assert_eq!(second.point.y, 0.0);
}

#[test]
fn paginator_uses_tall_inline_image_height_for_column_fitting() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A<img src='missing.png' style='width:10px;height:35px'></div><div style='line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let images = core
        .page
        .view
        .document
        .render_view()
        .fragments()
        .images_for_line(a);

    assert!(
        images.iter().any(|image| image.size().height == 35.0),
        "fixture must retain the 35px replaced-content height"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| screen.col_index),
        Some(1)
    );
}

#[test]
fn paginator_places_an_oversized_line_once_and_advances_to_the_next_page() {
    let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');

    assert_eq!(core.page.view.frame.page().start_line(), Some(a));
    assert_eq!(core.page.view.frame.page().end_line(), Some(a));
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_none()
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());

    assert_eq!(core.page.view.frame.page().start_line(), Some(b));
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_some(),
        "pagination must make progress beyond the oversized line"
    );
}

#[test]
fn paginator_roundtrips_backward_across_an_oversized_line() {
    let html = "<html><body style='margin:0'><div style='line-height:60px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_moves_an_oversized_image_line_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div><img src='missing.png' style='width:10px;height:60px;vertical-align:top'>B</div><div style='line-height:20px'>C</div></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 40.0), 2);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .is_none(),
        "the oversized image consumes its destination column"
    );
}

#[test]
fn paginator_honors_forced_modern_and_legacy_css_breaks() {
    for property in ["break-before:page", "page-break-before:always"] {
        let html = format!("<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>");
        let (mut core, mut shaper) = pagination_core(&html, Size::new(200.0, 40.0), 1);
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert!(core.page.view.frame.page().line_positions().get(a).is_some());
        assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "{property} must start B in the next fragmentainer");
        assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0, "document geometry stays continuous");
        // Page placement changes without introducing gaps into the document.
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
    }
}

#[test]
fn book_paginator_distinguishes_column_breaks_from_page_breaks() {
    let column_html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;break-before:column'>B</div></body></html>";
    let (column_core, _) = pagination_core_with_policy(
        column_html,
        Size::new(700.0, 60.0),
        3,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let column_b = line_index_for_character(&column_core, 'B');
    assert_eq!(
        column_core
            .page
            .view
            .frame
            .page()
            .line_positions()
            .get(column_b)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );

    for property in [
        "break-before:page",
        "page-break-before:always",
        "break-before:left",
        "break-before:right",
    ] {
        let html = format!(
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px;{property}'>B</div></body></html>"
        );
        let (mut core, mut shaper) = pagination_core_with_policy(
            &html,
            Size::new(700.0, 60.0),
            3,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let b = line_index_for_character(&core, 'B');

        assert!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(b)
                .is_none(),
            "{property} must skip the remaining reader columns"
        );
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(b)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((0, 0.0)),
            "{property} must begin at the next reader page origin"
        );
    }
}

#[test]
fn book_paginator_anchors_forced_breaks_on_empty_boxes_to_following_content() {
    for property in ["break-before:column", "break-after:column"] {
        let html = format!(
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='{property}'></div><div style='line-height:20px'>B</div></body></html>"
        );
        let (core, _) = pagination_core_with_policy(
            &html,
            Size::new(420.0, 60.0),
            2,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let a = line_index_for_character(&core, 'A');
        let b = line_index_for_character(&core, 'B');

        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(a)
                .map(|screen| screen.col_index),
            Some(0)
        );
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(b)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((1, 0.0)),
            "{property} on an empty box must anchor to B"
        );
    }
}

#[test]
fn forced_break_inside_an_avoided_container_takes_precedence() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 50.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0)),
        "avoid must not move content preceding an internal forced break"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
}

#[test]
fn forced_column_break_does_not_leave_a_margin_only_empty_column() {
    let html = "<html><body style='margin:0'><div style='height:90px;margin-bottom:20px;line-height:20px'>A</div><div style='break-before:column;line-height:20px'>B</div></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(700.0, 100.0),
        3,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let b = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("B line")
            .point()
            .y,
        110.0,
        "continuous layout must retain the ordinary block margin"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
}

#[test]
fn book_paginator_applies_break_after_without_fragmenting_continuous_layout() {
    for (property, first_page_has_b) in [
        ("break-after:column", true),
        ("break-after:page", false),
        ("page-break-after:always", false),
    ] {
        let html = format!(
            "<html><body style='margin:0'><div style='height:30px;margin-bottom:10px;line-height:20px;{property}'>A</div><div style='line-height:20px'>B</div></body></html>"
        );
        let (mut core, mut shaper) = pagination_core_with_policy(
            &html,
            Size::new(700.0, 60.0),
            3,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let b = line_index_for_character(&core, 'B');

        assert_eq!(
            core.page
                .view
                .document
                .render_view()
                .text()
                .line(b)
                .expect("B line")
                .point()
                .y,
            40.0,
            "forced pagination must not rewrite continuous document geometry"
        );
        if first_page_has_b {
            assert_eq!(
                core.page
                    .view
                    .frame
                    .page()
                    .line_positions()
                    .get(b)
                    .map(|screen| (screen.col_index, screen.point.y)),
                Some((1, 0.0))
            );
        } else {
            assert!(
                core.page
                    .view
                    .frame
                    .page()
                    .line_positions()
                    .get(b)
                    .is_none(),
                "{property} must skip the remaining columns"
            );
            core.next_page(&mut shaper);
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(
                core.page
                    .view
                    .frame
                    .page()
                    .line_positions()
                    .get(b)
                    .map(|screen| (screen.col_index, screen.point.y)),
                Some((0, 0.0))
            );
        }
    }
}

#[test]
fn paginator_forced_break_wins_over_an_adjoining_avoid() {
    let html = "<html><body style='margin:0'><div style='line-height:20px;break-after:page'>A</div><div style='line-height:20px;break-before:avoid'>B</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(a).expect("A line").point().y, 0.0);
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0, "document geometry stays continuous");
    // Page placement changes without introducing gaps into the document.
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
}

#[test]
fn paginator_keeps_a_break_inside_avoid_block_together() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0, "document geometry stays continuous");
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 40.0, "document geometry stays continuous");
    // Page placement changes without introducing gaps into the document.
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(c).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn book_paginator_applies_break_inside_avoid_without_changing_continuous_geometry() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 40.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("B line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(c)
            .expect("C line")
            .point()
            .y,
        40.0
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_none()
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
}

#[test]
fn paginator_honors_break_after_avoid_as_keep_with_next() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:avoid'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(heading).is_none());
    assert_eq!(core.page.view.document.render_view().text().line(heading).expect("heading line").point().y, 20.0, "document geometry stays continuous");
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("following line").point().y, 40.0, "document geometry stays continuous");
    // Page placement changes without introducing gaps into the document.
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(heading).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn book_paginator_automatically_keeps_a_heading_with_two_following_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D<br>E</p></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let a = line_index_for_character(&core, 'A');
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(a)
            .is_some()
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_none(),
        "the heading must move instead of becoming the last line of the page"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(heading)
            .expect("heading line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("first paragraph line")
            .point()
            .y,
        40.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(c)
            .expect("second paragraph line")
            .point()
            .y,
        60.0
    );
}

#[test]
fn web_paginator_leaves_automatic_heading_keep_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let heading = line_index_for_character(&core, 'H');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_some(),
        "web-compatible pagination must retain browser-style heading placement"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(heading)
            .expect("heading line")
            .point()
            .y,
        20.0
    );
}

#[test]
fn book_paginator_moves_a_large_multiline_heading_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H<br>I</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 80.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let heading = line_index_for_character(&core, 'H');
    let second_heading_line = line_index_for_character(&core, 'I');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_none()
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(heading)
            .expect("heading line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(second_heading_line)
            .expect("second heading line")
            .point()
            .y,
        40.0
    );
}

#[test]
fn book_paginator_does_not_move_an_oversized_heading_group() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;widows:1;orphans:1'>H<br>I<br>J</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let heading = line_index_for_character(&core, 'H');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_some(),
        "a group taller than a full column must use best-effort pagination"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(heading)
            .expect("heading line")
            .point()
            .y,
        20.0
    );
}

#[test]
fn authored_forced_break_after_heading_overrides_automatic_keep() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px;break-after:page'>H</h2><p style='margin:0;line-height:20px'>B</p></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let heading = line_index_for_character(&core, 'H');
    let b = line_index_for_character(&core, 'B');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_some(),
        "the authored forced break must leave the heading before the break"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_none(),
        "the following paragraph must begin on the next reader page"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(heading)
            .expect("heading line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("following line")
            .point()
            .y,
        40.0,
        "forced pagination must not insert a gap into continuous layout"
    );
}

#[test]
fn automatic_heading_keep_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><h2 style='margin:0;line-height:20px'>H</h2><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C<br>D</p></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn book_paginator_keeps_compact_semantic_sidebars_in_one_column() {
    for (uri, semantic, html) in [
        (
            "document.html",
            "aside",
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>",
        ),
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
        let (core, _) = pagination_core_with_policy_at_uri(
            uri,
            html,
            Size::new(420.0, 50.0),
            2,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let sidebar = line_index_for_character(&core, 'S');
        let sidebar_end = line_index_for_character(&core, 'T');

        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(sidebar)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((1, 0.0)),
            "{semantic} must move as one compact block"
        );
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(sidebar_end)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((1, 20.0))
        );
    }
}

#[test]
fn book_paginator_keeps_bounded_compact_semantic_structures_in_one_column() {
    for (uri, semantic, attribute) in [
        (
            "document.xhtml",
            "EPUB dedication",
            "epub:type='dedication'",
        ),
        ("document.xhtml", "EPUB abstract", "epub:type='abstract'"),
        ("document.xhtml", "EPUB notice", "epub:type='notice'"),
        ("document.xhtml", "EPUB pullquote", "epub:type='pullquote'"),
        (
            "document.xhtml",
            "prefixed EPUB theorem",
            "epub:type='math:theorem'",
        ),
        ("document.xhtml", "EPUB proof", "epub:type='proof'"),
        ("document.xhtml", "EPUB example", "epub:type='example'"),
        ("document.xhtml", "EPUB exercise", "epub:type='exercise'"),
        ("document.html", "ARIA abstract", "role='doc-abstract'"),
        ("document.html", "ARIA example", "role='doc-example'"),
    ] {
        let html = format!(
            "<html xmlns:epub='http://www.idpf.org/2007/ops'><body style='margin:0'><div style='line-height:20px'>A</div><section {attribute} style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>"
        );
        let (core, _) = pagination_core_with_policy_at_uri(
            uri,
            &html,
            Size::new(420.0, 50.0),
            2,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let start = line_index_for_character(&core, 'B');
        let end = line_index_for_character(&core, 'C');

        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(start)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((1, 0.0)),
            "{semantic} must move as one compact structure"
        );
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(end)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((1, 20.0)),
            "{semantic} must remain intact"
        );
    }
}

#[test]
fn book_paginator_splits_an_oversized_semantic_structure_normally() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='theorem' style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let start = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(start)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0)),
        "an oversized semantic structure must use available space instead of moving as one unit"
    );
}

#[test]
fn authored_break_inside_a_compact_semantic_structure_wins() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='abstract' style='margin:0'><div style='line-height:20px'>B</div><div style='line-height:20px;break-before:column'>C</div></section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 50.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let start = line_index_for_character(&core, 'B');
    let after_break = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(start)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(after_break)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
}

#[test]
fn book_paginator_keeps_a_semantic_statement_label_with_two_content_lines() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>X</div><section epub:type='theorem' style='margin:0'><p epub:type='label' style='margin:0;line-height:20px'>L</p><p style='margin:0;line-height:20px'>B<br>C<br>D<br>E<br>F<br>G<br>H<br>I<br>J</p></section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let label = line_index_for_character(&core, 'L');
    let first = line_index_for_character(&core, 'B');
    let second = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(label)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 20.0))
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 40.0))
    );
}

#[test]
fn web_paginator_does_not_add_semantic_sidebar_placement() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><aside epub:type='sidebar' style='margin:0'><div style='line-height:20px'>S</div><div style='line-height:20px'>T</div></aside></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 50.0), 2);
    let sidebar = line_index_for_character(&core, 'S');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(sidebar)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0))
    );
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
        let (mut core, mut shaper) = pagination_core_with_policy_at_uri(
            "document.xhtml",
            &html,
            Size::new(420.0, 60.0),
            2,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let section = line_index_for_character(&core, 'B');

        assert!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(section)
                .is_none(),
            "EPUB {semantic} must skip all remaining columns on the current reader page"
        );
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(section)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((0, 0.0)),
            "EPUB {semantic} must start at the first column"
        );
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
        let html = format!(
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='region {semantic}' style='margin:0;line-height:20px'>B</section></body></html>"
        );
        let (mut core, mut shaper) = pagination_core_with_policy(
            &html,
            Size::new(420.0, 60.0),
            2,
            html::pipeline::TextCompositionPolicy::BookOptimized,
        );
        let section = line_index_for_character(&core, 'B');

        assert!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(section)
                .is_none(),
            "ARIA {semantic} must skip all remaining columns on the current reader page"
        );
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(section)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((0, 0.0)),
            "ARIA {semantic} must start at the first column"
        );
    }
}

#[test]
fn generic_section_remains_in_normal_book_flow() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let section = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(section)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0))
    );
}

#[test]
fn authored_avoid_suppresses_a_semantic_page_start() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px;break-before:avoid'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let section = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(section)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0))
    );
}

#[test]
fn semantic_page_start_at_document_origin_does_not_create_a_blank_page() {
    let html = "<html><body style='margin:0'><section epub:type='titlepage' style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let section = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(section)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 0.0))
    );
}

#[test]
fn web_paginator_does_not_add_expanded_semantic_section_breaks() {
    for (semantic, html) in [
        (
            "EPUB appendix",
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='appendix' style='margin:0;line-height:20px'>B</section></body></html>",
        ),
        (
            "ARIA bibliography",
            "<html><body style='margin:0'><div style='line-height:20px'>A</div><section role='doc-bibliography' style='margin:0;line-height:20px'>B</section></body></html>",
        ),
    ] {
        let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
        let section = line_index_for_character(&core, 'B');

        assert_eq!(
            core.page
                .view
                .frame
                .page()
                .line_positions()
                .get(section)
                .map(|screen| (screen.col_index, screen.point.y)),
            Some((0, 20.0)),
            "{semantic} must remain ordinary flow in web mode"
        );
    }
}

#[test]
fn authored_column_break_overrides_a_semantic_chapter_page_break() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px;break-before:column'>B</section></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(420.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let chapter = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(chapter)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
}

#[test]
fn web_paginator_does_not_add_semantic_chapter_breaks() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section epub:type='chapter' style='margin:0;line-height:20px'>B</section></body></html>";
    let (core, _) = pagination_core(html, Size::new(420.0, 60.0), 2);
    let chapter = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(chapter)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((0, 20.0))
    );
}

#[test]
fn book_paginator_moves_a_compact_figure_and_caption_to_a_fresh_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .is_none(),
        "the compact figure must not be split across columns"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(image_label)
            .expect("figure content line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        40.0
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
}

#[test]
fn book_paginator_keeps_an_image_only_figure_whole() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 30.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let image_line = core
        .page
        .view
        .document
        .render_view()
        .fragments()
        .images()
        .iter()
        .next()
        .expect("fixture image fragment")
        .line_idx();

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_line)
            .is_none(),
        "the image-only figure must move intact"
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_line)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
}

#[test]
fn book_paginator_keeps_a_replaced_image_with_its_multiline_caption() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><img src='missing.png' style='width:10px;height:20px;vertical-align:top'><figcaption style='line-height:20px'>C<br>D</figcaption></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 70.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let image_line = core
        .page
        .view
        .document
        .render_view()
        .fragments()
        .images()
        .iter()
        .next()
        .expect("fixture image fragment")
        .line_idx();
    let caption_first = line_index_for_character(&core, 'C');
    let caption_second = line_index_for_character(&core, 'D');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_line)
            .is_none(),
        "the image must move instead of leaving its caption behind"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption_first)
            .is_none()
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption_second)
            .is_none()
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_line)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption_first)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption_second)
            .map(|screen| screen.point.y),
        Some(40.0)
    );
}

#[test]
fn web_paginator_leaves_automatic_figure_placement_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 50.0), 1);
    let image_label = line_index_for_character(&core, 'I');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .is_some(),
        "web-compatible pagination must preserve normal flow placement"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(image_label)
            .expect("figure content line")
            .point()
            .y,
        20.0
    );
}

#[test]
fn book_paginator_keeps_a_bottom_caption_with_oversized_figure_content() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(image_label)
            .expect("figure content line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        40.0
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .is_none()
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .is_none()
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
}

#[test]
fn book_paginator_keeps_a_top_caption_with_oversized_figure_content() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><figcaption style='line-height:20px'>C</figcaption><div style='line-height:20px'>I</div><div style='height:60px;line-height:20px'>X</div></figure></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let caption = line_index_for_character(&core, 'C');
    let image_label = line_index_for_character(&core, 'I');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(image_label)
            .expect("figure content line")
            .point()
            .y,
        40.0
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .is_none()
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .is_none()
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(image_label)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
}

#[test]
fn authored_forced_break_between_figure_content_and_caption_wins() {
    let html = "<html><body style='margin:0'><figure style='margin:0'><div style='line-height:20px;break-after:page'>I</div><figcaption style='line-height:20px'>C</figcaption></figure></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let image_label = line_index_for_character(&core, 'I');
    let caption = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(image_label)
            .expect("figure content line")
            .point()
            .y,
        0.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        20.0,
        "forced pagination must not insert a gap into continuous layout"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .is_none(),
        "the caption must begin on the next reader page"
    );
}

#[test]
fn automatic_figure_placement_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><figure style='margin:0'><div style='line-height:20px'>I</div><figcaption style='line-height:20px'>C</figcaption></figure><div style='line-height:20px'>D</div></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn book_paginator_prefers_a_compact_table_in_one_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 70.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let caption = line_index_for_character(&core, 'C');
    let first_row = line_index_for_character(&core, 'B');
    let second_row = line_index_for_character(&core, 'D');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .is_none(),
        "the table caption must move with the compact table"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        20.0,
        "layout geometry remains continuous"
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_row)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second_row)
            .map(|screen| screen.point.y),
        Some(40.0)
    );
}

#[test]
fn web_paginator_leaves_automatic_table_placement_disabled() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 70.0), 1);
    let caption = line_index_for_character(&core, 'C');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .is_some()
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        20.0
    );
}

#[test]
fn book_paginator_uses_remaining_space_for_a_table_taller_than_a_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let caption = line_index_for_character(&core, 'C');
    let first_row = line_index_for_character(&core, 'B');
    let second_row = line_index_for_character(&core, 'D');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(caption)
            .map(|screen| screen.point.y),
        Some(20.0),
        "an oversized table should use the remainder after preceding content"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_row)
            .map(|screen| screen.point.y),
        Some(40.0)
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second_row)
            .is_none(),
        "the next complete row should move when it no longer fits"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(caption)
            .expect("caption line")
            .point()
            .y,
        20.0,
        "layout geometry remains continuous"
    );
}

#[test]
fn book_paginator_breaks_an_oversized_table_between_rows() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr><tr><td style='padding:0;line-height:20px'>E</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_row = line_index_for_character(&core, 'B');
    let first_row_second_line = line_index_for_character(&core, 'C');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_row)
            .is_none()
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(first_row)
            .expect("first row line")
            .point()
            .y,
        20.0,
        "layout geometry remains continuous"
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_row)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    let first_y = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(first_row)
        .expect("first row line is visible")
        .point
        .y;
    let second_y = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(first_row_second_line)
        .expect("the paginator must not split a row")
        .point
        .y;
    assert!(second_y > first_y);
}

#[test]
fn book_paginator_repeats_explicit_table_header_on_a_continuation_column() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></tbody></table></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(500.0, 70.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let continued_row = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page.view.frame.page().repeated_table_headers().len(),
        1
    );
    assert_eq!(
        core.page.view.frame.page().repeated_table_headers()[0].col_index,
        1
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(continued_row)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 20.0))
    );
}

#[test]
fn book_paginator_repeats_a_leading_all_th_row_without_thead() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><tr><th style='padding:0;line-height:20px'>H</th></tr><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(500.0, 70.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let continued_row = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page.view.frame.page().repeated_table_headers().len(),
        1
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(continued_row)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 20.0))
    );
}

#[test]
fn book_paginator_omits_a_repeated_header_when_no_body_row_would_fit() {
    let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><thead><tr><th style='padding:0;line-height:20px'>H<br>I</th></tr></thead><tbody><tr><td style='padding:0;line-height:20px'>B<br>C</td></tr></tbody></table></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(500.0, 60.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let body_row = line_index_for_character(&core, 'B');

    assert!(
        core.page
            .view
            .frame
            .page()
            .repeated_table_headers()
            .is_empty()
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(body_row)
            .map(|screen| (screen.col_index, screen.point.y)),
        Some((1, 0.0))
    );
}

#[test]
fn table_row_boundary_does_not_move_the_previous_rows_last_line() {
    let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>A<br>B</td></tr><tr><td style='padding:0;line-height:20px'>C</td></tr></table></body></html>";
    let (core, _) = pagination_core_with_policy(
        html,
        Size::new(500.0, 50.0),
        2,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first = line_index_for_character(&core, 'A');
    let first_last = line_index_for_character(&core, 'B');
    let second = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first)
            .map(|screen| screen.col_index),
        Some(0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_last)
            .map(|screen| screen.col_index),
        Some(0),
        "a line ending exactly at the next row's top still belongs to the previous row"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second)
            .map(|screen| screen.col_index),
        Some(1)
    );
}

#[test]
fn book_paginator_does_not_break_through_a_rowspan_group() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><tr><td style='padding:0;line-height:20px'>R</td><td style='padding:0;line-height:20px'>Q</td></tr><tr><td rowspan='2' style='padding:0;line-height:20px'>S</td><td style='padding:0;line-height:20px'>T</td></tr><tr><td style='padding:0;line-height:20px'>U</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_group_row = line_index_for_character(&core, 'T');
    let second_group_row = line_index_for_character(&core, 'U');

    for _ in 0..4 {
        if core
            .page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_group_row)
            .is_some()
        {
            break;
        }
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
    }
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(first_group_row)
            .is_some(),
        "the rowspan group must eventually be visible"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second_group_row)
            .is_some(),
        "rows connected by a rowspan must stay on the same page"
    );
}

#[test]
fn book_paginator_keeps_a_bottom_table_caption_with_the_last_row() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>P</div><table style='margin:0;border-spacing:0'><caption style='caption-side:bottom;line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 50.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let last_row = line_index_for_character(&core, 'B');
    let caption = line_index_for_character(&core, 'C');

    let row_y = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(last_row)
        .expect("last row line")
        .point()
        .y;
    assert_eq!(row_y, 40.0, "layout geometry remains continuous");
    for _ in 0..4 {
        if core.page.view.frame.page().line_positions().get(last_row).is_some() { break; }
        core.next_page(&mut shaper);
        core.paint_forward(&mut RecordingPainter::default());
    }
    let positions = core.page.view.frame.page().line_positions();
    let row_screen = positions.get(last_row).expect("the final row must become visible");
    let caption_screen = positions.get(caption).expect("the bottom caption must accompany its final row");
    assert_eq!(row_screen.col_index, caption_screen.col_index);
}

#[test]
fn authored_forced_break_between_table_rows_wins() {
    let html = "<html><body style='margin:0'><table style='margin:0;border-spacing:0'><tr style='break-after:page'><td style='padding:0;line-height:20px'>A</td></tr><tr><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 60.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_row = line_index_for_character(&core, 'A');
    let second_row = line_index_for_character(&core, 'B');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(first_row)
            .expect("first row line")
            .point()
            .y,
        0.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(second_row)
            .expect("second row line")
            .point()
            .y,
        20.0,
        "forced pagination does not alter layout geometry"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second_row)
            .is_none()
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert!(
        core.page
            .view
            .frame
            .page()
            .repeated_table_headers()
            .is_empty(),
        "ordinary td rows must not be inferred as table headers"
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(second_row)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
}

#[test]
fn automatic_table_placement_roundtrips_through_backward_pagination() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><table style='margin:0;border-spacing:0'><caption style='line-height:20px'>C</caption><tr><td style='padding:0;line-height:20px'>B</td></tr><tr><td style='padding:0;line-height:20px'>D</td></tr></table></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 70.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_applies_default_widow_and_orphan_limits() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(core.page.view.frame.page().line_positions().get(a).is_some());
    assert!(core.page.view.frame.page().line_positions().get(b).is_none(), "a 1/1 paragraph split violates the default 2-line limits");
    assert_eq!(core.page.view.document.render_view().text().line(b).expect("B line").point().y, 20.0, "document geometry stays continuous");
    assert_eq!(core.page.view.document.render_view().text().line(c).expect("C line").point().y, 40.0, "document geometry stays continuous");
    // Page placement changes without introducing gaps into the document.
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(core.page.view.frame.page().line_positions().get(b).map(|screen| (screen.col_index, screen.point.y)), Some((0, 0.0)));
    assert_eq!(core.page.view.frame.page().line_positions().get(c).map(|screen| (screen.col_index, screen.point.y)), Some((0, 20.0)));
}

#[test]
fn book_paginator_applies_line_limits_without_changing_continuous_geometry() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px'>B<br>C</p></body></html>";
    let (mut core, mut shaper) = pagination_core_with_policy(
        html,
        Size::new(200.0, 40.0),
        1,
        html::pipeline::TextCompositionPolicy::BookOptimized,
    );
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("B line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(c)
            .expect("C line")
            .point()
            .y,
        40.0
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_none()
    );
    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .map(|screen| screen.point.y),
        Some(0.0)
    );
    assert_eq!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .map(|screen| screen.point.y),
        Some(20.0)
    );
}

#[test]
fn paginator_honors_authored_widow_and_orphan_limits() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><p style='margin:0;line-height:20px;widows:1;orphans:1'>B<br>C</p></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let b = line_index_for_character(&core, 'B');
    let c = line_index_for_character(&core, 'C');

    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(b)
            .is_some(),
        "an authored one-line orphan allowance should permit B at the column bottom"
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(c)
            .is_none()
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(b)
            .expect("B line")
            .point()
            .y,
        20.0
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(c)
            .expect("C line")
            .point()
            .y,
        40.0
    );
}

#[test]
fn paginator_roundtrips_backward_across_a_semantic_break() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><section style='break-inside:avoid'><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div></section></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn paginator_keeps_table_cell_lines_with_equal_document_y_together() {
    let html = "<html><body style='margin:0'><table style='border-spacing:0'><tr><td style='padding:0;line-height:20px'>A</td><td style='padding:0;line-height:20px'>B</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 20.0), 1);
    let a = line_index_for_character(&core, 'A');
    let b = line_index_for_character(&core, 'B');
    let source_a = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(a)
        .expect("first cell line")
        .point()
        .y;
    let source_b = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(b)
        .expect("second cell line")
        .point()
        .y;
    let screen_a = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(a)
        .expect("first cell visible");
    let screen_b = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(b)
        .expect("second cell visible");

    assert_eq!(
        source_a, source_b,
        "table cell lines must exercise the equal-document-y path"
    );
    assert_eq!(screen_a.col_index, screen_b.col_index);
    assert_eq!(screen_a.point.y, screen_b.point.y);
}

#[test]
fn text_hit_testing_distinguishes_adjacent_table_cells_at_the_same_y() {
    let html = "<html><body style='margin:0'><table style='border-spacing:0;table-layout:fixed;width:200px'><tr><td style='padding:0;line-height:20px'>LEFT</td><td style='padding:0;line-height:20px'>RIGHT</td></tr></table></body></html>";
    let (core, _) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let left_glyph = glyph_index_for_character(&core, 'L');
    let right_glyph = glyph_index_for_character(&core, 'R');
    let left_line_idx = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line_index_for_glyph(left_glyph)
        .expect("left cell line");
    let right_line_idx = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line_index_for_glyph(right_glyph)
        .expect("right cell line");
    let left_line = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(left_line_idx)
        .expect("left cell geometry");
    let right_line = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(right_line_idx)
        .expect("right cell geometry");
    let left_screen = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(left_line_idx)
        .expect("left cell visible");
    let right_screen = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(right_line_idx)
        .expect("right cell visible");
    let left_point = Point::new(
        left_screen.point.x + core.text_geometry().glyph_x_in_line(&left_line, left_glyph) + 0.1,
        left_screen.point.y + left_line.height() / 2.0,
    );
    let right_point = Point::new(
        right_screen.point.x
            + core
                .text_geometry()
                .glyph_x_in_line(&right_line, right_glyph)
            + 0.1,
        right_screen.point.y + right_line.height() / 2.0,
    );

    assert_eq!(
        left_screen.point.y, right_screen.point.y,
        "fixture must exercise vertically overlapping cell lines"
    );
    assert_eq!(
        core.text_geometry().hit_test_glyph(left_point),
        Some(left_glyph)
    );
    assert_eq!(
        core.text_geometry().hit_test_glyph(right_point),
        Some(right_glyph)
    );
}

#[test]
fn semantic_selection_promotes_table_cells_and_embeds_a_markdown_table() {
    let html = "<html><body style='margin:0'><p>Before</p><table><tr><th>Head A</th><th>Head B</th></tr><tr><td>One</td><td>Two</td></tr></table><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let glyph_count = core.page.view.document.render_view().text().glyph_count() as u32;

    core.page
        .view
        .selection
        .select_range(0, glyph_count, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    let markdown = core
        .page
        .view
        .selection
        .content()
        .markdown()
        .expect("semantic Markdown selection");
    assert!(markdown.contains("Before"));
    assert!(markdown.contains("| Head A | Head B |\n| --- | --- |\n| One | Two |"));
    assert!(markdown.contains("After"));
    assert_eq!(
        core.selection_view()
            .semantic_cell_glyph_ranges(0, glyph_count)
            .len(),
        4
    );

    let one = glyph_index_for_character(&core, 'O');
    core.page
        .view
        .selection
        .select_range(one, one + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(
        core.page.view.selection.content().plain(),
        Some("O"),
        "annotation text remains tied to the actual glyph range"
    );
    assert_eq!(
        core.page.view.selection.content().markdown(),
        Some("|  |\n| --- |\n| One |")
    );
    assert_eq!(
        core.selection_view()
            .semantic_cell_glyph_ranges(one, one + 1)
            .len(),
        1
    );
}

#[test]
fn semantic_selection_promotes_complete_nested_list_items() {
    let html = "<html><body style='margin:0'><p>Before</p><ol start='5'><li>Alpha<ul><li>Beta</li></ul></li><li>Gamma</li></ol><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let alpha = glyph_index_for_character(&core, 'p');

    core.page
        .view
        .selection
        .select_range(alpha, alpha + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(
        core.page.view.selection.content().plain(),
        Some("p"),
        "annotation text remains tied to the actual glyph range"
    );
    assert_eq!(
        core.page.view.selection.content().markdown(),
        Some("5. Alpha\n    - Beta")
    );
    let promoted = core
        .selection_view()
        .semantic_promoted_glyph_ranges(alpha, alpha + 1);
    assert!(
        promoted.len() >= 2,
        "the complete parent and nested item text should be selected"
    );

    let beta = glyph_index_for_character(&core, 't');
    core.page
        .view
        .selection
        .select_range(beta, beta + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();
    assert_eq!(
        core.page.view.selection.content().markdown(),
        Some("- Beta"),
        "a nested item selected alone should remain a portable standalone list item"
    );
}

#[test]
fn semantic_selection_promotes_code_blocks_only_when_crossing_their_boundary() {
    let html = "<html><body style='margin:0'><p>Before</p><pre><code class='language-rust'>let x = ```;\nnext();</code></pre><p>After</p></body></html>";
    let (mut core, _) = pagination_core(html, Size::new(500.0, 300.0), 1);
    let selected = glyph_index_for_character(&core, 'x');

    core.page
        .view
        .selection
        .select_range(selected, selected + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    assert_eq!(core.page.view.selection.content().plain(), Some("x"));
    assert_eq!(core.page.view.selection.content().markdown(), Some("`x`"));
    assert!(
        core.selection_view()
            .semantic_promoted_glyph_ranges(selected, selected + 1)
            .is_empty(),
        "a selection contained by the code block must remain granular"
    );

    let before = glyph_index_for_character(&core, 'B');
    core.page
        .view
        .selection
        .select_range(before, selected + 1, crate::SelectionMode::Semantic);
    core.page.view.update_selection_text();

    let markdown = core
        .page
        .view
        .selection
        .content()
        .markdown()
        .expect("cross-boundary Markdown selection");
    assert!(markdown.contains("Before"));
    assert!(markdown.contains("````rust\nlet x = ```;\nnext();\n````"));
    assert!(
        !core
            .selection_view()
            .semantic_promoted_glyph_ranges(before, selected + 1)
            .is_empty(),
        "crossing into the code block must promote it completely"
    );
}

#[test]
fn paginator_forward_then_uncached_backward_restores_the_same_page_geometry() {
    let html = "<html><body style='margin:0'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div><div style='line-height:20px'>D</div><div style='line-height:20px'>E</div></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 40.0), 1);
    let first_page = visible_positions(&core);
    let first_range = (
        core.page.view.frame.page().start_line(),
        core.page.view.frame.page().end_line(),
    );

    core.next_page(&mut shaper);
    core.paint_forward(&mut RecordingPainter::default());
    assert_ne!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range,
        "forward navigation must reach a different page"
    );

    core.prepared_pages.clear();
    core.prev_page(&mut shaper);
    assert!(matches!(
        core.viewport.composition().direction(),
        crate::PaintDirection::Backward
    ));
    core.prepare_backward();

    assert_eq!(
        (
            core.page.view.frame.page().start_line(),
            core.page.view.frame.page().end_line()
        ),
        first_range
    );
    assert_eq!(visible_positions(&core), first_page);
}

#[test]
fn core_loads_and_navigates_between_documents_without_a_window_backend() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = std::env::temp_dir().join(format!("html-view-core-nav-{nonce}"));
    std::fs::create_dir_all(&fixture).unwrap();
    let first = fixture.join("one.html");
    let second = fixture.join("two.html");
    std::fs::write(
        &first,
        "<html><body><a href=\"two.html#target\">Next</a></body></html>",
    )
    .unwrap();
    std::fs::write(
        &second,
        "<html><body><h1 id=\"target\">Target</h1></body></html>",
    )
    .unwrap();

    let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
    let uris = vec![
        first.to_string_lossy().into_owned(),
        second.to_string_lossy().into_owned(),
    ];
    let mut shaper = TestShaper::default();
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::from_provider_with_nav(
        host.clone(),
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );

    core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
    core.paint_forward(&mut RecordingPainter::default());
    let view = core.page.view.document.render_view();
    let text = view.text();
    let addressing = view.addressing();
    let linked_glyph = (0..text.glyph_count() as u32)
        .find(|glyph| addressing.link_for_glyph(*glyph).is_some())
        .expect("fixture should contain a linked glyph");
    let line_idx = core
        .doc()
        .text().line_index_for_glyph(linked_glyph)
        .expect("linked glyph should be laid out");
    let screen = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(line_idx)
        .expect("linked line should be painted");
    let line = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(line_idx)
        .expect("linked line should be present");
    let position = Point::new(
        screen.point.x + core.text_geometry().glyph_x_in_line(&line, linked_glyph) + 0.1,
        screen.point.y + line.height() / 2.0,
    );
    assert!(core.link_at(position));
    assert!(core.begin_selection_at(position, crate::SelectionMode::Plain));
    assert_eq!(
        core.finish_document_selection().map(|hit| hit.glyph),
        Some(linked_glyph)
    );

    assert!(activate_glyph(&mut core, &mut shaper, linked_glyph));
    assert_eq!(core.nav.location().document(), 1);
    assert!(
        core.nav.current_anchor_id(core.nav_view()).as_deref() == Some("target")
            || core.nav.location().anchor().is_some()
    );
    assert!(core.navigate_history_back(&mut shaper));
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.navigate_history_forward(&mut shaper));
    assert_eq!(core.nav.location().document(), 1);
    assert!(
        host.events
            .borrow()
            .iter()
            .any(|event| matches!(event, RendererEvent::TitleChanged(_)))
    );

    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn href_resolution_prefers_the_base_resolved_document_over_an_earlier_basename() {
    let (core, _) = spine_core(
        vec![
            ("part-a/current.html", "<html><body>Current</body></html>"),
            ("other/chapter.html", "<html><body>Wrong</body></html>"),
            ("part-a/chapter.html", "<html><body>Right</body></html>"),
        ],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    assert_eq!(core.resolve_href("chapter.html"), Some((2, None)));
}

#[test]
fn href_resolution_does_not_guess_from_a_matching_basename() {
    let (core, _) = spine_core(
        vec![
            ("part-a/current.html", "<html><body>Current</body></html>"),
            (
                "other/chapter.html",
                "<html><body>Other chapter</body></html>",
            ),
        ],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    // The provider resolves this to part-a/chapter.html, which is not in the
    // spine. The same basename in another directory is a different resource.
    assert_eq!(core.resolve_href("chapter.html"), None);
}

#[test]
fn href_resolution_rejects_an_ambiguous_resolved_spine_identifier() {
    let (core, _) = spine_core(
        vec![
            ("part-a/current.html", "<html><body>Current</body></html>"),
            (
                "part-a/chapter.html",
                "<html><body>First occurrence</body></html>",
            ),
            (
                "part-a/chapter.html",
                "<html><body>Second occurrence</body></html>",
            ),
        ],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    assert_eq!(core.resolve_href("chapter.html"), None);
}

#[test]
fn a_missing_cross_document_anchor_does_not_change_document_or_history() {
    let (mut core, mut shaper) = spine_core(
        vec![
            ("current.html", "<html><body>Current</body></html>"),
            (
                "target.html",
                "<html><body><h1 id='present'>Target</h1></body></html>",
            ),
        ],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );

    assert!(!core.navigate_to_href(&mut shaper, "target.html#missing"));
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.nav.history.current().is_none());
}

#[test]
fn a_failed_history_target_does_not_advance_the_history_cursor() {
    let (mut core, mut shaper) = spine_core(
        vec![
            ("current.html", "<html><body>Current</body></html>"),
            (
                "target.html",
                "<html><body><h1 id='present'>Target</h1></body></html>",
            ),
        ],
        Size::new(600.0, 800.0),
        600.0,
        1,
    );
    let host = core.host.clone();
    let current = html_view_navigate::Location::new(0, None);
    let invalid = html_view_navigate::Location::new(1, Some("missing".to_owned()));
    core.nav.history.push(current.clone(), host.as_ref());
    core.nav.history.push(invalid, host.as_ref());
    assert_eq!(
        core.nav.history.step(-1, host.as_ref()),
        Some(current.clone())
    );

    assert!(!core.navigate_history_forward(&mut shaper));
    assert_eq!(core.nav.location().document(), 0);
    assert_eq!(core.nav.history.current(), Some(&current));
}

#[test]
fn commands_preserve_search_selection_and_settings_without_a_window_backend() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = std::env::temp_dir().join(format!("html-view-core-commands-{nonce}"));
    std::fs::create_dir_all(&fixture).unwrap();
    let document_path = fixture.join("index.html");
    std::fs::write(&document_path, "<html><head><title>Commands</title></head><body><p id=\"spoken\"><strong>Needle</strong> and needle.</p></body></html>").unwrap();

    let provider: Arc<dyn crate::ResourceProvider> = Arc::new(FileSystemProvider::new());
    let uris = vec![document_path.to_string_lossy().into_owned()];
    let mut shaper = TestShaper::default();
    let loaded = load::load_document(provider.clone(), &uris[0], 16.0, 600.0, &mut shaper);
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::new(
        host.clone(),
        loaded,
        provider,
        uris,
        0,
        RendererInitialConfig::default(),
    );
    assert_eq!(
        core.page.inputs.layout.image_sizing_policy,
        html::pipeline::ImageSizingPolicy::SmartStandalone,
        "reader UI documents should enable smart standalone image sizing at initial layout"
    );

    core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
    core.apply(&mut shaper, crate::RendererCommand::SetScale(1.5));
    assert_eq!(core.page.view.layout.base_col_width, 480.0);
    assert_eq!(core.root_font_size, 18.0);
    assert_eq!(core.page.view.layout.scale, 1.5);
    let reader_overrides_before_theme = core.page.inputs.reader_overrides.clone();
    let repaint_before_theme = host.repaint_requests.get();
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetReaderPaintPalette(crate::ReaderPaintPalette {
            foreground: Some(0xe7e2d8ff),
            background: Some(0x171916ff),
        }),
    );
    assert_eq!(
        core.page.inputs.reader_overrides, reader_overrides_before_theme,
        "theme changes must not rerun the style pipeline"
    );
    assert!(host.repaint_requests.get() > repaint_before_theme);
    let width_event_count = host
        .events
        .borrow()
        .iter()
        .filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_)))
        .count();
    let repaint_count = host.repaint_requests.get();
    core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(480.0));
    assert_eq!(
        host.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, RendererEvent::ColumnWidthChanged(_)))
            .count(),
        width_event_count,
    );
    assert_eq!(host.repaint_requests.get(), repaint_count);

    core.apply(&mut shaper, crate::RendererCommand::ActivateSearch);
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetSearchQuery("needle".to_string()),
    );
    assert_eq!(core.highlight.matches().len(), 2);
    assert_eq!(core.highlight.current_match_index(), 0);
    core.apply(&mut shaper, crate::RendererCommand::NavigateSearch(1));
    assert_eq!(core.highlight.current_match_index(), 1);

    let range = &core.highlight.matches()[0];
    let (start, end) = (range.start, range.end);
    core.page
        .view
        .selection
        .select_range(start, end, crate::SelectionMode::Plain);
    core.page.view.update_selection_text();
    assert_eq!(core.page.view.selection.content().plain(), Some("Needle"));
    assert_eq!(
        core.page.view.selection.content().markdown(),
        Some("**Needle**")
    );
    let range_cfi = crate::cfi::generate_cfi_range(&core.page.view.document, 0, start, end)
        .expect("selection has a range CFI");
    assert_eq!(
        crate::cfi::parse_cfi_range(&core.page.view.document, &range_cfi),
        Some((0, start, end))
    );
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
    assert!(
        core.annotations
            .activate_at_glyph(start, core.host.as_ref())
    );
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
    assert!(
        core.annotations
            .activate_at_glyph(start, core.host.as_ref())
    );
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetMediaOverlayTarget(Some("#spoken".to_owned())),
    );
    assert!(core.media_overlay.visible().is_some_and(|range| range.start <= start && range.end > end));
    core.apply(
        &mut shaper,
        crate::RendererCommand::SetMediaOverlayTarget(None),
    );
    assert_eq!(core.media_overlay.visible(), None);
    assert_eq!(core.copy_selection_to_clipboard(), Ok(true));
    assert_eq!(host.clipboard_text.borrow().as_deref(), Some("**Needle**"));

    core.configure_layout_for_viewport(&mut shaper, Size::new(1_600.0, 800.0));
    assert_eq!(core.page.view.layout.col_count, 2.0);
    assert!(core.page.view.layout.col_gap >= 0.0);

    let events = host.events.borrow();
    assert!(
        events.iter().any(
            |event| matches!(event, RendererEvent::ColumnWidthChanged(width) if *width == 480.0)
        )
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 18.0))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RendererEvent::ScaleChanged(scale) if *scale == 1.5))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RendererEvent::SearchActiveChanged(true)))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RendererEvent::MatchInfo {
            current: 2,
            total: 2
        }
    )));
    assert!(events.iter().any(
        |event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-1")
    ));
    assert!(events.iter().any(|event| matches!(event, RendererEvent::AnnotationActivated { id } if id == "annotation-recovered")));
    assert!(host.repaint_requests.get() > 0);
    drop(events);

    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn failed_pipeline_update_keeps_committed_settings_and_reports_the_error() {
    let (provider, _) = CountingProvider::new(vec![(
        "doc.html",
        "<html><body><p>Transactional settings</p></body></html>",
    )]);
    let provider: Arc<dyn crate::ResourceProvider> = provider;
    let mut shaper = TestShaper::default();
    let loaded = load::load_document(provider.clone(), "doc.html", 16.0, 600.0, &mut shaper);
    let host = Rc::new(TestHost::default());
    let mut core = RendererCore::new(
        host.clone(),
        loaded,
        provider,
        vec!["doc.html".to_owned()],
        0,
        RendererInitialConfig::default(),
    );
    let committed_size = core.root_font_size;
    let committed_input_size = core.page.inputs.style_environment.root_font_size;
    host.events.borrow_mut().clear();

    shaper.reject_glyphs = true;
    core.set_root_font_size(&mut shaper, 24.0);

    assert_eq!(core.root_font_size, committed_size);
    assert_eq!(
        core.page.inputs.style_environment.root_font_size,
        committed_input_size
    );
    let events = host.events.borrow();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RendererEvent::FontSizeChanged(size) if *size == 24.0))
    );
    assert!(events.iter().any(|event| matches!(event, RendererEvent::OperationFailed { operation: RendererOperation::UpdateDocument, message } if !message.is_empty())));
}

#[test]
fn pre_frame_relayout_preserves_the_document_start_before_the_first_glyph() {
    let (provider, _) = CountingProvider::new(vec![(
        "document.html",
        "<html><body style='margin:0'><div style='height:100px;background:green'></div><p style='margin:0'>First text</p></body></html>",
    )]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        None,
        RendererInitialConfig::default(),
    );

    assert_eq!(core.viewport.composition().start_offset_y(), 0.0);
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(18.0));
    assert_eq!(
        core.viewport.composition().start_offset_y(),
        0.0,
        "relayout before the first frame must retain leading non-text content"
    );

    core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 800.0));
    core.paint_forward(&mut RecordingPainter::default());
    let document_y = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(0)
        .expect("fixture has a text line")
        .point()
        .y;
    let screen_y = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(0)
        .expect("first line is visible")
        .point
        .y;
    assert_eq!(
        screen_y, document_y,
        "the first frame must not scroll directly to its first glyph"
    );
}

#[test]
fn omitted_nav_state_stays_at_document_start_when_semantic_relayout_moves_a_later_paragraph() {
    let html = "<html><body style='margin:10px'><h1 style='margin:0;line-height:20px'>Heading</h1><div style='height:30px'></div><p style='margin:0;line-height:20px;widows:2;orphans:2'>B<br>C</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        None,
        RendererInitialConfig::default(),
    );

    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 80.0));
    core.paint_forward(&mut RecordingPainter::default());

    let heading = line_index_for_character(&core, 'H');
    let moved = line_index_for_character(&core, 'B');
    assert_eq!(
        core.viewport.composition().start_offset_y(),
        0.0,
        "a publication without saved state must retain the true document origin"
    );
    assert_eq!(core.page.view.frame.page().start_line(), Some(heading));
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(heading)
            .is_some()
    );
    assert!(
        core.page
            .view
            .frame
            .page()
            .line_positions()
            .get(moved)
            .is_none(),
        "the protected paragraph should move without becoming the restored startup anchor"
    );
    assert_eq!(
        core.page
            .view
            .document
            .render_view()
            .text()
            .line(moved)
            .expect("moved line")
            .point()
            .y,
        60.0,
        "continuous layout must not contain a pagination gap"
    );
}

#[test]
fn navigation_reuses_cached_documents_between_adjacent_spine_entries() {
    let (provider, read_count) = CountingProvider::new(vec![
        (
            "doc0.html",
            "<html><body><a href=\"doc1.html\">next</a></body></html>",
        ),
        (
            "doc1.html",
            "<html><body><a href=\"doc0.html\">prev</a></body></html>",
        ),
    ]);

    let host = Rc::new(TestHost::default());
    let uris = vec!["doc0.html".to_owned(), "doc1.html".to_owned()];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );

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
fn chapter_switches_move_sessions_with_a_single_cached_document_or_no_cache() {
    for capacity in [0, 1] {
        let (provider, reads) = CountingProvider::new(vec![
            ("one.html", "<html><body>First chapter</body></html>"),
            ("two.html", "<html><body>Second chapter</body></html>"),
        ]);
        let mut shaper = TestShaper::default();
        let mut core = RendererCore::from_provider_with_nav(
            Rc::new(TestHost::default()), &mut shaper, provider,
            vec!["one.html".into(), "two.html".into()], 0, None,
            RendererInitialConfig::default(),
        );
        core.document_cache = crate::document_cache::DocumentCache::new(capacity);
        let baseline = reads.load(Ordering::Relaxed);

        for (turn, index) in [1, 0, 1, 0].into_iter().enumerate() {
            core.load_document_at(&mut shaper, index, crate::DocAnchor::Start);
            // Reflow each restored session and paint from its current document.
            core.prepare_frame(&mut shaper, Size::new(400.0 + turn as f64 * 50.0, 600.0));
            core.paint_forward(&mut RecordingPainter::default());
            assert_eq!(core.nav.location().document(), index);
            assert!(laid_out_text(&core.page.view.document).contains(if index == 0 { "First chapter" } else { "Second chapter" }));
            assert!(core.page.view.frame.page().line_positions().iter().next().is_some());
            assert_eq!(reads.load(Ordering::Relaxed), baseline + if capacity == 0 { turn + 1 } else { 1 },
                "taking the incoming chapter must precede caching the outgoing chapter");
        }
    }
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
    let uris = vec![
        "doc0.html".to_owned(),
        "doc1.html".to_owned(),
        "doc2.html".to_owned(),
        "doc3.html".to_owned(),
        "doc4.html".to_owned(),
    ];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );

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

    assert!(
        core.nav.history.is_at_newest(),
        "walking back to the first document should leave the history cursor at its newest entry"
    );
    // Taking the incoming item before caching the outgoing one avoids
    // evicting the chapter that is about to become visible.
    assert_eq!(read_count.load(Ordering::Relaxed), indexed_baseline + 5);
}

#[test]
fn publication_search_stops_between_spine_documents_when_cancelled() {
    let (provider, read_count) = CountingProvider::new(vec![
        ("doc0.html", "<p>needle first</p>"),
        ("doc1.html", "<p>needle second</p>"),
        ("doc2.html", "<p>needle third</p>"),
    ]);
    let documents = vec![
        "doc0.html".to_owned(),
        "doc1.html".to_owned(),
        "doc2.html".to_owned(),
    ];
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

    assert_eq!(
        crate::publication_text_lengths(provider.as_ref(), &documents),
        vec![6]
    );
    assert_eq!(
        crate::search_publication(
            provider.as_ref(),
            &documents,
            "two",
            crate::SearchOptions::default()
        )
        .len(),
        1
    );
    assert!(
        crate::search_publication(
            provider.as_ref(),
            &documents,
            "hidden",
            crate::SearchOptions::default()
        )
        .is_empty()
    );
}

#[test]
fn publication_search_joins_to_layout_by_source_position_not_ordinal() {
    let source = "<html><body><p id='a'>needle A</p><p style='display:none'>needle hidden</p><p id='b'>needle B</p><p id='c'>needle C</p></body></html>";
    let (provider, reads) = CountingProvider::new(vec![("doc.html", source)]);
    let documents = vec!["doc.html".to_owned()];
    let options = crate::SearchOptions { scope: crate::SearchScope::WholeBook, ..Default::default() };
    let results = crate::search_publication(provider.as_ref(), &documents, "needle", options);
    assert_eq!(results.len(), 4, "parse-level search sees text hidden later by style");

    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, documents, 0, None, RendererInitialConfig::default());

    let expected = core.doc().view().addressing().glyph_range_for_anchor("b").expect("the visible target has glyphs").start;
    for command in
        [crate::RendererCommand::SetSearchResults { query: "needle".into(), options, results: vec![results[2].clone()] }, crate::RendererCommand::AppendSearchResults { query: "needle".into(), options, results: vec![results[2].clone()] }]
    {
        core.highlight.restart(String::new(), options);
        core.viewport.begin_forward_at(5.0);
        let before = reads.load(Ordering::Relaxed);
        core.apply(&mut shaper, command);
        assert_eq!(core.highlight.current_local_match_start(), Some(expected));
        assert_eq!(core.viewport.composition().start_offset_y(), 5.0, "arriving results must not move the reader");
        assert_eq!(reads.load(Ordering::Relaxed), before, "highlighting must use the loaded document");
    }
}

#[test]
fn footnote_preview_requires_note_semantics_and_resolves_cross_spine_targets() {
    let (provider, read_count) = CountingProvider::new(vec![
        (
            "chapter.html",
            "<p><a epub:type=\"noteref\" href=\"notes.html#n1\">1</a><a href=\"notes.html#ordinary\">more</a></p>",
        ),
        (
            "notes.html",
            "<aside id=\"n1\" epub:type=\"footnote\"><p><em>Structured</em> note text</p><ul><li>First source</li></ul></aside><section id=\"ordinary\">Ordinary section</section>",
        ),
    ]);
    let host = Rc::new(TestHost::default());
    let uris = vec!["chapter.html".to_owned(), "notes.html".to_owned()];
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );
    let initial_reads = read_count.load(Ordering::Relaxed);

    assert!(
        core.footnote_preview(&mut shaper, "notes.html#n1")
            .is_some()
    );
    let rendered_note = laid_out_text(&core.note.as_ref().expect("open note").document);
    assert!(rendered_note.contains("Structured note text"));
    assert!(rendered_note.contains("First source"));
    let indexed_reads = read_count.load(Ordering::Relaxed);
    assert_eq!(
        indexed_reads,
        initial_reads + 1,
        "the target spine document should be indexed once on first use"
    );
    assert_eq!(
        core.footnote_preview(&mut shaper, "notes.html#ordinary"),
        None
    );
    assert_eq!(
        read_count.load(Ordering::Relaxed),
        indexed_reads,
        "subsequent targets in the same spine document must reuse its semantic index"
    );
}

#[test]
fn note_reference_classification_is_per_link_even_when_hrefs_match() {
    let html = "<html><body><p><a href='#target'>R</a><a role='doc-noteref' href='#target'>N</a></p><div id='target'><p>Popup body</p></div></body></html>";
    let (provider, read_count) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host.clone(),
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );
    let regular = glyph_index_for_character(&core, 'R');
    let noteref = glyph_index_for_character(&core, 'N');
    let addressing = core.page.view.document.render_view().addressing();

    assert!(!addressing.is_note_reference(regular));
    assert!(addressing.is_note_reference(noteref));
    let reads_after_preparation = read_count.load(Ordering::Relaxed);

    assert!(activate_glyph(&mut core, &mut shaper, regular));
    assert!(
        !host
            .events
            .borrow()
            .iter()
            .any(|event| matches!(event, RendererEvent::FootnoteOpened(_))),
        "an ordinary link sharing the href must navigate normally"
    );
    host.events.borrow_mut().clear();

    assert!(activate_glyph(&mut core, &mut shaper, noteref));
    assert!(host.events.borrow().iter().any(
        |event| matches!(event, RendererEvent::FootnoteOpened(preview) if preview.href == "#target")
    ));
    assert_eq!(
        read_count.load(Ordering::Relaxed),
        reads_after_preparation,
        "same-document previews must use the index built during preparation"
    );
}

#[test]
fn reading_fragments_filter_structure_and_preserve_identity_on_relayout() {
    let source = "<html><body><nav><p>Contents</p></nav><h1>Chapter</h1><p>Hello <em>bright</em> world<a role='doc-noteref' href='#note'>1</a>.</p><table><tr><td>Excluded cell</td></tr></table><aside id='note' role='doc-footnote'>Excluded note</aside><div hidden>Hidden</div><p>Read <a href='https://example.com'>this link</a>.</p></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", source)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host, &mut shaper, provider, vec!["document.html".to_owned()], 0, Some("0:0"), RendererInitialConfig::default());
    let fragments = core.page.view.document.render_view().addressing().reading_fragments(0);
    assert_eq!(fragments.iter().map(|fragment| fragment.text.as_str()).collect::<Vec<_>>(), ["Chapter", "Hello bright world.", "Read this link."]);
    let target = fragments[1].target.clone();
    let old_ranges = target.glyph_ranges.clone();
    assert!(!old_ranges.is_empty());
    core.apply(&mut shaper, crate::RendererCommand::SetReadingTarget(Some(target.clone())));
    core.apply(&mut shaper, crate::RendererCommand::SetColumnWidth(300.0));
    let fragments_after = core.page.view.document.render_view().addressing().reading_fragments(0);
    assert_eq!(fragments_after[1].target, target, "array indices survive relayout");
    assert_eq!(fragments_after[1].text, "Hello bright world.");
    assert_eq!(core.media_overlay.reading_target(), Some(&target));
    let clipped_start = old_ranges[1].start;
    let clipped = core.page.view.document.render_view().addressing().reading_fragments(clipped_start);
    assert!(!clipped.iter().any(|fragment| fragment.text == "Chapter"));
    assert!(clipped[0].text.starts_with("bright"));
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(24.0));
    let reshaped = core.page.view.document.render_view().addressing().reading_fragments(0);
    assert_eq!(reshaped[1].target, target, "element and glyph indices survive font changes");
    assert_eq!(reshaped[1].text, "Hello bright world.");
    assert_eq!(core.media_overlay.reading_target(), Some(&target));
    let reference = glyph_index_for_character(&core, '1');
    assert!(!target.glyph_ranges.iter().any(|range| range.contains(&reference)), "excluded references must not be highlighted");
    core.apply(&mut shaper, crate::RendererCommand::SetReadingTarget(None));
    assert!(core.media_overlay.reading_target().is_none());
    let mut session = crate::RendererSession::from_core(core, shaper);
    let (_, fragments) = session.reading_fragments().unwrap();
    let revision = session.navigation_revision();
    session.follow_reading_target(fragments.last().unwrap().target.clone());
    assert_eq!(session.navigation_revision(), revision, "following narration is not a manual page change");
    session.next_page();
    assert_ne!(session.navigation_revision(), revision);
}

#[test]
fn narration_retains_earlier_elements_but_starts_on_the_visible_page() {
    let prose = (0..160).map(|index| format!("word{index}")).collect::<Vec<_>>().join(" ");
    let source = format!("<html><body style='margin:17px'><p>Earlier element</p><p>{prose}</p><p>Later element</p></body></html>");
    let (core, shaper) = pagination_core(&source, Size::new(240.0, 180.0), 1);
    let mut session = crate::RendererSession::from_core(core, shaper);
    let (_, initial) = session.reading_fragments().unwrap();
    session.follow_reading_target(initial[2].target.clone());
    session.prepare_frame(Size::new(240.0, 180.0));
    let (_, onward) = session.reading_fragments().unwrap();
    let (_, start, all) = session.document_reading_fragments().unwrap();
    assert!(start > 0, "backwards skips need the earlier elements");
    assert_eq!(&all[start..], onward.as_slice(), "starting TTS must keep the visible-page boundary");
    assert!(all[0].text.contains("Earlier element"));
    assert!(session.reading_target_cfi(&all[start].target).is_some());
}

#[test]
fn ruler_detects_narration_page_changes_without_manual_navigation() {
    let paragraphs = (0..40).map(|index| format!("<p>Line {index}</p>")).collect::<String>();
    let source = format!("<html><body style='margin:0'><style>p {{margin:0;line-height:28px}}</style>{paragraphs}</body></html>");
    let viewport = Size::new(400.0, 140.0);
    let (core, shaper) = pagination_core(&source, viewport, 1);
    let mut session = crate::RendererSession::from_core(core, shaper);
    let (_, _, _, targets) = session.reading_items().unwrap();
    let revision = session.navigation_revision();
    assert!(!session.follow_reading_target(targets[0].clone()), "a visible target must leave the ruler alone");
    assert!(session.follow_reading_target(targets[20].clone()), "an offscreen target must reset the ruler");
    assert_eq!(session.navigation_revision(), revision);
    session.prepare_frame(viewport);
    assert!(!session.follow_reading_target(targets[20].clone()));
    assert_eq!(session.navigation_revision(), revision);
}

#[test]
fn ruler_skips_overflow_clipped_lines_and_keeps_partial_lines() {
    let fixtures = [
        ("<div style='height:28px;overflow:hidden'><p>First</p><p>Hidden</p><p>Hidden</p></div><p>Last</p>", vec![28.0, 28.0]),
        ("<div style='height:42px;overflow:hidden'><p>First</p><p>Partial</p><p>Hidden</p></div><p>Last</p>", vec![28.0, 14.0, 28.0]),
        ("<div style='height:0;overflow:hidden'><p>Hidden</p></div><p>Last</p>", vec![28.0]),
        ("<p style='width:40px;overflow:hidden;white-space:nowrap;text-indent:80px'>Hidden</p><p>Last</p>", vec![28.0]),
        ("<div style='height:28px;overflow:visible'><p>First</p><p>Visible</p><p>Visible</p></div><p>Last</p>", vec![28.0, 28.0, 28.0, 28.0]),
    ];
    for (markup, heights) in fixtures {
        let source = format!("<html><body style='margin:0'><style>p {{margin:0;line-height:28px}}</style>{markup}</body></html>");
        let (core, shaper) = pagination_core(&source, Size::new(600.0, 400.0), 1);
        let session = crate::RendererSession::from_core(core, shaper);
        let lines = session.visible_reading_lines();
        assert_eq!(lines.len(), heights.len(), "{markup}: {lines:?}");
        for (line, height) in lines.iter().zip(heights) {
            assert!((line.bounds.height() - height).abs() < 0.01, "{markup}: {line:?}");
        }
    }
}

#[test]
fn reading_fragments_continue_a_single_element_across_pages() {
    let prose = (0..160).map(|index| format!("word{index}")).collect::<Vec<_>>().join(" ");
    let source = format!("<html><body style='margin:0'><p>{prose}</p></body></html>");
    let (core, shaper) = pagination_core(&source, Size::new(240.0, 180.0), 1);
    let mut session = crate::RendererSession::from_core(core, shaper);
    let (_, fragments) = session.reading_fragments().unwrap();
    assert!(fragments.len() > 1, "a long paragraph needs multiple page fragments");
    assert_eq!(fragments.iter().map(|fragment| fragment.text.as_str()).collect::<Vec<_>>().join(" "), prose, "page boundaries must neither lose nor repeat words");
    let element = fragments[0].target.element;
    for pair in fragments.windows(2) {
        assert_eq!(pair[0].target.element, element);
        assert_eq!(pair[1].target.element, element);
        assert!(pair[0].target.glyph_ranges.last().unwrap().end <= pair[1].target.glyph_ranges[0].start);
    }
    let revision = session.navigation_revision();
    let first_position = session.current_cfi();
    session.follow_reading_target(fragments[1].target.clone());
    assert_ne!(session.current_cfi(), first_position, "continuation must turn the displayed page");
    assert_eq!(session.navigation_revision(), revision);
    session.apply(crate::RendererCommand::SetFontSize(24.0));
    let text = session.document().render_view().text();
    let still_readable: String = fragments[1].target.glyph_ranges.iter().flat_map(|range| range.clone())
        .filter_map(|index| text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch()))
        .collect();
    assert_eq!(still_readable.split_whitespace().collect::<Vec<_>>().join(" "), fragments[1].text, "stored glyph indices still address the same passage after font changes");
}

#[test]
fn semantic_footnotes_are_popup_only_and_absent_from_paginated_text() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);
    let text = core.page.view.document.render_view().text();
    let noteref = glyph_index_for_character(&core, '1');
    assert!(
        core.page
            .view
            .document
            .render_view()
            .addressing()
            .is_note_reference(noteref)
    );
    let visible_source = (0..text.glyph_count())
        .filter_map(|index| {
            text.glyph_at(index)
                .and_then(|glyph| text.glyph_metric(glyph))
                .map(|metric| metric.ch())
        })
        .collect::<String>();

    assert!(visible_source.contains("Reading"));
    assert!(visible_source.contains("Continues"));
    assert!(
        !visible_source.contains("zebra"),
        "the footnote body must not consume paginated layout space"
    );
    assert!(core.footnote_preview(&mut shaper, "#note").is_some());
    assert!(
        laid_out_text(&core.note.as_ref().expect("open note").document)
            .contains("Popup-only zebra")
    );
}

#[test]
fn clicked_footnote_emits_a_viewport_overlay_anchor() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup</p></aside></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host.clone(),
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());
    let noteref = glyph_index_for_character(&core, '1');
    let line_idx = core
        .doc()
        .text().line_index_for_glyph(noteref)
        .expect("note reference line");
    let screen = core
        .page
        .view
        .frame
        .page()
        .line_positions()
        .get(line_idx)
        .expect("painted note reference line");
    let line = core
        .page
        .view
        .document
        .render_view()
        .text()
        .line(line_idx)
        .expect("laid out note reference line");
    let click = Point::new(
        screen.point.x + core.text_geometry().glyph_x_in_line(&line, noteref) + 0.1,
        screen.point.y + line.height() / 2.0,
    );
    assert!(
        click.x < 100.0 && click.y < 100.0,
        "fixture click is in the top-left viewport quadrant"
    );

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
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        config,
    );
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let text = core.page.view.document.render_view().text();
    let visible_source = (0..text.glyph_count())
        .filter_map(|index| {
            text.glyph_at(index)
                .and_then(|glyph| text.glyph_metric(glyph))
                .map(|metric| metric.ch())
        })
        .collect::<String>();
    assert!(
        visible_source.contains("zebra"),
        "an as-authored note occupies the reading flow rather than being held back for a popup"
    );

    // The engine's note semantics are untouched by the display mode: the
    // target is still a note, which is what a later mode switch relies on.
    assert!(
        core.page
            .view
            .document
            .render_view()
            .addressing()
            .is_note_target("note")
    );
}

#[test]
fn switching_note_display_relays_out_the_document() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside><p>Continues</p></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);

    let flow_text = |core: &RendererCore| {
        let text = core.page.view.document.render_view().text();
        (0..text.glyph_count())
            .filter_map(|index| {
                text.glyph_at(index)
                    .and_then(|glyph| text.glyph_metric(glyph))
                    .map(|metric| metric.ch())
            })
            .collect::<String>()
    };
    assert!(
        !flow_text(&core).contains("zebra"),
        "the note starts held back for a popup"
    );

    core.apply(
        &mut shaper,
        crate::RendererCommand::SetNoteDisplay(crate::NoteDisplay::AsAuthored),
    );

    assert!(
        flow_text(&core).contains("zebra"),
        "switching to as-authored must rebuild the document with the note in flow"
    );
    assert_eq!(
        core.footnote_preview(&mut shaper, "#note"),
        None,
        "and the reference stops opening a popup"
    );

    core.apply(
        &mut shaper,
        crate::RendererCommand::SetNoteDisplay(crate::NoteDisplay::Popup),
    );
    assert!(
        !flow_text(&core).contains("zebra"),
        "switching back holds the note out of the flow again"
    );
}

#[test]
fn a_previewed_note_paints_its_own_content() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Popup-only zebra</p></aside></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(200.0, 200.0), 1);
    core.footnote_preview(&mut shaper, "#note")
        .expect("the note must preview");

    let scene = core
        .note_scene()
        .expect("a previewed note must form a scene");
    let mut painter = RecordingPainter::default();
    scene.paint(&mut painter);

    // The note is drawn from its own layout through the engine's fragment
    // renderer, rather than through the page's viewport, which holds no
    // glyphs for it at all.
    assert!(
        !painter.glyphs.is_empty(),
        "a previewed note must paint its own glyphs"
    );
    assert!(
        scene.content_height() > 0.0,
        "a note's scene must have height"
    );
}

/// A reader over a spine, laid out in columns of `column_width`. The
/// spine is the HTML; anything else a fixture lists is a resource the
/// documents refer to.
fn spine_core(
    documents: Vec<(&str, &str)>,
    viewport: Size,
    column_width: f64,
    max_columns: u8,
) -> (RendererCore, TestShaper) {
    let uris = documents
        .iter()
        .filter(|(uri, _)| uri.ends_with(".html"))
        .map(|(uri, _)| (*uri).to_owned())
        .collect::<Vec<_>>();
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
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        config,
    );
    core.prepare_frame(&mut shaper, viewport);
    (core, shaper)
}

#[test]
fn page_turn_leaves_a_chapter_after_returning_to_its_last_page() {
    let viewport = Size::new(200.0, 40.0);
    let first =
        "<html><body style='margin:0;line-height:20px'>A<br>B<br>C<br>D<br>E<br>F</body></html>";
    let second = "<html><body style='margin:0'>Next chapter</body></html>";
    let (mut core, mut shaper) = spine_core(
        vec![("one.html", first), ("two.html", second)],
        viewport,
        200.0,
        1,
    );
    core.next_document(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    core.prev_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.viewport.composition().start_offset_y() > 0.0);
    assert_eq!(
        core.page.view.frame.page().end_line(),
        Some(core.page.view.document.render_view().text().line_count() - 1),
        "backward navigation must land on the chapter's last line"
    );

    core.next_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);

    assert_eq!(
        core.nav.location().document(),
        1,
        "Right must reopen the next chapter without an extra blank page"
    );
    assert_eq!(core.viewport.composition().start_offset_y(), 0.0);
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
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        uris,
        0,
        Some("0:0"),
        config,
    );
    core.prepare_frame(&mut shaper, Size::new(420.0, 300.0));

    core.next_page(&mut shaper);

    assert_eq!(core.nav.location().document(), 1);
    assert_eq!(
        core.viewport.composition().start_offset_y(),
        0.0,
        "and it opens at the start, none of it having been shown"
    );
}

fn source_position_reader(source: &str) -> (RendererCore, TestShaper, Rc<TestHost>) {
    let (provider, _) = CountingProvider::new(vec![("chapter.html", source)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let config = RendererInitialConfig {
        font_size: 16.0,
        column_width: 340.0,
        max_column_count: Some(1),
        image_sizing_policy: html::pipeline::ImageSizingPolicy::WebCompatible,
        text_composition_policy: html::pipeline::TextCompositionPolicy::WebCompatible,
        ..RendererInitialConfig::default()
    };
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider, vec!["chapter.html".into()], 0, None, config);
    core.apply(&mut shaper, crate::RendererCommand::SetDocumentTextLengths(vec![source.chars().count() as u64]));
    (core, shaper, host)
}

fn reading_positions(host: &TestHost) -> Vec<crate::ReadingPosition> {
    host.events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            RendererEvent::ReadingPositionChanged(position) => Some(position.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn reading_position_snapshot_waits_for_pagination_and_deduplicates_repaints() {
    let source = format!("<html><head></head><body><p>{}</p></body></html>", "Page text. ".repeat(800));
    let (mut core, mut shaper, host) = source_position_reader(&source);
    assert!(reading_positions(&host).is_empty(), "construction has no settled page to publish");
    let viewport = Size::new(340.0, 600.0);
    core.prepare_frame(&mut shaper, viewport);
    let first = reading_positions(&host).last().unwrap().clone();
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(reading_positions(&host).len(), 1, "a repaint is not a new reading location");
    core.next_page(&mut shaper);
    assert_eq!(reading_positions(&host).len(), 1, "navigation must not publish the previous frame's location");
    core.prepare_frame(&mut shaper, viewport);
    let second = reading_positions(&host).last().unwrap().clone();
    assert_ne!(second.source, first.source);
    assert!(second.fraction > first.fraction);
    assert!(second.document_fraction > first.document_fraction);
    assert_eq!(second.source.document, 0);
    assert_eq!(second.document_count, 1);
    let cfi = crate::cfi::source_position_cfi(&second.source);
    assert_eq!(crate::cfi::parse_source_position(&cfi), Some(second.source));
}

#[test]
fn reading_position_moves_even_when_reported_progress_does_not() {
    let source = format!("<html><body><p>{}</p></body></html>", "Page text. ".repeat(800));
    let (mut core, mut shaper, host) = source_position_reader(&source);
    core.apply(&mut shaper, crate::RendererCommand::SetDocumentTextLengths(vec![1]));
    let viewport = Size::new(340.0, 600.0);
    core.prepare_frame(&mut shaper, viewport);
    let first = reading_positions(&host).last().unwrap().clone();
    core.next_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    let second = reading_positions(&host).last().unwrap().clone();
    assert_eq!(first.fraction, second.fraction);
    assert_ne!(first.source, second.source, "chapter tracking must not depend on a progress change");
}

#[test]
fn reading_position_waits_for_background_document_navigation() {
    let (mut core, mut shaper) = spine_core(vec![("one.html", "<html><body><p>First</p></body></html>"), ("two.html", "<html><body><img src='picture.svg' width='300' height='500'/></body></html>")], Size::new(340.0, 600.0), 340.0, 1);
    let jobs = Arc::new(DeferredFileJobs::default());
    let host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
    core.host = host.clone();
    core.next_page(&mut shaper);
    assert!(reading_positions(&host).is_empty());
    jobs.run();
    core.finish_pending_navigation(&mut shaper);
    assert!(reading_positions(&host).is_empty(), "loading does not publish unpaginated coordinates");
    core.prepare_frame(&mut shaper, Size::new(340.0, 600.0));
    let position = reading_positions(&host).last().unwrap().clone();
    assert_eq!(position.source.document, 1);
    assert_eq!(position.document_count, 2);
    assert_eq!(position.source.path, vec![4, 2]);
    assert_eq!(position.source.utf16_offset, None);
    core.prev_page(&mut shaper);
    core.prepare_frame(&mut shaper, Size::new(340.0, 600.0));
    assert_eq!(reading_positions(&host).last().unwrap().source.document, 0, "cached documents resolve their own source addresses");
}

#[test]
fn reading_position_source_crosses_empty_chapter_markers() {
    let filler = "<p>Some text in the current section. More text follows here.</p>".repeat(12);
    let source = format!("<html><head></head><body style='margin:0'><h1 id='first'>First</h1>{filler}<div id='second'></div><h2>Second</h2>{filler}</body></html>");
    let parsed = html::document::parse_document(&source, html::document::MarkupSyntax::Html).unwrap().build_dom_checked().unwrap();
    let marker = parsed.node_ids().find(|node| parsed.get_dom_id(*node) == Some("second")).unwrap();
    let boundary = parsed.get_dom_path_to_node(marker).into_iter().skip(1).map(|node| parsed.get_element_step(node).unwrap()).collect::<Vec<_>>();
    let (mut core, mut shaper, host) = source_position_reader(&source);
    core.set_toc_anchor_strings_by_doc(vec![vec!["first".into(), "second".into()]]);
    let viewport = Size::new(340.0, 600.0);
    core.prepare_frame(&mut shaper, viewport);
    assert!(reading_positions(&host).last().unwrap().source.path < boundary);
    for _ in 0..5 {
        core.next_page(&mut shaper);
        core.prepare_frame(&mut shaper, viewport);
    }
    assert!(reading_positions(&host).last().unwrap().source.path > boundary, "source addresses cross the empty marker even though it owns no glyph");
    for _ in 0..5 {
        core.prev_page(&mut shaper);
        core.prepare_frame(&mut shaper, viewport);
    }
    assert!(reading_positions(&host).last().unwrap().source.path < boundary, "backward navigation uses the same ordering");
}

#[test]
fn reading_position_image_pages_restore_and_survive_reflow() {
    let source = "<html><head></head><body style='margin:0'><img src='one.svg' width='300' height='500'/><img src='two.svg' width='300' height='500'/></body></html>";
    let (mut core, mut shaper, host) = source_position_reader(source);
    let second_path = vec![4, 4];
    let viewport = Size::new(340.0, 600.0);
    core.prepare_frame(&mut shaper, viewport);
    let first = reading_positions(&host).last().unwrap().clone();
    assert!(first.source.utf16_offset.is_none(), "an image has an element address rather than a borrowed text address");
    for _ in 0..5 {
        if reading_positions(&host).last().unwrap().source.path == second_path {
            break;
        }
        core.next_page(&mut shaper);
        core.prepare_frame(&mut shaper, viewport);
    }
    let second = reading_positions(&host).last().unwrap().clone();
    assert_eq!(second.source.path, second_path);
    assert!(second.fraction > first.fraction);
    let saved = crate::cfi::source_position_cfi(&second.source);
    core.prev_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    assert!(core.restore_position_from_cfi(&saved));
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(reading_positions(&host).last().unwrap().source, second.source);
    core.apply(&mut shaper, crate::RendererCommand::SetFontSize(20.0));
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(reading_positions(&host).last().unwrap().source, second.source, "reflow must restore the element source position");
    let (mut reopened, mut reopened_shaper, reopened_host) = source_position_reader(source);
    assert!(reopened.restore_position_from_cfi(&saved));
    reopened.prepare_frame(&mut reopened_shaper, viewport);
    assert_eq!(reading_positions(&reopened_host).last().unwrap().source, second.source);
}

#[test]
fn a_previous_line_at_a_document_start_opens_the_previous_document() {
    let document = "<html><body style='margin:0'><p>One line</p></body></html>";
    let (mut core, mut shaper) = spine_core(
        vec![("one.html", document), ("two.html", document)],
        Size::new(200.0, 300.0),
        200.0,
        1,
    );
    core.next_document(&mut shaper);
    assert_eq!(core.nav.location().document(), 1);

    core.prev_line(&mut shaper);

    assert_eq!(
        core.nav.location().document(),
        0,
        "backward line navigation must cross the spine boundary"
    );
}

#[test]
fn text_can_be_selected_inside_an_open_note() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Selectable note body</p></aside></body></html>";
    let (mut core, mut shaper) = pagination_core(html, Size::new(300.0, 300.0), 1);
    core.footnote_preview(&mut shaper, "#note")
        .expect("the note must preview");

    // A note is a document view, so selection is the page's code applied
    // to it rather than a second implementation.
    let note = core.note.as_ref().expect("an open note is held as a view");
    let last_line = note.doc().text().line_count().saturating_sub(1);
    let bottom = note
        .doc()
        .text()
        .line(last_line)
        .map(|line| line.point().y + line.height())
        .unwrap_or(10.0);

    assert!(
        core.note
            .as_mut()
            .expect("open note")
            .begin_selection_at(Point::new(1.0, 1.0), crate::SelectionMode::Plain),
        "a press inside the note starts a selection"
    );
    assert!(
        core.note
            .as_mut()
            .expect("open note")
            .update_selection_at(Point::new(280.0, bottom - 1.0), crate::SelectionMode::Plain),
        "dragging extends it"
    );
    core.note.as_mut().expect("open note").finish_selection();

    let selected = core
        .note
        .as_ref()
        .expect("open note")
        .selection_text()
        .expect("the note yields selected text");
    assert!(
        selected.contains("Selectable"),
        "the note's own text is selected, not the page's: {selected:?}"
    );

    core.close_note();
    assert!(
        core.note.as_ref().is_none(),
        "closing the note drops what was selected in it"
    );
}

#[test]
fn note_selection_uses_the_shared_clipboard_operation() {
    let html = "<html><body style='margin:0'><p>Reading<a epub:type='noteref' href='#note'>1</a></p><aside id='note' epub:type='footnote'><p>Clipboard note body</p></aside></body></html>";
    let (provider, _) = CountingProvider::new(vec![("document.html", html)]);
    let host = Rc::new(TestHost::default());
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        host.clone(),
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );
    core.configure_layout_for_viewport(&mut shaper, Size::new(300.0, 300.0));
    core.footnote_preview(&mut shaper, "#note")
        .expect("the note must preview");
    let mut session = crate::RendererSession::from_core(core, shaper);
    let bottom = session
        .note_scene()
        .expect("open note scene")
        .content_height();

    assert!(session.note_pointer_down(Point::new(1.0, 1.0), crate::SelectionMode::Plain));
    assert!(
        session.note_pointer_move(Point::new(280.0, bottom - 1.0), crate::SelectionMode::Plain)
    );
    assert!(session.note_pointer_up());
    assert!(session.copy_selection());
    assert!(
        host.clipboard_text
            .borrow()
            .as_deref()
            .is_some_and(|text| text.contains("Clipboard note body"))
    );
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
        let config = RendererInitialConfig {
            font_size: 16.0,
            column_width: 600.0,
            max_column_count: Some(1),
            ..RendererInitialConfig::default()
        };
        let mut core = RendererCore::from_provider_with_nav(
            host,
            &mut shaper,
            provider,
            vec!["document.html".to_owned()],
            0,
            Some("0:0"),
            config,
        );
        core.configure_layout_for_viewport(&mut shaper, Size::new(600.0, 400.0));
        core.footnote_preview(&mut shaper, "#note")
            .expect("the note must preview");
        core.note
            .as_ref()
            .expect("open note")
            .document
            .render_view()
            .text()
            .line_count()
    };

    let wide = lines_at(None);
    let narrow = lines_at(Some(120.0));

    assert!(
        narrow > wide,
        "a narrower popup must wrap the note into more lines ({narrow} at 120px vs {wide} at the column width)"
    );
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
    let mut core = RendererCore::from_provider_with_nav(
        host.clone(),
        &mut shaper,
        provider,
        vec!["document.html".to_owned()],
        0,
        Some("0:0"),
        config,
    );
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let noteref = glyph_index_for_character(&core, '1');
    assert!(
        core.page
            .view
            .document
            .render_view()
            .addressing()
            .is_note_reference(noteref),
        "the reference keeps its note semantics; only the presentation differs"
    );

    assert!(activate_glyph(&mut core, &mut shaper, noteref));

    assert!(
        !host
            .events
            .borrow()
            .iter()
            .any(|event| matches!(event, RendererEvent::FootnoteOpened(_))),
        "a note that reads in place must not also be shown as a popup"
    );
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
    let mut core = RendererCore::from_provider_with_nav(
        host,
        &mut shaper,
        provider,
        vec!["document.xhtml".to_owned()],
        0,
        Some("0:0"),
        RendererInitialConfig::default(),
    );
    core.configure_layout_for_viewport(&mut shaper, Size::new(200.0, 200.0));
    core.paint_forward(&mut RecordingPainter::default());

    let text = core.page.view.document.render_view().text();
    let noteref = glyph_index_for_character(&core, '1');
    assert!(
        core.page
            .view
            .document
            .render_view()
            .addressing()
            .is_note_reference(noteref),
        "namespaced EPUB noteref semantics must survive preparation"
    );
    let visible_source = (0..text.glyph_count())
        .filter_map(|index| {
            text.glyph_at(index)
                .and_then(|glyph| text.glyph_metric(glyph))
                .map(|metric| metric.ch())
        })
        .collect::<String>();
    assert!(visible_source.contains("Reading"));
    assert!(visible_source.contains("Continues"));
    assert!(
        !visible_source.contains("magenta"),
        "the namespaced XHTML footnote body must not consume paginated layout space"
    );
    assert!(core.footnote_preview(&mut shaper, "#note").is_some());
    assert!(
        laid_out_text(&core.note.as_ref().expect("open note").document)
            .contains("Popup-only magenta")
    );
}

#[derive(Default)]
struct DeferredFileJobs(std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>>);
impl html::resources::BlockingJobSpawner for DeferredFileJobs {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) { self.0.lock().unwrap().push(job); }
}
impl DeferredFileJobs {
    fn run(&self) {
        let jobs = std::mem::take(&mut *self.0.lock().unwrap());
        std::thread::spawn(move || { for job in jobs { job(); } }).join().unwrap();
    }
}

#[test]
fn reading_waits_for_the_next_document_without_replaying_the_old_one() {
    let (mut core, shaper) = spine_core(vec![
        ("one.html", "<html><body><p>First chapter</p></body></html>"),
        ("two.html", "<html><body><p>Second chapter</p></body></html>"),
    ], Size::new(600.0, 800.0), 600.0, 1);
    let jobs = Arc::new(DeferredFileJobs::default());
    core.host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
    let mut session = crate::RendererSession::from_core(core, shaper);
    let revision = session.navigation_revision();
    session.advance_reading_document();
    assert!(session.reading_fragments().is_none(), "old chapter fragments must be unavailable while the next chapter loads");
    session.update_resources();
    assert!(session.reading_fragments().is_none(), "a slow file read must keep narration waiting");
    jobs.run();
    session.update_resources();
    let (doc, fragments) = session.reading_fragments().unwrap();
    assert_eq!(doc, 1);
    assert_eq!(fragments[0].text, "Second chapter");
    assert_eq!(session.navigation_revision(), revision, "automatic chapter advancement is not manual navigation");
}

#[test]
fn delayed_link_navigation_is_manual_but_a_footnote_preview_is_not() {
    for note in [false, true] {
        let source = if note {
            "<html><body><p><a role='doc-noteref' href='two.html#note'>Jump</a></p></body></html>"
        } else {
            "<html><body><p><a href='two.html#target'>Jump</a></p></body></html>"
        };
        let (mut core, shaper) = spine_core(vec![
            ("one.html", source),
            ("two.html", "<html><body><p id='target'>Second chapter</p><aside id='note' role='doc-footnote'>A note</aside></body></html>"),
        ], Size::new(600.0, 800.0), 600.0, 1);
        let jobs = Arc::new(DeferredFileJobs::default());
        core.host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
        let glyph = glyph_index_for_character(&core, 'J');
        let line_index = core.doc().text().line_index_for_glyph(glyph).unwrap();
        let line = core.page.view.document.render_view().text().line(line_index).unwrap();
        let screen = core.page.view.frame.page().line_positions().get(line_index).unwrap();
        let click = Point::new(screen.point.x + core.text_geometry().glyph_x_in_line(&line, glyph) + 0.1, screen.point.y + line.height() / 2.0);
        let mut session = crate::RendererSession::from_core(core, shaper);
        let revision = session.navigation_revision();
        assert!(session.pointer_down(click, crate::PointerDownOptions::default()));
        assert!(session.pointer_up());
        assert!(session.reading_fragments().is_none());
        jobs.run();
        session.update_resources();
        if note {
            assert_eq!(session.navigation_revision(), revision, "opening a note does not interrupt narration");
            assert_eq!(session.reading_fragments().unwrap().0, 0);
        } else {
            assert_ne!(session.navigation_revision(), revision, "a delayed chapter link must restart narration");
            assert_eq!(session.reading_fragments().unwrap().0, 1);
        }
    }
}

#[test]
fn remote_file_navigation_waits_for_background_reads_and_preserves_failed_page() {
    let (provider, reads) = CountingProvider::new(vec![
        ("one.html", "<html><body>First</body></html>"),
        ("two.html", "<html><body><p id='target'>Second</p></body></html>"),
    ]);
    let jobs = Arc::new(DeferredFileJobs::default());
    let host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(host.clone(), &mut shaper, provider,
        vec!["one.html".into(), "two.html".into(), "missing.html".into()], 0, None, RendererInitialConfig::default());
    let before = reads.load(Ordering::Relaxed);
    assert!(core.navigate_to_href(&mut shaper, "two.html#target"));
    assert_eq!(core.nav.location().document(), 0);
    assert_eq!(reads.load(Ordering::Relaxed), before, "UI navigation must not read the file");
    jobs.run();
    core.finish_pending_navigation(&mut shaper);
    assert_eq!(core.nav.location().document(), 1);
    assert!(core.navigate_history_back(&mut shaper));
    assert_eq!(core.nav.location().document(), 0);
    core.load_document_at(&mut shaper, 2, crate::DocAnchor::Start);
    jobs.run();
    core.finish_pending_navigation(&mut shaper);
    assert_eq!(core.nav.location().document(), 0);
    assert!(host.events.borrow().iter().any(|event| matches!(event, RendererEvent::OperationFailed { .. })));
    // Failure does not poison the source or prevent another request.
    core.load_document_at(&mut shaper, 2, crate::DocAnchor::Start);
    assert!(core.pending_navigation.is_some());
}

#[test]
fn local_navigation_cancels_a_pending_chapter_turn() {
    let viewport = Size::new(200.0, 40.0);
    for (source, can_move) in [
        (
            "<html><body style='margin:0;line-height:20px'>A<br>B<br>C<br>D<br>E<br>F</body></html>",
            true,
        ),
        (
            "<html><body style='margin:0;line-height:20px'>A</body></html>",
            false,
        ),
    ] {
        for forward in [true, false] {
            for by_line in [false, true] {
                let (provider, _) =
                    CountingProvider::new(vec![("one.html", source), ("two.html", source)]);
                let mut shaper = TestShaper::default();
                let current_doc = usize::from(!forward);
                let mut core = RendererCore::from_provider_with_nav(
                    Rc::new(TestHost::default()),
                    &mut shaper,
                    provider,
                    vec!["one.html".into(), "two.html".into()],
                    current_doc,
                    None,
                    RendererInitialConfig {
                        column_width: 200.0,
                        max_column_count: Some(1),
                        ..Default::default()
                    },
                );
                core.prepare_frame(&mut shaper, viewport);
                if forward {
                    for _ in 0..10 {
                        if core.viewport.continuation().reached_end() {
                            break;
                        }
                        core.next_page(&mut shaper);
                        core.prepare_frame(&mut shaper, viewport);
                    }
                    assert!(core.viewport.continuation().reached_end());
                }
                let jobs = Arc::new(DeferredFileJobs::default());
                let host = Rc::new(TestHost {
                    jobs: Some(jobs.clone()),
                    ..Default::default()
                });
                core.host = host.clone();
                if forward {
                    core.next_page(&mut shaper);
                } else {
                    core.prev_page(&mut shaper);
                }
                assert!(
                    core.pending_navigation.is_some(),
                    "the adjacent chapter must still be loading"
                );
                let before = core.nav.current_cfi(core.nav_view());

                let repaints = host.repaint_requests.get();
                match (forward, by_line) {
                    (true, false) => core.prev_page(&mut shaper),
                    (true, true) => core.prev_line(&mut shaper),
                    (false, false) => core.next_page(&mut shaper),
                    (false, true) => core.next_line(&mut shaper),
                }
                assert!(
                    host.repaint_requests.get() > repaints,
                    "cancelling a chapter read must repaint even at a book boundary"
                );
                core.prepare_frame(&mut shaper, viewport);
                let after = core.nav.current_cfi(core.nav_view());
                if can_move {
                    assert_ne!(
                        after, before,
                        "the later navigation must move within the current chapter"
                    );
                } else {
                    assert_eq!(
                        after, before,
                        "a turn past a book boundary keeps its position"
                    );
                }

                jobs.run();
                core.finish_pending_navigation(&mut shaper);
                core.prepare_frame(&mut shaper, viewport);
                assert_eq!(
                    core.nav.location().document(),
                    current_doc,
                    "a delayed chapter request must not override local navigation (forward={forward}, by_line={by_line})"
                );
                assert_eq!(
                    core.nav.current_cfi(core.nav_view()),
                    after,
                    "the later reading position must survive completion of the stale read"
                );
                assert!(core.pending_navigation.is_none());
            }
        }
    }
}

#[test]
fn repeated_chapter_turns_reuse_the_pending_read() {
    let viewport = Size::new(200.0, 200.0);
    let source = "<html><body>Chapter</body></html>";
    for forward in [true, false] {
        let (provider, _) = CountingProvider::new(vec![("one.html", source), ("two.html", source)]);
        let mut shaper = TestShaper::default();
        let current_doc = usize::from(!forward);
        let jobs = Arc::new(DeferredFileJobs::default());
        let mut core = RendererCore::from_provider_with_nav(
            Rc::new(TestHost {
                jobs: Some(jobs.clone()),
                ..Default::default()
            }),
            &mut shaper,
            provider,
            vec!["one.html".into(), "two.html".into()],
            current_doc,
            None,
            RendererInitialConfig {
                column_width: 200.0,
                max_column_count: Some(1),
                ..Default::default()
            },
        );
        core.prepare_frame(&mut shaper, viewport);
        for _ in 0..3 {
            if forward {
                core.next_page(&mut shaper);
            } else {
                core.prev_page(&mut shaper);
            }
        }
        assert_eq!(
            jobs.0.lock().unwrap().len(),
            1,
            "holding an arrow must reuse the chapter read"
        );
        jobs.run();
        core.finish_pending_navigation(&mut shaper);
        core.prepare_frame(&mut shaper, viewport);
        assert_eq!(core.nav.location().document(), usize::from(forward));
    }
}

#[test]
fn remote_file_navigation_discards_superseded_results() {
    let (mut core, mut shaper) = spine_core(vec![
        ("one.html", "<html><body>First</body></html>"),
        ("two.html", "<html><body>Second</body></html>"),
        ("three.html", "<html><body>Third</body></html>"),
    ], Size::new(600.0, 800.0), 600.0, 1);
    let jobs = Arc::new(DeferredFileJobs::default());
    core.host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
    core.load_document_at(&mut shaper, 1, crate::DocAnchor::Start);
    core.load_document_at(&mut shaper, 2, crate::DocAnchor::Start);
    assert_eq!(core.nav.location().document(), 0);
    jobs.run();
    core.finish_pending_navigation(&mut shaper);
    assert_eq!(core.nav.location().document(), 2);
}

#[test]
fn remote_file_search_navigation_finishes_on_the_loaded_document() {
    let (mut core, mut shaper) = spine_core(vec![("one.html", "<html><body>First</body></html>"), ("two.html", "<html><body>Second needle</body></html>")], Size::new(600.0, 800.0), 600.0, 1);
    let jobs = Arc::new(DeferredFileJobs::default());
    core.host = Rc::new(TestHost { jobs: Some(jobs.clone()), ..Default::default() });
    core.set_search_results(&mut shaper, "needle".into(), crate::SearchOptions::default(), vec![crate::BookSearchMatch { doc: 1, target: crate::BookSearchTarget::LocalOccurrence(0), excerpt: "Second needle".into() }]);
    assert_eq!(core.nav.location().document(), 0);
    assert!(core.pending_navigation.is_none(), "arriving search results must not load another chapter");
    assert!(jobs.0.lock().unwrap().is_empty());
    core.next_match(&mut shaper);
    assert_eq!(core.nav.location().document(), 0);
    jobs.run();
    core.finish_pending_navigation(&mut shaper);
    assert_eq!(core.nav.location().document(), 1);
    assert!(!core.highlight.matches().is_empty());
}

#[test]
fn narration_items_are_unpaginated_and_materialize_text_on_demand() {
    let prose = (0..160).map(|index| format!("word{index}")).collect::<Vec<_>>().join(" ");
    let source = format!("<html><body style='margin:0'><p>Earlier element</p><p>{prose}</p><p>Later element</p></body></html>");
    let (core, shaper) = pagination_core(&source, Size::new(240.0, 180.0), 1);
    let mut session = crate::RendererSession::from_core(core, shaper);
    let (_, start, offset, items) = session.reading_items().unwrap();
    assert_eq!(items.len(), 3, "visual pages must not split logical items");
    assert_eq!(start, 0);
    assert!(offset <= items[0].glyph_ranges[0].start);
    let addressing = session.document().render_view().addressing();
    assert_eq!(addressing.reading_text(&items[1]), prose);
    let mut middle = items[1].clone();
    middle.glyph_ranges = vec![middle.glyph_ranges[0].start + 400..middle.glyph_ranges[0].start + 405];
    session.follow_reading_target(middle);
    session.prepare_frame(Size::new(240.0, 180.0));
    let (_, start, offset, later) = session.reading_items().unwrap();
    assert_eq!(start, 1);
    assert!(offset > items[1].glyph_ranges[0].start, "start inside the visible paragraph");
    assert_eq!(later, items, "previous-item navigation retains the original paragraph start");
}

#[test]
fn narration_chunks_carry_normalized_text_and_original_glyphs() {
    let source = "<html><body><p>First ångström.  <em>Second cafés.</em><a role='doc-noteref'>99</a> Third.</p><table><tr><td>Excluded</td></tr></table></body></html>";
    let (core, shaper) = pagination_core(source, Size::new(240.0, 180.0), 1);
    let session = crate::RendererSession::from_core(core, shaper);
    let (_, _, _, items) = session.reading_items().unwrap();
    assert_eq!(items.len(), 1);
    let addressing = session.document().render_view().addressing();
    assert_eq!(addressing.reading_text(&items[0]), "First ångström. Second cafés. Third.");
    let mut cursor = 0;
    let mut texts = Vec::new();
    loop {
        let (chunk, next) = addressing
            .reading_chunk(&items[0], cursor, 400, |text| {
                let end = text.find('.').map_or(text.len(), |index| index + 1);
                Some(0..end)
            })
            .unwrap();
        assert_eq!(chunk.target.element, items[0].element);
        assert_eq!(addressing.reading_text(&chunk.target), chunk.text);
        assert!(chunk.target.glyph_ranges[0].start >= cursor);
        texts.push(chunk.text);
        let Some(next) = next else { break };
        assert!(next > cursor);
        cursor = next;
    }
    assert_eq!(texts, ["First ångström.", "Second cafés.", "Third."]);
    assert!(addressing.reading_chunk(&items[0], u32::MAX, 400, |text| (!text.is_empty()).then_some(0..text.len())).is_none());
}

#[test]
fn narration_bounded_chunks_preserve_unicode_and_disjoint_source_ranges() {
    let source = "<html><body><p>界界界 <em>ångström</em><a role='doc-noteref'>99</a> cafés 界界界.</p></body></html>";
    let (core, shaper) = pagination_core(source, Size::new(240.0, 180.0), 1);
    let session = crate::RendererSession::from_core(core, shaper);
    let (_, _, _, items) = session.reading_items().unwrap();
    let addressing = session.document().render_view().addressing();
    let expected = addressing.reading_text(&items[0]);
    let mut actual = String::new();
    let mut cursor = 0;
    loop {
        let (chunk, next) = addressing
            .reading_chunk(&items[0], cursor, 7, |text| {
                assert!(text.chars().count() <= 8, "extraction is bounded, including lookahead");
                Some(0..text.char_indices().nth(7).map_or(text.len(), |(offset, _)| offset))
            })
            .unwrap();
        assert!(chunk.text.chars().count() <= 7);
        assert_eq!(addressing.reading_text(&chunk.target), chunk.text.trim());
        actual.push_str(&chunk.text);
        let Some(next) = next else { break };
        assert!(next > cursor);
        cursor = next;
    }
    assert_eq!(actual.chars().filter(|ch| !ch.is_whitespace()).collect::<String>(), expected.chars().filter(|ch| !ch.is_whitespace()).collect::<String>());
    let (whole, next) = addressing.reading_chunk(&items[0], 0, 400, |text| Some(0..text.len())).unwrap();
    assert_eq!(whole.text, expected);
    assert!(whole.target.glyph_ranges.len() > 1, "excluded note references leave gaps in source ranges");
    assert_eq!(next, None);
}

#[test]
fn reader_hyphenation_changes_apply_immediately_and_book_restores_publication_rules() {
    use html::style::Hyphens;
    for (authored, book_hyphenates) in [("none", false), ("auto", true)] {
        let html = format!("<html lang='en'><body style='margin:0'><p style='margin:0;text-align:justify;hyphens:{authored}'>internationalization internationalization</p></body></html>");
        let (mut core, mut shaper) = pagination_core(&html, Size::new(72.0, 300.0), 1);
        for (selection, expected) in [(Some(Hyphens::Auto), true), (Some(Hyphens::None), false), (None, book_hyphenates)] {
            core.apply(&mut shaper, crate::RendererCommand::SetReaderStyleOverrides(html::pipeline::ReaderStyleOverrides { hyphens: selection, ..Default::default() }));
            let text = core.page.view.document.render_view().text();
            assert_eq!((0..text.line_count()).any(|index| text.hyphen_for_line(index).is_some()), expected,
                "switching modes must reflow the current book: {authored}, {selection:?}");
        }
    }
}

#[test]
fn reader_word_spacing_keeps_carets_selection_and_hit_testing_aligned() {
    let (mut core, mut shaper) = pagination_core("<html><body style='margin:0'><p style='margin:0'>a a</p></body></html>", Size::new(400.0, 300.0), 1);
    for factor in [1.5, 0.5, 1.0] {
        core.apply(&mut shaper, crate::RendererCommand::SetReaderStyleOverrides(html::pipeline::ReaderStyleOverrides {
            word_spacing_factor: (factor != 1.0).then_some(factor), ..Default::default()
        }));
        core.paint_forward(&mut RecordingPainter::default());
        let geometry = core.text_geometry();
        let text = core.page.view.document.render_view().text();
        let line_idx = text.line_index_for_glyph(1).expect("space line");
        let line = text.line(line_idx).unwrap();
        let first_x = geometry.glyph_x_in_line(&line, 0);
        let second_x = geometry.glyph_x_in_line(&line, 2);
        let expected_gap = 8.0 * f64::from(factor);
        assert!((second_x - first_x - 8.0 - expected_gap).abs() < 1.0e-6, "word origins must use the scaled gap");
        let bounds = geometry.glyph_range_bounds(1, 2).expect("space selection");
        assert!((bounds.width() - expected_gap).abs() < 1.0e-6, "selection must cover the complete adjusted space");
        let y = (bounds.y0 + bounds.y1) / 2.0;
        assert_eq!(geometry.hit_test_glyph(Point::new(bounds.x0 + expected_gap * 0.25, y)), Some(1));
        assert_eq!(geometry.hit_test_glyph(Point::new(bounds.x0 + expected_gap * 0.75, y)), Some(2));
    }
}

#[test]
fn reader_color_update_preserves_chapter_text_selection_and_geometry() {
    let (mut core, mut shaper) = pagination_core("<p id='anchor'>Selected <a href='#anchor'>link</a> and ordinary text.</p>", Size::new(400.0, 300.0), 1);
    core.page.view.selection.select_range(0, 8, crate::SelectionMode::Plain);
    core.page.view.update_selection_text();
    let selected = core.page.view.selection.content().plain().map(str::to_owned);
    let text = core.page.inputs.source.clone();
    let layout_revision = core.revisions.layout;
    let display_revision = core.revisions.base_display;
    let line_count = core.page.view.doc().text().line_count();
    shaper.reject_glyphs = true;
    core.apply(&mut shaper, crate::RendererCommand::SetReaderStyleOverrides(html::pipeline::ReaderStyleOverrides { link_foreground: Some(0x123456ff), ..Default::default() }));
    assert!(Arc::ptr_eq(&text, &core.page.inputs.source));
    assert_eq!(core.page.view.selection.content().plain(), selected.as_deref());
    assert_eq!(core.revisions.layout, layout_revision);
    assert!(core.revisions.base_display > display_revision);
    assert_eq!(core.page.view.doc().text().line_count(), line_count);
    assert_eq!(core.page.inputs.reader_overrides.link_foreground, Some(0x123456ff));
}


#[test]
fn publication_fonts_follow_navigation_and_refresh_preserves_reading_position() {
    let (provider, _) = CountingProvider::new(vec![
        ("one.html", "<html><head><style>@font-face { font-family: First; src: url(first.ttf) }</style></head><body><p>First chapter</p></body></html>"),
        ("two.html", "<html><head><style>@font-face { font-family: Second; src: url(second.ttf) }</style></head><body><p>Second chapter</p></body></html>"),
    ]);
    let host = Rc::new(TestHost::default());
    let mut session = crate::RendererSession::from_provider_with_nav(host.clone(), TestShaper::default(), provider, vec!["one.html".into(), "two.html".into()], 0, None, RendererInitialConfig::default());
    assert_eq!(host.font_preparations.borrow().last().unwrap()[0].family, "First");
    let before = session.revisions().shape;
    let position = session.current_cfi();
    session.apply(crate::RendererCommand::RefreshFonts);
    assert!(session.revisions().shape > before);
    assert_eq!(session.current_cfi(), position);
    session.next_document();
    assert_eq!(host.font_preparations.borrow().last().unwrap()[0].family, "Second");
    session.previous_document();
    assert_eq!(host.font_preparations.borrow().last().unwrap()[0].family, "First");
}

#[test]
fn malformed_cfi_zero_steps_return_none_for_points_and_ranges() {
    let (core, _, _) = source_position_reader("<html><body><p>Text</p></body></html>");
    let doc = &core.page.view.document;
    for cfi in ["epubcfi(/6/2!/0)", "epubcfi(/6/2!/4/0/1:0)", "epubcfi(/6/2!/4/2/0:0)"] {
        assert_eq!(crate::cfi::parse_cfi(doc, cfi), None);
        assert_eq!(crate::cfi::parse_source_position(cfi), None);
    }
    for cfi in ["epubcfi(/6/2!/4/0,/1:0,/1:2)", "epubcfi(/6/2!/4/2,/0:0,/1:2)", "epubcfi(/6/2!/4/2,/1:0,/0:2)"] {
        assert_eq!(crate::cfi::parse_cfi_range(doc, cfi), None);
    }
}
