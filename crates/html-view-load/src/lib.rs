use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

use html::layout::GlyphShaper;
use html::layout::{LaidOutDocument, LayoutTimings};
use html::resources::ResourceProvider;
use html::engine::Engine;
use html::pipeline::{
    FontEnvironmentRevision, ImageMetricsRevision, ImageSizingPolicy, LayoutConstraints as PipelineLayoutConstraints, MarkupSyntax, PaintSettingsRevision, PipelineInputs, ReaderStyleOverrides, ResourceRevision, SourceRevision,
    StyleEnvironment, StylesheetRevision,
};
use html_view_types::NoteDisplay;


pub struct LoadedRenderDocument {
    pub session: Engine,
    pub inputs: PipelineInputs,
    pub document: LaidOutDocument,
    pub source: String,
}

/// Initial document state prepared without a platform glyph shaper.
///
/// This value is `Send` as long as the resource provider is `Send`, so UI
/// adapters can construct it on their worker executor and finish shaping on
/// the thread that owns their text system.
pub struct PreparedRenderDocument {
    pub session: Engine,
    pub inputs: PipelineInputs,
    pub source: String,
}

#[derive(Clone)]
pub struct DocumentLoadSettings {
    pub root_font_size: f32,
    pub column_width: f64,
    pub reader_overrides: ReaderStyleOverrides,
    pub image_sizing_policy: ImageSizingPolicy,
    pub text_composition_policy: html::pipeline::TextCompositionPolicy,
    pub note_display: NoteDisplay,
}

impl DocumentLoadSettings {
    pub fn book(root_font_size: f32, column_width: f64) -> Self {
        Self {
            root_font_size,
            column_width,
            reader_overrides: ReaderStyleOverrides::default(),
            image_sizing_policy: ImageSizingPolicy::SmartStandalone,
            text_composition_policy: html::pipeline::TextCompositionPolicy::BookOptimized,
            note_display: NoteDisplay::default(),
        }
    }
}

/// How a presentation choice reaches the pipeline. Popup mode holds notes back
/// from the flow so the reader can show them itself; as-authored leaves them
/// where they were written.
pub fn note_flow_for(note_display: NoteDisplay) -> html::pipeline::NoteFlow {
    match note_display {
        NoteDisplay::Popup => html::pipeline::NoteFlow::Excluded,
        NoteDisplay::AsAuthored => html::pipeline::NoteFlow::InFlow,
    }
}

/// EPUB manifest media types take precedence over filenames: XHTML chapters
/// are often named `.html`, where HTML parsing would swallow `<a id="..."/>`
/// page markers and color subsequent text as links.
pub fn markup_syntax_for_resource(provider: &dyn ResourceProvider, uri: &str) -> MarkupSyntax {
    let media_type = provider.metadata(uri).ok().and_then(|metadata| metadata.media_type);
    match media_type.as_deref().map(|value| value.split(';').next().unwrap_or(value).trim()) {
        Some(value) if value.eq_ignore_ascii_case("application/xhtml+xml") || value.eq_ignore_ascii_case("application/xml") || value.eq_ignore_ascii_case("text/xml") => MarkupSyntax::Xml,
        Some(value) if value.eq_ignore_ascii_case("text/html") => MarkupSyntax::Html,
        _ => MarkupSyntax::from_uri(uri),
    }
}

pub fn load_css_with_provider(provider: &dyn ResourceProvider, base_uri: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    for candidate in ["computer_style.css", "style.css"] {
        let uri = provider.resolve(base_uri, candidate);
        if provider.exists(&uri)
            && let Ok(extra) = provider.read_string(&uri)
        {
            chunks.push(extra);
        }
    }
    chunks
}

pub fn load_document_with_dom_pipeline(provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, glyph_shaper: &mut impl GlyphShaper) -> LoadedRenderDocument {
    load_document_with_dom_pipeline_impl(
        provider,
        uri,
        DocumentLoadSettings::book(root_font_size, col_width),
        glyph_shaper,
        None,
    )
}

