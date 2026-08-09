use std::sync::Arc;
use std::time::{Duration, Instant};

use html::layout::GlyphShaper;
use html::layout::{LaidOutDocument, LayoutTimings};
use html::pipeline::BuildPipelineTimings;
use html::resources::ResourceProvider;
use html::engine::Engine;
use html::pipeline::{
    FontEnvironmentRevision, ImageMetricsRevision, ImageSizingPolicy, LayoutConstraints as PipelineLayoutConstraints, MarkupSyntax, PaintSettingsRevision, PipelineInputs, ReaderStyleOverrides, ResourceRevision, SourceRevision,
    StyleEnvironment, StylesheetRevision,
};

/// Notes are rendered on demand by the reader's footnote preview. Keeping
/// their source subtree in the document preserves link-target extraction,
/// while this reader-layer rule removes it from the paginated reading flow.
const READER_FOOTNOTE_CSS: &str = r#"
[epub\:type~="footnote"],
[epub\:type~="endnote"],
[epub\:type~="rearnote"],
[*|type~="footnote"],
[*|type~="endnote"],
[*|type~="rearnote"],
[role~="doc-footnote"],
[role~="doc-endnote"] {
    display: none !important;
}
"#;

pub struct LoadedRenderDocument {
    pub session: Engine,
    pub inputs: PipelineInputs,
    pub document: LaidOutDocument,
    pub source: String,
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
    load_document_with_dom_pipeline_impl(provider, uri, root_font_size, col_width, Default::default(), ImageSizingPolicy::SmartStandalone, html::pipeline::TextCompositionPolicy::BookOptimized, glyph_shaper, None)
}

pub fn load_document_with_dom_pipeline_timed(provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, glyph_shaper: &mut impl GlyphShaper) -> (LoadedRenderDocument, PipelineTimings) {
    let mut timings = PipelineTimings::default();
    let loaded = load_document_with_dom_pipeline_impl(provider, uri, root_font_size, col_width, Default::default(), ImageSizingPolicy::SmartStandalone, html::pipeline::TextCompositionPolicy::BookOptimized, glyph_shaper, Some(&mut timings));
    (loaded, timings)
}

pub fn load_document_with_settings(
    provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, reader_overrides: ReaderStyleOverrides, image_sizing_policy: ImageSizingPolicy, text_composition_policy: html::pipeline::TextCompositionPolicy,
    glyph_shaper: &mut impl GlyphShaper,
) -> LoadedRenderDocument {
    load_document_with_dom_pipeline_impl(provider, uri, root_font_size, col_width, reader_overrides, image_sizing_policy, text_composition_policy, glyph_shaper, None)
}

fn load_document_with_dom_pipeline_impl(
    provider: Arc<dyn ResourceProvider>, uri: &str, root_font_size: f32, col_width: f64, reader_overrides: ReaderStyleOverrides, image_sizing_policy: ImageSizingPolicy, text_composition_policy: html::pipeline::TextCompositionPolicy,
    glyph_shaper: &mut impl GlyphShaper, mut timings: Option<&mut PipelineTimings>,
) -> LoadedRenderDocument {
    let load_started = Instant::now();
    let source_bytes = provider.read_bytes(uri).expect("Failed to load HTML");
    let transport_encoding = provider.metadata(uri).ok().and_then(|metadata| metadata.charset);
    let html = html::pipeline::decode_html_bytes(&source_bytes, transport_encoding.as_deref());
    if let Some(timings) = timings.as_deref_mut() {
        timings.load_html += load_started.elapsed();
    }

    let start = Instant::now();
    let mut session = Engine::new(provider.clone());
    let layout_constraints = PipelineLayoutConstraints { viewport_width: col_width, viewport_height: None, line_height: root_font_size as f64, image_sizing_policy, text_composition_policy };
    let inputs = PipelineInputs {
        source: html.clone(),
        markup_syntax: MarkupSyntax::from_uri(uri),
        user_styles: vec![READER_FOOTNOTE_CSS.to_owned()],
        reader_overrides,
        source_revision: SourceRevision::INITIAL,
        base_uri: uri.to_string(),
        resource_revision: ResourceRevision::INITIAL,
        stylesheet_revision: StylesheetRevision::INITIAL,
        style_environment: StyleEnvironment { root_font_size: root_font_size.max(1.0).round() as u32, media: html::pipeline::MediaEnvironment::screen(col_width, None).expect("column width must be finite and positive"), direction: 0 },
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
    pub document_pipeline: BuildPipelineTimings,
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
        self.document_pipeline.build_dom_tree += other.document_pipeline.build_dom_tree;
        self.document_pipeline.parse_default_css += other.document_pipeline.parse_default_css;
        self.document_pipeline.parse_author_css += other.document_pipeline.parse_author_css;
        self.document_pipeline.resolve_css_imports += other.document_pipeline.resolve_css_imports;
        self.document_pipeline.prepare_style_rules += other.document_pipeline.prepare_style_rules;
        self.document_pipeline.resolve_styles += other.document_pipeline.resolve_styles;
        self.document_pipeline.build_layout_inputs += other.document_pipeline.build_layout_inputs;
        self.document_pipeline.rebuild_document_toc += other.document_pipeline.rebuild_document_toc;
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
    use html::layout::LayoutConstraints;
    use html::layout::GlyphShaper;
    use html::pipeline::DocumentFactory;
    use html::resources::{FileSystemProvider, ResourceProvider};
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
    }

    impl GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
        }

        fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, font_weight: u16, font_slant: FontSlant, color: u32, family: Option<&str>) -> Result<GlyphId, html::layout::ShapeError> {
            let _ = (font_weight, font_slant, color, family);
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = glyph_metrics.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout_html(html: &str) -> html::layout::LaidOutDocument {
        let mut factory = DocumentFactory::new();
        let mut glyph_shaper = TestGlyphShaper::default();
        factory.parse_with_new_pipeline(html, None).shape(&mut glyph_shaper).expect("test shaper must register every glyph").layout(LayoutConstraints::new(600.0, 20.0).unwrap())
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
        print_stage("document: dom tree", totals.document_pipeline.build_dom_tree, processed);
        print_stage("document: parse default css", totals.document_pipeline.parse_default_css, processed);
        print_stage("document: parse author css", totals.document_pipeline.parse_author_css, processed);
        print_stage("document: resolve css imports", totals.document_pipeline.resolve_css_imports, processed);
        print_stage("document: prepare style rules", totals.document_pipeline.prepare_style_rules, processed);
        print_stage("document: resolve styles", totals.document_pipeline.resolve_styles, processed);
        print_stage("document: build layout inputs", totals.document_pipeline.build_layout_inputs, processed);
        print_stage("document: rebuild toc", totals.document_pipeline.rebuild_document_toc, processed);
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
        let mut heaviest: Option<(String, usize, Vec<(String, usize)>)> = None;
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
            if heaviest.as_ref().map(|(_, bytes, _)| total > *bytes).unwrap_or(true) {
                heaviest = Some((uri.clone(), total, top_entries));
            }
        }

        let mut aggregate_entries: Vec<(String, usize)> = aggregate.into_iter().collect();
        aggregate_entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        println!("\n=== html-view-core memory summary ===");
        println!("files processed: {processed}   failed: {failed}");
        if let Some((uri, total, top_entries)) = heaviest {
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
