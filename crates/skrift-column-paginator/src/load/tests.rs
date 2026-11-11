use html_engine_model::layout_state;

use html::layout::{BoxId, ImageSizingPolicy, NoteFlow, TextCompositionPolicy};
use html::text::{FontMetrics, ShapedRun, TextRunShapeRequest, ShapedGlyph};
use html::{Document, DocumentOptions, LoadTimings};

use html::resources::{FileSystemProvider, ResourceMetadata, ResourceProvider};
use html::text::GlyphShaper;
use html::text::{FontSlant, GlyphHandle, GlyphResourceGeneration};
use html_dom::NodeRef;
use html_pipeline::DocumentFactory;
use html_pipeline::MarkupSyntax;
use html_pipeline::engine::GlyphResourceStore;
use html_pipeline::engine::{LayoutConstraints, LayoutRequest};
use html_pipeline::parse_document;
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
    let syntax = html_parse::markup_syntax_for_resource(&provider, "chapter.html");
    assert_eq!(syntax, MarkupSyntax::Xhtml);
    let source = String::from_utf8(provider.read_bytes("chapter.html").unwrap()).unwrap();
    let document = parse_document(&source, syntax).unwrap().into_dom();
    let root = document
        .dom_root()
        .and_then(|id| document.try_element_ref(id))
        .unwrap();
    let body = root
        .children()
        .find_map(|id| {
            document
                .try_element_ref(id)
                .filter(|element| element.tag() == "body")
        })
        .unwrap();
    let paragraph = body
        .children()
        .find_map(|id| {
            document
                .try_element_ref(id)
                .filter(|element| element.tag() == "p")
        })
        .unwrap();
    let mut children = paragraph.children();
    let marker = document.try_element_ref(children.next().unwrap()).unwrap();
    assert_eq!(marker.tag(), "a");
    assert!(marker.children().next().is_none());
    assert!(
        matches!(document.node_ref(children.next().unwrap()), NodeRef::Text(text) if text.text() == "Ordinary text")
    );

    assert_eq!(
        html_parse::markup_syntax_for_resource(
            &TypedChapterProvider {
                media_type: Some("text/html")
            },
            "chapter.xhtml"
        ),
        MarkupSyntax::Html
    );
    assert_eq!(
        html_parse::markup_syntax_for_resource(
            &TypedChapterProvider { media_type: None },
            "chapter.xhtml"
        ),
        MarkupSyntax::Xhtml
    );
}

#[derive(Default)]
struct TestGlyphShaper {
    glyphs: HashMap<(char, u32), GlyphHandle>,
    generation: GlyphResourceGeneration,
}

impl GlyphShaper for TestGlyphShaper {
    fn resource_generation(&self) -> GlyphResourceGeneration {
        self.generation
    }

    fn shape_run(&mut self, request: TextRunShapeRequest<'_>) -> ShapedRun {
        ShapedRun::characters(request, |character, style| {
            let key = (character, style.font_size.to_bits());
            let advance = style.font_size * 0.5;
            let metrics = FontMetrics {
                ascent: style.font_size * 0.75,
                descent: style.font_size * 0.25,
            };
            if let Some(&glyph) = self.glyphs.get(&key) {
                return ShapedGlyph::new(glyph, advance, metrics);
            }
            let glyph = GlyphHandle(self.glyphs.len() as u32);
            self.glyphs.insert(key, glyph);
            ShapedGlyph::new(glyph, advance, metrics)
        })
    }
}

fn layout_html(
    html: &str,
    resources: &mut GlyphResourceStore,
) -> html_pipeline::engine::LaidOutDocument<'static> {
    let mut factory = DocumentFactory::new();
    let mut glyph_shaper = TestGlyphShaper::default();
    factory
        .parse_with_new_pipeline(html, None)
        .shape(&mut glyph_shaper, resources)
        .expect("test shaper must register every glyph")
        .layout(LayoutRequest::new(LayoutConstraints::new(600.0, 20.0).unwrap()), &mut glyph_shaper, resources)
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
            super::DocumentLoadSettings {
                root_font_size: 16.0,
                column_width: 600.0,
                reader_overrides: Default::default(),
                image_sizing_policy: ImageSizingPolicy::SmartStandalone,
                text_composition_policy: TextCompositionPolicy::BookOptimized,
                note_display: Default::default(),
            },
        )
    })
    .join()
    .expect("preparation worker should not panic")
    .expect("document should prepare");
    let mut shaper = TestGlyphShaper::default();
    let loaded = prepared
        .finish(html::configuration::DocumentUpdate::default(), &mut shaper)
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
    document: &html_pipeline::engine::LaidOutDocument,
    resources: &GlyphResourceStore,
) -> Vec<BoxId> {
    let boxes = document.render_view(resources).boxes();
    (0..(boxes).len())
        .map(layout_state::box_id_new)
        .filter(|&i| (boxes).tag(i).is_some_and(|tag| tag.eq_ignore_ascii_case("li")))
        .collect()
}