pub fn load_document_with_dom_pipeline_timed(provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, glyph_shaper: &mut impl GlyphShaper) -> (LoadedRenderDocument, PipelineTimings) {
    let mut timings = PipelineTimings::default();
    let loaded = load_document_with_dom_pipeline_impl(
        provider,
        uri,
        DocumentLoadSettings::book(root_font_size, col_width),
        glyph_shaper,
        Some(&mut timings),
    );
    (loaded, timings)
}

pub fn load_document_with_settings(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
    glyph_shaper: &mut impl GlyphShaper,
) -> LoadedRenderDocument {
    load_document_with_dom_pipeline_impl(provider, uri, settings, glyph_shaper, None)
}

pub fn prepare_document_with_settings(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
) -> Result<PreparedRenderDocument, String> {
    let DocumentLoadSettings {
        root_font_size,
        column_width,
        reader_overrides,
        image_sizing_policy,
        text_composition_policy,
        note_display,
    } = settings;
    let source_bytes = provider
        .read_bytes(uri)
        .map_err(|error| format!("failed to load HTML resource {uri}: {error}"))?;
    let transport_encoding = provider.metadata(uri).ok().and_then(|metadata| metadata.charset);
    let source = html::pipeline::decode_html_bytes(&source_bytes, transport_encoding.as_deref());
    let layout_constraints = PipelineLayoutConstraints { viewport_width: column_width, viewport_height: None, line_height: root_font_size as f64, image_sizing_policy, text_composition_policy };
    let inputs = PipelineInputs {
        source: source.clone(),
        markup_syntax: markup_syntax_for_resource(provider.as_ref(), uri),
        user_styles: Vec::new(),
        reader_overrides,
        note_flow: note_flow_for(note_display),
        source_revision: SourceRevision::INITIAL,
        base_uri: uri.to_string(),
        resource_revision: ResourceRevision::INITIAL,
        stylesheet_revision: StylesheetRevision::INITIAL,
        style_environment: StyleEnvironment { root_font_size: root_font_size.max(1.0).round() as u32, media: html::pipeline::MediaEnvironment::screen(column_width, None).ok_or_else(|| "column width must be finite and positive".to_owned())?, direction: 0 },
        font_environment: FontEnvironmentRevision::INITIAL,
        image_metrics_revision: ImageMetricsRevision::INITIAL,
        layout: layout_constraints,
        image_metrics: Default::default(),
        paint: PaintSettingsRevision::INITIAL,
    };
    let mut session = Engine::new(provider);
    session.prepare_through_style(inputs.clone()).map_err(|error| error.to_string())?;
    Ok(PreparedRenderDocument { session, inputs, source })
}

pub fn finish_prepared_document(
    mut prepared: PreparedRenderDocument,
    glyph_shaper: &mut impl GlyphShaper,
) -> Result<LoadedRenderDocument, String> {
    let document = prepared.session.rehydrate_glyphs(glyph_shaper).map_err(|error| error.to_string())?;
    Ok(LoadedRenderDocument { session: prepared.session, inputs: prepared.inputs, document, source: prepared.source })
}

