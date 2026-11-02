use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use html::engine::{DocumentOptions, Engine};
use html::layout::GlyphShaper;
use html::pipeline::{ImageSizingPolicy, ReaderStyleOverrides};
use html::resources::ResourceProvider;
use html_view_types::NoteDisplay;

/// Compatibility names for the engine's prepared and loaded session states.
/// The engine owns both states; the reader adds only its loading preferences.
pub type LoadedRenderDocument = Engine;
pub type PreparedRenderDocument = Engine;
pub use html::engine::LoadTimings as PipelineTimings;

#[derive(Clone, PartialEq)]
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

    fn into_options(self) -> DocumentOptions {
        DocumentOptions {
            width: self.column_width,
            root_font_size: self.root_font_size,
            reader_overrides: self.reader_overrides,
            image_sizing_policy: self.image_sizing_policy,
            text_composition_policy: self.text_composition_policy,
            note_flow: note_flow_for(self.note_display),
            ..DocumentOptions::new(self.column_width)
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
pub use html::document::markup_syntax_for_resource;

pub fn load_document_with_settings(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
    glyph_shaper: &mut impl GlyphShaper,
) -> LoadedRenderDocument {
    Engine::load(provider, uri, settings.into_options(), glyph_shaper)
        .expect("initial pipeline update should succeed")
}

pub fn load_document_timed(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    root_font_size: f32,
    col_width: f64,
    glyph_shaper: &mut impl GlyphShaper,
) -> (LoadedRenderDocument, PipelineTimings) {
    Engine::load_timed(
        provider,
        uri,
        DocumentLoadSettings::book(root_font_size, col_width).into_options(),
        glyph_shaper,
    )
    .expect("initial pipeline update should succeed")
}

pub fn prepare_document_with_settings(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
) -> Result<PreparedRenderDocument, String> {
    Engine::prepare(provider, uri, settings.into_options()).map_err(|error| error.to_string())
}

pub fn load_document(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    root_font_size: f32,
    col_width: f64,
    glyph_shaper: &mut impl GlyphShaper,
) -> LoadedRenderDocument {
    load_document_with_settings(
        provider,
        uri,
        DocumentLoadSettings::book(root_font_size, col_width),
        glyph_shaper,
    )
}

#[cfg(test)]
mod tests {
    use html::text::{GlyphRequest, ShapedGlyph};

    use html::document::NodeRef;
    use html::layout::GlyphShaper;
    use html::layout::LayoutConstraints;
    use html::layout::{
        FontSlant, GlyphId, GlyphMetric, GlyphResourceGeneration, GlyphResourceStore,
    };
    use html::pipeline::DocumentFactory;
    use html::resources::{FileSystemProvider, ResourceMetadata, ResourceProvider};
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
            Ok(ResourceMetadata {
                media_type: self.media_type.map(str::to_owned),
                charset: None,
            })
        }

        fn exists(&self, _uri: &str) -> bool {
            true
        }
        fn resolve(&self, _base: &str, href: &str) -> String {
            href.to_owned()
        }
        fn list_html_candidates(&self, _root: &str) -> std::io::Result<Vec<String>> {
            Ok(vec!["chapter.html".to_owned()])
        }
    }

    #[test]
    fn declared_xhtml_keeps_self_closing_page_marker_empty_in_html_named_chapter() {
        let provider = TypedChapterProvider {
            media_type: Some("application/xhtml+xml"),
        };
        let syntax = super::markup_syntax_for_resource(&provider, "chapter.html");
        assert_eq!(syntax, html::parse::MarkupSyntax::Xhtml);
        let source = String::from_utf8(provider.read_bytes("chapter.html").unwrap()).unwrap();
        let document = html::parse::parse_document(&source, syntax)
            .unwrap()
            .build_dom();
        let root = document.element_ref(document.dom_root().unwrap()).unwrap();
        let body = root
            .children()
            .find_map(|id| {
                document
                    .element_ref(id)
                    .filter(|element| element.tag() == "body")
            })
            .unwrap();
        let paragraph = body
            .children()
            .find_map(|id| {
                document
                    .element_ref(id)
                    .filter(|element| element.tag() == "p")
            })
            .unwrap();
        let mut children = paragraph.children();
        let marker = document.element_ref(children.next().unwrap()).unwrap();
        assert_eq!(marker.tag(), "a");
        assert!(marker.children().next().is_none());
        assert!(
            matches!(document.node_ref(children.next().unwrap()), Some(NodeRef::Text(text)) if text.text() == "Ordinary text")
        );

        assert_eq!(
            super::markup_syntax_for_resource(
                &TypedChapterProvider {
                    media_type: Some("text/html")
                },
                "chapter.xhtml"
            ),
            html::parse::MarkupSyntax::Html
        );
        assert_eq!(
            super::markup_syntax_for_resource(
                &TypedChapterProvider { media_type: None },
                "chapter.xhtml"
            ),
            html::parse::MarkupSyntax::Xhtml
        );
    }

    #[derive(Default)]
    struct TestGlyphShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        generation: GlyphResourceGeneration,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceGeneration)>,
    }

    struct DocumentMemoryProfile {
        uri: String,
        total: usize,
        top_entries: Vec<(String, usize)>,
    }

    impl GlyphShaper for TestGlyphShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.generation = GlyphResourceGeneration::default();
        }

        fn resource_generation(&self) -> GlyphResourceGeneration {
            self.generation
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::layout::ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.generation.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.generation = glyph_store;
            }
        }

        fn shape_glyph(
            &mut self,
            request: GlyphRequest<'_>,
        ) -> Result<ShapedGlyph, html::layout::ShapeError> {
            let key = (request.character, request.font_size.to_bits());
            let metric = GlyphMetric::try_new(
                request.character,
                request.font_size * 0.5,
                request.font_size * 0.75,
                request.font_size * 0.25,
            )?;
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(ShapedGlyph::from_metric(glyph, metric));
            }
            let glyph = self.glyphs.len() as GlyphId;
            self.glyphs.insert(key, glyph);
            Ok(ShapedGlyph::from_metric(glyph, metric))
        }
    }

    fn layout_html(
        html: &str,
        resources: &mut GlyphResourceStore,
    ) -> html::layout::LaidOutDocument<'static> {
        let mut factory = DocumentFactory::new();
        let mut glyph_shaper = TestGlyphShaper::default();
        factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut glyph_shaper, resources)
            .expect("test shaper must register every glyph")
            .layout(LayoutConstraints::new(600.0, 20.0).unwrap(), resources)
            .expect("test layout succeeds")
    }

    #[test]
    fn initial_preparation_crosses_a_worker_boundary_before_platform_shaping() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("html-view-preparation-{nonce}.html"));
        fs::write(&path, "<html><head><style>p { color: red }</style></head><body><p>Prepared text</p></body></html>").unwrap();
        let uri = path.to_string_lossy().into_owned();
        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let prepared = std::thread::spawn(move || {
            super::prepare_document_with_settings(
                provider,
                &uri,
                super::DocumentLoadSettings::book(16.0, 600.0),
            )
        })
        .join()
        .expect("preparation worker should not panic")
        .expect("document should prepare");
        let mut shaper = TestGlyphShaper::default();
        let mut loaded = prepared;
        loaded
            .rehydrate_glyphs(&mut shaper)
            .expect("platform shaping should finish");
        assert!(
            loaded
                .render_view()
                .expect("loaded document has glyph resources")
                .text()
                .glyph_count()
                > 0
        );
        let _ = fs::remove_file(path);
    }

    fn li_boxes(
        document: &html::layout::LaidOutDocument,
        resources: &GlyphResourceStore,
    ) -> Vec<usize> {
        let boxes = document.render_view(resources).boxes();
        (0..boxes.len())
            .filter(|&i| {
                boxes
                    .tag(i)
                    .is_some_and(|tag| tag.eq_ignore_ascii_case("li"))
            })
            .collect()
    }

    /// Reconstruct a list item's marker text from its marker box's shaped glyphs.
    fn marker_string(
        document: &html::layout::LaidOutDocument,
        li_box: usize,
        resources: &GlyphResourceStore,
    ) -> String {
        let root = document.render_view(resources);
        let boxes = root.boxes();
        let text = root.text();
        let marker = boxes.list_marker(li_box).expect("li should have a marker");
        let mut out = String::new();
        for run in text.marker_runs().only_box(marker.marker_box()) {
            for g in run.glyphs() {
                let glyph_id = text.glyph_at(g as usize).expect("marker glyph");
                out.push(
                    text.glyph_metric(glyph_id)
                        .expect("registered marker glyph")
                        .ch(),
                );
            }
        }
        out
    }

    #[test]
    fn ordered_list_counts_with_value_overrides() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><ol><li>a</li><li>b</li><li value=\"10\">c</li><li>d</li></ol></body></html>",
            &mut document_resources,
        );
        let li = li_boxes(&document, &document_resources);
        assert_eq!(marker_string(&document, li[0], &document_resources), "1.");
        assert_eq!(marker_string(&document, li[1], &document_resources), "2.");
        assert_eq!(marker_string(&document, li[2], &document_resources), "10.");
        assert_eq!(marker_string(&document, li[3], &document_resources), "11.");
    }

    #[test]
    fn ordered_list_start_and_reversed() {
        let mut start_resources = GlyphResourceStore::default();
        let start = layout_html(
            "<html><body><ol start=\"5\"><li>a</li><li>b</li></ol></body></html>",
            &mut start_resources,
        );
        let li = li_boxes(&start, &start_resources);
        assert_eq!(marker_string(&start, li[0], &start_resources), "5.");
        assert_eq!(marker_string(&start, li[1], &start_resources), "6.");

        let mut rev_resources = GlyphResourceStore::default();
        let rev = layout_html(
            "<html><body><ol reversed><li>a</li><li>b</li><li>c</li></ol></body></html>",
            &mut rev_resources,
        );
        let li = li_boxes(&rev, &rev_resources);
        assert_eq!(marker_string(&rev, li[0], &rev_resources), "3.");
        assert_eq!(marker_string(&rev, li[1], &rev_resources), "2.");
        assert_eq!(marker_string(&rev, li[2], &rev_resources), "1.");
    }

    #[test]
    fn nested_unordered_bullets_by_depth() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li></ul></body></html>",
            &mut document_resources,
        );
        let li = li_boxes(&document, &document_resources);
        assert_eq!(
            marker_string(&document, li[0], &document_resources),
            "\u{2022}"
        ); // disc
        assert_eq!(
            marker_string(&document, li[1], &document_resources),
            "\u{25E6}"
        ); // circle
        assert_eq!(
            marker_string(&document, li[2], &document_resources),
            "\u{25AA}"
        ); // square
    }

    #[test]
    fn inside_marker_sits_at_content_edge() {
        use html::layout::ListStylePosition;
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><ul style=\"list-style-position: inside\"><li>Item</li></ul></body></html>",
            &mut document_resources,
        );
        let boxes = document.render_view(&document_resources).boxes();
        let li = li_boxes(&document, &document_resources)[0];
        let marker = boxes.list_marker(li).expect("marker");
        assert_eq!(marker.position(), ListStylePosition::Inside);
        // The 40px padding-inline-start is on the <ul>, so the <li> is already
        // positioned inside it; the li's own content edge is its box origin.
        // Inside markers sit at that edge rather than hanging to its left.
        let marker_x = boxes
            .point(marker.marker_box())
            .expect("marker box should exist")
            .x;
        let content_left = boxes.point(li).expect("li box should exist").x;
        assert!(
            (marker_x - content_left).abs() < 1.0,
            "inside marker x {marker_x} should be at content edge {content_left}"
        );
    }

    #[test]
    fn list_item_generates_positioned_outside_marker() {
        use html::layout::ListStylePosition;
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><ul><li>Item one</li></ul></body></html>",
            &mut document_resources,
        );
        let boxes = document.render_view(&document_resources).boxes();

        let li_idx = (0..boxes.len())
            .find(|&i| {
                boxes
                    .tag(i)
                    .is_some_and(|tag| tag.eq_ignore_ascii_case("li"))
            })
            .expect("li box");
        let marker = boxes.list_marker(li_idx).expect("li should have a marker");
        assert_eq!(marker.position(), ListStylePosition::Outside);

        let marker_size = boxes
            .size(marker.marker_box())
            .expect("marker box should exist");
        assert!(
            marker_size.width > 0.0,
            "marker should have non-zero width, got {marker_size:?}"
        );

        // Outside marker hangs left of the content edge (li content begins after
        // the UA 40px padding-inline-start).
        let marker_x = boxes
            .point(marker.marker_box())
            .expect("marker box should exist")
            .x;
        let content_left = boxes.point(li_idx).expect("li box should exist").x + 40.0;
        assert!(
            marker_x < content_left,
            "outside marker x {marker_x} should be left of content edge {content_left}"
        );

        // A Marker run exists and carries glyphs.
        assert!(
            document
                .render_view(&document_resources)
                .text()
                .marker_runs()
                .into_iter()
                .any(|run| {
                    let glyphs = run.glyphs();
                    glyphs.start < glyphs.end
                }),
            "a non-empty Marker run should exist"
        );
    }

    #[test]
    fn nested_inline_text_in_anchor_maps_glyphs_to_href() {
        let html =
            "<html><body><a href=\"chapter2.xhtml#target\"><span>Link</span></a></body></html>";
        let mut factory = DocumentFactory::new();
        let mut glyph_shaper = TestGlyphShaper::default();
        let mut document_resources = GlyphResourceStore::default();
        let document = factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut glyph_shaper, &mut document_resources)
            .expect("test shaper must register every glyph")
            .layout(
                LayoutConstraints::new(600.0, 20.0).unwrap(),
                &document_resources,
            )
            .expect("test layout succeeds");
        let root = document.render_view(&document_resources);
        let boxes = root.boxes();
        let text = root.text();
        let addressing = root.addressing();

        let href_idx = root
            .lookup_string("chapter2.xhtml#target")
            .expect("href should be interned");
        let span_idx = (0..boxes.len())
            .find(|&i| boxes.tag(i).is_some_and(|tag| tag == "span"))
            .expect("span box should exist");
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

        assert!(
            !files.is_empty(),
            "no HTML files found under {}",
            testdata.display()
        );

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut totals = super::PipelineTimings::default();
        let mut processed = 0usize;
        let mut failed = 0usize;

        for path in files {
            let uri = path.to_string_lossy().into_owned();
            let mut glyph_cache = TestGlyphShaper::default();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::load_document_timed(provider.clone(), &uri, 16.0, 600.0, &mut glyph_cache)
            })) {
                Ok((_document, timings)) => {
                    totals.add_assign(&timings);
                    processed += 1;
                    println!(
                        "{}\n  total: {:>8.3} ms  resource: {:>8.3} pipeline: {:>8.3}",
                        path.display(),
                        timings.total().as_secs_f64() * 1000.0,
                        timings.resource.as_secs_f64() * 1000.0,
                        timings.pipeline.as_secs_f64() * 1000.0
                    );
                }
                Err(_) => {
                    failed += 1;
                    eprintln!("{}: pipeline panicked", path.display());
                }
            }
        }

        println!("\n=== skrift-column-paginator pipeline timing summary ===");
        println!("files processed: {processed}   failed: {failed}");
        println!("total: {:>8.3} ms", totals.total().as_secs_f64() * 1000.0);
        print_stage("resource read", totals.resource, processed);
        print_stage("pipeline", totals.pipeline, processed);

        assert!(
            processed > 0,
            "expected at least one HTML file to be processed"
        );
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::load_document(provider.clone(), &uri, 16.0, 600.0, &mut glyph_shaper)
            }));
            let document = match result {
                Ok(document) => document,
                Err(_) => {
                    failed += 1;
                    eprintln!("{uri}: renderer panicked");
                    continue;
                }
            };

            let mut report = document.document().unwrap().memory_usage_report();
            report.sort_by_bytes_desc();
            let total = report.total_bytes();
            processed += 1;

            for entry in &report.entries {
                *aggregate.entry(entry.label.clone()).or_insert(0) += entry.bytes;
            }

            let top_entries: Vec<(String, usize)> = report
                .entries
                .iter()
                .take(10)
                .map(|entry| (entry.label.clone(), entry.bytes))
                .collect();
            if heaviest
                .as_ref()
                .is_none_or(|profile| total > profile.total)
            {
                heaviest = Some(DocumentMemoryProfile {
                    uri: uri.clone(),
                    total,
                    top_entries,
                });
            }
        }

        let mut aggregate_entries: Vec<(String, usize)> = aggregate.into_iter().collect();
        aggregate_entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        println!("\n=== skrift-column-paginator memory summary ===");
        println!("files processed: {processed}   failed: {failed}");
        if let Some(DocumentMemoryProfile {
            uri,
            total,
            top_entries,
        }) = heaviest
        {
            println!(
                "heaviest document: {uri} ({:.2} KiB)",
                total as f64 / 1024.0
            );
            for (label, bytes) in top_entries {
                println!(
                    "  {:>12}  {}",
                    format!("{:.2} KiB", bytes as f64 / 1024.0),
                    label
                );
            }
        }
        println!("\nTop aggregate memory consumers:");
        for (label, bytes) in aggregate_entries.into_iter().take(25) {
            println!(
                "{:>12}  {}",
                format!("{:.2} KiB", bytes as f64 / 1024.0),
                label
            );
        }

        assert!(
            processed > 0,
            "expected at least one HTML file to be processed"
        );
    }

    fn profile_corpus() -> PathBuf {
        std::env::var_os("HTML_VIEW_PROFILE_CORPUS")
            .map(PathBuf::from)
            .expect("set HTML_VIEW_PROFILE_CORPUS to a directory containing HTML documents")
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
            let is_html = matches!(
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .as_deref(),
                Some("html") | Some("htm") | Some("xhtml")
            );
            if is_html {
                out.push(path);
            }
        }
    }

    fn print_stage(label: &str, total: Duration, processed: usize) {
        let per_file = total.as_secs_f64() * 1000.0 / processed.max(1) as f64;
        println!(
            "{label:<24} {:>8.3} ms total   {:>8.3} ms/file",
            total.as_secs_f64() * 1000.0,
            per_file
        );
    }
}