/// Reconstruct a list item's marker text from its marker box's shaped glyphs.
fn marker_string(
    document: &html_pipeline::engine::LaidOutDocument,
    li_box: BoxId,
    resources: &GlyphResourceStore,
) -> String {
    let root = document.render_view(resources);
    let boxes = root.boxes();
    let text = root.text();
    let marker = (boxes).list_marker(li_box).expect("li should have a marker");
    let mut out = String::new();
    for run in (text).marker_runs().only_box(marker.marker_box()) {
        for g in run.glyphs() {
            let glyph_id = (text).glyph_at(g as usize).expect("marker glyph");
            out.push(
                (text).glyph_metric(glyph_id)
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
    use html_pipeline::engine::ListStylePosition;
    let mut document_resources = GlyphResourceStore::default();
    let document = layout_html(
        "<html><body><ul style=\"list-style-position: inside\"><li>Item</li></ul></body></html>",
        &mut document_resources,
    );
    let boxes = document.render_view(&document_resources).boxes();
    let li = li_boxes(&document, &document_resources)[0];
    let marker = (boxes).list_marker(li).expect("marker");
    assert_eq!(marker.position(), ListStylePosition::Inside);
    // The 40px padding-inline-start is on the <ul>, so the <li> is already
    // positioned inside it; the li's own content edge is its box origin.
    // Inside markers sit at that edge rather than hanging to its left.
    let marker_x = (boxes).point(marker.marker_box())
        .expect("marker box should exist")
        .x;
    let content_left = (boxes).point(li)
        .expect("li box should exist")
        .x;
    assert!(
        (marker_x - content_left).abs() < 1.0,
        "inside marker x {marker_x} should be at content edge {content_left}"
    );
}

#[test]
fn list_item_generates_positioned_outside_marker() {
    use html_pipeline::engine::ListStylePosition;
    let mut document_resources = GlyphResourceStore::default();
    let document = layout_html(
        "<html><body><ul><li>Item one</li></ul></body></html>",
        &mut document_resources,
    );
    let boxes = document.render_view(&document_resources).boxes();

    let li_idx = (0..(boxes).len())
        .map(layout_state::box_id_new)
        .find(|&i| (boxes).tag(i).is_some_and(|tag| tag.eq_ignore_ascii_case("li")))
        .expect("li box");
    let marker = (boxes).list_marker(li_idx).expect("li should have a marker");
    assert_eq!(marker.position(), ListStylePosition::Outside);

    let marker_size = (boxes).size(marker.marker_box())
        .expect("marker box should exist");
    assert!(
        marker_size.width > 0.0,
        "marker should have non-zero width, got {marker_size:?}"
    );

    // Outside marker hangs left of the content edge (li content begins after
    // the UA 40px padding-inline-start).
    let marker_x = (boxes).point(marker.marker_box())
        .expect("marker box should exist")
        .x;
    let content_left = (boxes).point(li_idx)
        .expect("li box should exist")
        .x
        + 40.0;
    assert!(
        marker_x < content_left,
        "outside marker x {marker_x} should be left of content edge {content_left}"
    );

    // A Marker run exists and carries glyphs.
    assert!(
        (document.render_view(&document_resources).text()).marker_runs()
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
    let html = "<html><body><a href=\"chapter2.xhtml#target\"><span>Link</span></a></body></html>";
    let mut factory = DocumentFactory::new();
    let mut glyph_shaper = TestGlyphShaper::default();
    let mut document_resources = GlyphResourceStore::default();
    let document = factory
        .parse_with_new_pipeline(html, None)
        .shape(&mut glyph_shaper, &mut document_resources)
        .expect("test shaper must register every glyph")
        .layout(
            LayoutRequest::new(LayoutConstraints::new(600.0, 20.0).unwrap()),
            &mut glyph_shaper,
            &mut document_resources,
        )
        .expect("test layout succeeds");
    let root = document.render_view(&document_resources);
    let boxes = root.boxes();
    let text = root.text();
    let addressing = root.addressing();

    let span_idx = (0..(boxes).len())
        .map(layout_state::box_id_new)
        .find(|&i| (boxes).tag(i).is_some_and(|tag| tag == "span"))
        .expect("span box should exist");
    let mut glyphs = None;
    for run in (text).text_runs().only_box(span_idx) {
        glyphs = Some(run.glyphs());
    }
    let glyphs = glyphs.expect("span text should exist");

    assert!(glyphs.start < glyphs.end, "span text should produce glyphs");
    for glyph_idx in glyphs {
        assert_eq!(
            addressing.link_for_glyph(glyph_idx).map(|link| link.href),
            Some("chapter2.xhtml#target"),
        );
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
    let mut totals = LoadTimings::default();
    let mut processed = 0usize;
    let mut failed = 0usize;

    for path in files {
        let uri = path.to_string_lossy().into_owned();
        let mut glyph_cache = TestGlyphShaper::default();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut options = DocumentOptions::new(600.0)
                .unwrap()
                .with_root_font_size(16.0)
                .unwrap();
            options.image_sizing_policy = ImageSizingPolicy::SmartStandalone;
            options.text_composition_policy = TextCompositionPolicy::BookOptimized;
            options.note_flow = NoteFlow::Excluded;
            Document::load_timed(provider.clone(), &uri, options, &mut glyph_cache)
                .expect("profile document")
        })) {
            Ok((_document, timings)) => {
                totals.resource += timings.resource;
                totals.pipeline += timings.pipeline;
                processed += 1;
                println!(
                    "{}\n  total: {:>8.3} ms  resource: {:>8.3} pipeline: {:>8.3}",
                    path.display(),
                    (timings.resource + timings.pipeline).as_secs_f64() * 1000.0,
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
    println!(
        "total: {:>8.3} ms",
        (totals.resource + totals.pipeline).as_secs_f64() * 1000.0
    );
    print_stage("resource read", totals.resource, processed);
    print_stage("pipeline", totals.pipeline, processed);

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