fn load_document_with_dom_pipeline_impl(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
    glyph_shaper: &mut impl GlyphShaper,
    mut timings: Option<&mut PipelineTimings>,
) -> LoadedRenderDocument {
    let DocumentLoadSettings {
        root_font_size,
        column_width,
        reader_overrides,
        image_sizing_policy,
        text_composition_policy,
        note_display,
    } = settings;
    let load_started = Instant::now();
    let source_bytes = provider.read_bytes(uri).expect("Failed to load HTML");
    let transport_encoding = provider.metadata(uri).ok().and_then(|metadata| metadata.charset);
    let html = html::pipeline::decode_html_bytes(&source_bytes, transport_encoding.as_deref());
    if let Some(timings) = timings.as_deref_mut() {
        timings.load_html += load_started.elapsed();
    }

    let start = Instant::now();
    let mut session = Engine::new(provider.clone());
    let layout_constraints = PipelineLayoutConstraints { viewport_width: column_width, viewport_height: None, line_height: root_font_size as f64, image_sizing_policy, text_composition_policy };
    let inputs = PipelineInputs {
        source: html.clone(),
        markup_syntax: markup_syntax_for_resource(provider.as_ref(), uri),
        user_styles: Vec::new(),
        reader_overrides,
        note_flow: note_flow_for(note_display),
        source_revision: SourceRevision::INITIAL,
        base_uri: uri.to_string(),
        resource_revision: ResourceRevision::INITIAL,
        stylesheet_revision: StylesheetRevision::INITIAL,
        style_environment: StyleEnvironment { root_font_size: root_font_size.max(1.0).round() as u32, media: html::pipeline::MediaEnvironment::screen(column_width, None).expect("column width must be finite and positive"), direction: 0 },
        font_environment: FontEnvironmentRevision::INITIAL,
        image_metrics_revision: ImageMetricsRevision::INITIAL,
        layout: layout_constraints,
        image_metrics: Default::default(),
        paint: PaintSettingsRevision::INITIAL,
    };

    session.update(inputs.clone(), glyph_shaper).expect("initial pipeline update should succeed");
    let document = session.document().expect("pipeline session should own a laid-out document").clone();
    if let Some(timings) = timings {
        timings.layout_total += start.elapsed();
    }

    LoadedRenderDocument { session, inputs, document, source: html }
}

pub fn load_document(provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, glyph_shaper: &mut impl GlyphShaper) -> LoadedRenderDocument {
    // Use the new DOM-based pipeline
    load_document_with_dom_pipeline(provider, uri, root_font_size, col_width, glyph_shaper)
}

#[derive(Default, Debug, Clone)]
pub struct PipelineTimings {
    pub load_html: Duration,
    pub parse_html: Duration,
    pub extract_title: Duration,
    pub load_external_css: Duration,
    pub collect_inline_css: Duration,
    pub build_document_pipeline: Duration,
    pub set_metadata: Duration,
    pub probe_image_dimensions: Duration,
    pub shape_text: Duration,
    pub build_anchor_glyphs: Duration,
    pub layout_total: Duration,
    pub layout_detail: LayoutTimings,
    pub build_anchor_positions: Duration,
}

impl PipelineTimings {
    pub fn total(&self) -> Duration {
        self.load_html
            + self.parse_html
            + self.extract_title
            + self.load_external_css
            + self.collect_inline_css
            + self.build_document_pipeline
            + self.set_metadata
            + self.probe_image_dimensions
            + self.shape_text
            + self.build_anchor_glyphs
            + self.layout_total
            + self.build_anchor_positions
    }

    pub fn add_assign(&mut self, other: &Self) {
        self.load_html += other.load_html;
        self.parse_html += other.parse_html;
        self.extract_title += other.extract_title;
        self.load_external_css += other.load_external_css;
        self.collect_inline_css += other.collect_inline_css;
        self.build_document_pipeline += other.build_document_pipeline;
        self.set_metadata += other.set_metadata;
        self.probe_image_dimensions += other.probe_image_dimensions;
        self.shape_text += other.shape_text;
        self.build_anchor_glyphs += other.build_anchor_glyphs;
        self.layout_total += other.layout_total;
        self.build_anchor_positions += other.build_anchor_positions;
        self.layout_detail.clear_layout_output += other.layout_detail.clear_layout_output;
        self.layout_detail.layout_tree_traversal += other.layout_detail.layout_tree_traversal;
        self.layout_detail.root_box_layout += other.layout_detail.root_box_layout;
        self.layout_detail.finalize_layout += other.layout_detail.finalize_layout;
        self.layout_detail.sort_lines_and_remap_images += other.layout_detail.sort_lines_and_remap_images;
        self.layout_detail.collect_inline_decorations += other.layout_detail.collect_inline_decorations;
        self.layout_detail.rebuild_image_fragments_by_line += other.layout_detail.rebuild_image_fragments_by_line;
        self.layout_detail.layout_box_total += other.layout_detail.layout_box_total;
        self.layout_detail.layout_block_children += other.layout_detail.layout_block_children;
        self.layout_detail.layout_table += other.layout_detail.layout_table;
        self.layout_detail.layout_table_row += other.layout_detail.layout_table_row;
        self.layout_detail.layout_runs += other.layout_detail.layout_runs;
        self.layout_detail.layout_runs_around_float_exclusions += other.layout_detail.layout_runs_around_float_exclusions;
        self.layout_detail.build_inline_tokens += other.layout_detail.build_inline_tokens;
        self.layout_detail.build_inline_tokens_from_runs += other.layout_detail.build_inline_tokens_from_runs;
        self.layout_detail.break_lines += other.layout_detail.break_lines;
        self.layout_detail.break_lines_knuth += other.layout_detail.break_lines_knuth;
        self.layout_detail.emit_lines += other.layout_detail.emit_lines;
        self.layout_detail.measure_line += other.layout_detail.measure_line;
        self.layout_detail.write_line_fragments += other.layout_detail.write_line_fragments;
        self.layout_detail.place_float_anchor += other.layout_detail.place_float_anchor;
    }
}

#[cfg(test)]
mod tests {
    use html::document::NodeRef;
    use html::layout::LayoutConstraints;
    use html::layout::GlyphShaper;
    use html::pipeline::DocumentFactory;
    use html::resources::{FileSystemProvider, ResourceMetadata, ResourceProvider};
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphResourceStore};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TypedChapterProvider {
        media_type: Option<&'static str>,
    }

    impl ResourceProvider for TypedChapterProvider {
        fn read_bytes(&self, _uri: &str) -> std::io::Result<Vec<u8>> {
            Ok(br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p><a id="page1"/>Ordinary text</p></body></html>"#.to_vec())
        }

        fn metadata(&self, _uri: &str) -> std::io::Result<ResourceMetadata> {
            Ok(ResourceMetadata { media_type: self.media_type.map(str::to_owned), charset: None })
        }

        fn exists(&self, _uri: &str) -> bool { true }
        fn resolve(&self, _base: &str, href: &str) -> String { href.to_owned() }
        fn list_html_candidates(&self, _root: &str) -> std::io::Result<Vec<String>> { Ok(vec!["chapter.html".to_owned()]) }
    }

    #[test]
    fn declared_xhtml_keeps_self_closing_page_marker_empty_in_html_named_chapter() {
        let provider = TypedChapterProvider { media_type: Some("application/xhtml+xml") };
        let syntax = super::markup_syntax_for_resource(&provider, "chapter.html");
        assert_eq!(syntax, html::parse::MarkupSyntax::Xml);
        let source = String::from_utf8(provider.read_bytes("chapter.html").unwrap()).unwrap();
        let document = html::parse::parse_document(&source, syntax).unwrap().build_dom();
        let root = document.element_ref(document.dom_root().unwrap()).unwrap();
        let body = root.children().find_map(|id| document.element_ref(id).filter(|element| element.tag() == "body")).unwrap();
        let paragraph = body.children().find_map(|id| document.element_ref(id).filter(|element| element.tag() == "p")).unwrap();
        let mut children = paragraph.children();
        let marker = document.element_ref(children.next().unwrap()).unwrap();
        assert_eq!(marker.tag(), "a");
        assert!(marker.children().next().is_none());
        assert!(matches!(document.node_ref(children.next().unwrap()), Some(NodeRef::Text(text)) if text.text() == "Ordinary text"));

        assert_eq!(super::markup_syntax_for_resource(&TypedChapterProvider { media_type: Some("text/html") }, "chapter.xhtml"), html::parse::MarkupSyntax::Html);
        assert_eq!(super::markup_syntax_for_resource(&TypedChapterProvider { media_type: None }, "chapter.xhtml"), html::parse::MarkupSyntax::Xml);
    }

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        glyph_store: GlyphResourceStore,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceStore)>,
    }

    struct DocumentMemoryProfile {
        uri: String,
        total: usize,
        top_entries: Vec<(String, usize)>,
    }

    impl GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.glyph_store.clear();
        }

        fn glyph_resources(&mut self) -> &mut GlyphResourceStore {
            &mut self.glyph_store
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::layout::ShapeError> {
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
            }
        }

        fn shape_glyph(&mut self, ch: char, font_size: f32, font_weight: u16, font_slant: FontSlant, color: u32, family: Option<&str>) -> Result<GlyphId, html::layout::ShapeError> {
            let _ = (font_weight, font_slant, color, family);
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = self.glyph_store.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout_html(html: &str) -> html::layout::LaidOutDocument {
        let mut factory = DocumentFactory::new();
        let mut glyph_shaper = TestGlyphShaper::default();
        factory.parse_with_new_pipeline(html, None).shape(&mut glyph_shaper).expect("test shaper must register every glyph").layout(LayoutConstraints::new(600.0, 20.0).unwrap())
    }

    #[test]
    fn initial_preparation_crosses_a_worker_boundary_before_platform_shaping() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("html-view-preparation-{nonce}.html"));
        fs::write(&path, "<html><head><style>p { color: red }</style></head><body><p>Prepared text</p></body></html>").unwrap();
        let uri = path.to_string_lossy().into_owned();
        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let prepared = std::thread::spawn(move || super::prepare_document_with_settings(provider, &uri, super::DocumentLoadSettings::book(16.0, 600.0)))
            .join()
            .expect("preparation worker should not panic")
            .expect("document should prepare");
        let mut shaper = TestGlyphShaper::default();
        let loaded = super::finish_prepared_document(prepared, &mut shaper).expect("platform shaping should finish");
        assert!(loaded.document.render_view().text().glyph_count() > 0);
        let _ = fs::remove_file(path);
    }

    fn li_boxes(document: &html::layout::LaidOutDocument) -> Vec<usize> {
        let boxes = document.render_view().boxes();
        (0..boxes.len()).filter(|&i| boxes.tag(i).is_some_and(|tag| tag.eq_ignore_ascii_case("li"))).collect()
    }

    /// Reconstruct a list item's marker text from its marker box's shaped glyphs.
    fn marker_string(document: &html::layout::LaidOutDocument, li_box: usize) -> String {
        let root = document.render_view();
        let boxes = root.boxes();
        let text = root.text();
        let marker = boxes.list_marker(li_box).expect("li should have a marker");
        let mut out = String::new();
        for run in text.marker_runs().only_box(marker.marker_box()) {
            for g in run.glyphs() {
                let glyph_id = text.glyph_at(g as usize).expect("marker glyph");
                out.push(text.glyph_metric(glyph_id).expect("registered marker glyph").ch());
            }
        }
        out
    }

    #[test]
    fn ordered_list_counts_with_value_overrides() {
        let document = layout_html("<html><body><ol><li>a</li><li>b</li><li value=\"10\">c</li><li>d</li></ol></body></html>");
        let li = li_boxes(&document);
        assert_eq!(marker_string(&document, li[0]), "1.");
        assert_eq!(marker_string(&document, li[1]), "2.");
        assert_eq!(marker_string(&document, li[2]), "10.");
        assert_eq!(marker_string(&document, li[3]), "11.");
    }

    #[test]
    fn ordered_list_start_and_reversed() {
        let start = layout_html("<html><body><ol start=\"5\"><li>a</li><li>b</li></ol></body></html>");
        let li = li_boxes(&start);
        assert_eq!(marker_string(&start, li[0]), "5.");
        assert_eq!(marker_string(&start, li[1]), "6.");

        let rev = layout_html("<html><body><ol reversed><li>a</li><li>b</li><li>c</li></ol></body></html>");
        let li = li_boxes(&rev);
        assert_eq!(marker_string(&rev, li[0]), "3.");
        assert_eq!(marker_string(&rev, li[1]), "2.");
        assert_eq!(marker_string(&rev, li[2]), "1.");
    }

    #[test]
    fn nested_unordered_bullets_by_depth() {
        let document = layout_html("<html><body><ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li></ul></body></html>");
        let li = li_boxes(&document);
        assert_eq!(marker_string(&document, li[0]), "\u{2022}"); // disc
        assert_eq!(marker_string(&document, li[1]), "\u{25E6}"); // circle
        assert_eq!(marker_string(&document, li[2]), "\u{25AA}"); // square
    }

    #[test]
    fn inside_marker_sits_at_content_edge() {
        use html::layout::ListStylePosition;
        let document = layout_html("<html><body><ul style=\"list-style-position: inside\"><li>Item</li></ul></body></html>");
        let boxes = document.render_view().boxes();
        let li = li_boxes(&document)[0];
        let marker = boxes.list_marker(li).expect("marker");
        assert_eq!(marker.position(), ListStylePosition::Inside);
        // The 40px padding-inline-start is on the <ul>, so the <li> is already
        // positioned inside it; the li's own content edge is its box origin.
        // Inside markers sit at that edge rather than hanging to its left.
        let marker_x = boxes.point(marker.marker_box()).expect("marker box should exist").x;
        let content_left = boxes.point(li).expect("li box should exist").x;
        assert!((marker_x - content_left).abs() < 1.0, "inside marker x {marker_x} should be at content edge {content_left}");
    }

    #[test]
    fn list_item_generates_positioned_outside_marker() {
        use html::layout::ListStylePosition;
        let document = layout_html("<html><body><ul><li>Item one</li></ul></body></html>");
        let boxes = document.render_view().boxes();

        let li_idx = (0..boxes.len()).find(|&i| boxes.tag(i).is_some_and(|tag| tag.eq_ignore_ascii_case("li"))).expect("li box");
        let marker = boxes.list_marker(li_idx).expect("li should have a marker");
        assert_eq!(marker.position(), ListStylePosition::Outside);

        let marker_size = boxes.size(marker.marker_box()).expect("marker box should exist");
        assert!(marker_size.width > 0.0, "marker should have non-zero width, got {marker_size:?}");

        // Outside marker hangs left of the content edge (li content begins after
        // the UA 40px padding-inline-start).
        let marker_x = boxes.point(marker.marker_box()).expect("marker box should exist").x;
        let content_left = boxes.point(li_idx).expect("li box should exist").x + 40.0;
        assert!(marker_x < content_left, "outside marker x {marker_x} should be left of content edge {content_left}");

        // A Marker run exists and carries glyphs.
        assert!(
            document.render_view().text().marker_runs().into_iter().any(|run| {
                let glyphs = run.glyphs();
                glyphs.start < glyphs.end
            }),
            "a non-empty Marker run should exist"
        );
    }

    #[test]
    fn nested_inline_text_in_anchor_maps_glyphs_to_href() {
        let html = "<html><body><a href=\"chapter2.xhtml#target\"><span>Link</span></a></body></html>";
        let mut factory = DocumentFactory::new();
        let mut glyph_shaper = TestGlyphShaper::default();
        let document = factory.parse_with_new_pipeline(html, None).shape(&mut glyph_shaper).expect("test shaper must register every glyph").layout(LayoutConstraints::new(600.0, 20.0).unwrap());
        let root = document.render_view();
        let boxes = root.boxes();
        let text = root.text();
        let addressing = root.addressing();

        let href_idx = root.lookup_string("chapter2.xhtml#target").expect("href should be interned");
        let span_idx = (0..boxes.len()).find(|&i| boxes.tag(i).is_some_and(|tag| tag == "span")).expect("span box should exist");
        let mut glyphs = None;
        for run in text.text_runs().only_box(span_idx) {
            glyphs = Some(run.glyphs());
        }
        let glyphs = glyphs.expect("span text should exist");

        assert!(glyphs.start < glyphs.end, "span text should produce glyphs");
        for glyph_idx in glyphs {
            assert_eq!(addressing.link_for_glyph(glyph_idx), Some(href_idx));
        }
    }

    #[test]
    #[ignore = "profiling output is noisy; run with --ignored --nocapture"]
    fn profile_pipeline_stages_for_testdata_html() {
        let mut files = Vec::new();
        let testdata = profile_corpus();
        collect_html_files(&testdata, &mut files);
        files.sort();

        assert!(!files.is_empty(), "no HTML files found under {}", testdata.display());

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut totals = super::PipelineTimings::default();
        let mut processed = 0usize;
        let mut failed = 0usize;

        for path in files {
            let uri = path.to_string_lossy().into_owned();
            let mut glyph_cache = TestGlyphShaper::default();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::load_document_with_dom_pipeline_timed(provider.clone(), &uri, 16.0, 600.0, &mut glyph_cache))) {
                Ok((_document, timings)) => {
                    totals.add_assign(&timings);
                    processed += 1;
                    println!(
                        "{}\n  total: {:>8.3} ms   load: {:>8.3} parse: {:>8.3} doc: {:>8.3} layout: {:>8.3}",
                        path.display(),
                        timings.total().as_secs_f64() * 1000.0,
                        timings.load_html.as_secs_f64() * 1000.0,
                        timings.parse_html.as_secs_f64() * 1000.0,
                        timings.build_document_pipeline.as_secs_f64() * 1000.0,
                        timings.layout_total.as_secs_f64() * 1000.0,
                    );
                }
                Err(_) => {
                    failed += 1;
                    eprintln!("{}: pipeline panicked", path.display());
                }
            }
        }

        println!("\n=== html-view-core pipeline timing summary ===");
        println!("files processed: {processed}   failed: {failed}");
        println!("total: {:>8.3} ms", totals.total().as_secs_f64() * 1000.0);
        print_stage("load html", totals.load_html, processed);
        print_stage("parse html", totals.parse_html, processed);
        print_stage("extract title", totals.extract_title, processed);
        print_stage("load external css", totals.load_external_css, processed);
        print_stage("collect inline css", totals.collect_inline_css, processed);
        print_stage("build document pipeline", totals.build_document_pipeline, processed);
        print_stage("set metadata", totals.set_metadata, processed);
        print_stage("probe image dims", totals.probe_image_dimensions, processed);
        print_stage("shape text", totals.shape_text, processed);
        print_stage("build anchor glyphs", totals.build_anchor_glyphs, processed);
        print_stage("layout total", totals.layout_total, processed);
        print_stage("layout: clear output", totals.layout_detail.clear_layout_output, processed);
        print_stage("layout: traversal", totals.layout_detail.layout_tree_traversal, processed);
        print_stage("layout: sort lines", totals.layout_detail.sort_lines_and_remap_images, processed);
        print_stage("layout: decorations", totals.layout_detail.collect_inline_decorations, processed);
        print_stage("layout: rebuild image lines", totals.layout_detail.rebuild_image_fragments_by_line, processed);
        print_stage("layout: layout box", totals.layout_detail.layout_box_total, processed);
        print_stage("layout: block children", totals.layout_detail.layout_block_children, processed);
        print_stage("layout: table", totals.layout_detail.layout_table, processed);
        print_stage("layout: table row", totals.layout_detail.layout_table_row, processed);
        print_stage("layout: inline runs", totals.layout_detail.layout_runs, processed);
        print_stage("layout: inline runs float exclusions", totals.layout_detail.layout_runs_around_float_exclusions, processed);
        print_stage("layout: build tokens", totals.layout_detail.build_inline_tokens, processed);
        print_stage("layout: build tokens/runs", totals.layout_detail.build_inline_tokens_from_runs, processed);
        print_stage("layout: break lines", totals.layout_detail.break_lines, processed);
        print_stage("layout: break lines knuth", totals.layout_detail.break_lines_knuth, processed);
        print_stage("layout: emit lines", totals.layout_detail.emit_lines, processed);
        print_stage("layout: measure line", totals.layout_detail.measure_line, processed);
        print_stage("layout: write fragments", totals.layout_detail.write_line_fragments, processed);
        print_stage("layout: place float anchor", totals.layout_detail.place_float_anchor, processed);
        print_stage("build anchor positions", totals.build_anchor_positions, processed);

        assert!(processed > 0, "expected at least one HTML file to be processed");
    }

    #[test]
    #[ignore = "profiling output is noisy; run with --ignored --nocapture"]
    fn profile_testdata_memory_usage() {
        let testdata = profile_corpus();
        let mut files = Vec::new();
        collect_html_files(&testdata, &mut files);
        files.sort();

        let mut aggregate: HashMap<String, usize> = HashMap::new();
        let mut heaviest: Option<DocumentMemoryProfile> = None;
        let mut processed = 0usize;
        let mut failed = 0usize;
        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());

        for path in files {
            let uri = path.to_string_lossy().into_owned();
            let mut glyph_shaper = TestGlyphShaper::default();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::load_document(provider.clone(), &uri, 16.0, 600.0, &mut glyph_shaper)));
            let document = match result {
                Ok(document) => document,
                Err(_) => {
                    failed += 1;
                    eprintln!("{uri}: renderer panicked");
                    continue;
                }
            };

            let mut report = document.document.memory_usage_report();
            report.sort_by_bytes_desc();
            let total = report.total_bytes();
            processed += 1;

            for entry in &report.entries {
                *aggregate.entry(entry.label.clone()).or_insert(0) += entry.bytes;
            }

            let top_entries: Vec<(String, usize)> = report.entries.iter().take(10).map(|entry| (entry.label.clone(), entry.bytes)).collect();
            if heaviest.as_ref().is_none_or(|profile| total > profile.total) {
                heaviest = Some(DocumentMemoryProfile {
                    uri: uri.clone(),
                    total,
                    top_entries,
                });
            }
        }

        let mut aggregate_entries: Vec<(String, usize)> = aggregate.into_iter().collect();
        aggregate_entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        println!("\n=== html-view-core memory summary ===");
        println!("files processed: {processed}   failed: {failed}");
        if let Some(DocumentMemoryProfile {
            uri,
            total,
            top_entries,
        }) = heaviest
        {
            println!("heaviest document: {uri} ({:.2} KiB)", total as f64 / 1024.0);
            for (label, bytes) in top_entries {
                println!("  {:>12}  {}", format!("{:.2} KiB", bytes as f64 / 1024.0), label);
            }
        }
        println!("\nTop aggregate memory consumers:");
        for (label, bytes) in aggregate_entries.into_iter().take(25) {
            println!("{:>12}  {}", format!("{:.2} KiB", bytes as f64 / 1024.0), label);
        }

        assert!(processed > 0, "expected at least one HTML file to be processed");
    }

    fn profile_corpus() -> PathBuf {
        std::env::var_os("HTML_VIEW_PROFILE_CORPUS").map(PathBuf::from).expect("set HTML_VIEW_PROFILE_CORPUS to a directory containing HTML documents")
    }

    fn collect_html_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_html_files(&path, out);
                continue;
            }
            let is_html = matches!(path.extension().and_then(|ext| ext.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(), Some("html") | Some("htm") | Some("xhtml"));
            if is_html {
                out.push(path);
            }
        }
    }

    fn print_stage(label: &str, total: Duration, processed: usize) {
        let per_file = total.as_secs_f64() * 1000.0 / processed.max(1) as f64;
        println!("{label:<24} {:>8.3} ms total   {:>8.3} ms/file", total.as_secs_f64() * 1000.0, per_file);
    }
}
