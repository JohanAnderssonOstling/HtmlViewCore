use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use kurbo::{Point, Size};

use crate::{FrameDisplayCacheKeys, GlyphShaper, InteractionPalette, LaidOutDocument, Painter, RendererCommand, RendererCore, RendererHost, RendererOperation, RendererRevisions, ResourceProvider, TocEntry};

/// The high-level renderer API used by UI backends.
///
/// A session owns both the renderer state and its glyph shaper so callers do
/// not have to coordinate them for navigation, relayout, or link activation.
pub struct RendererSession<S> {
    core: RendererCore,
    glyph_shaper: S,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerDownOptions {
    /// Promote intersected semantic structures, currently table cells, while
    /// retaining ordinary text-range selection everywhere else.
    pub semantic_selection: bool,
    pub copy_image: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerMoveOutcome {
    pub handled: bool,
    pub over_link: bool,
    pub link_hover_changed: bool,
}

impl<S: GlyphShaper> RendererSession<S> {
    pub(crate) fn from_core(core: RendererCore, glyph_shaper: S) -> Self {
        Self { core, glyph_shaper }
    }

    /// Paints a note from a [`FootnotePreview`] for a host showing it in a
    /// popup. The note was laid out to the width the host reported through
    /// [`RendererHost::note_popup_width`], so it is drawn at its own layout
    /// positions with no viewport or pagination applied.
    ///
    /// [`FootnotePreview`]: crate::FootnotePreview
    /// [`RendererHost::note_popup_width`]: crate::RendererHost::note_popup_width
    pub fn paint_note(&self, note: &crate::LaidOutDocument, painter: &mut impl crate::Painter) {
        self.core.paint_note(note, painter);
    }

    pub fn from_provider_with_nav(host: Rc<dyn RendererHost>, mut glyph_shaper: S, provider: Arc<dyn ResourceProvider>, document_uris: Vec<String>, start_index: usize, nav_state: Option<&str>, config: crate::RendererInitialConfig) -> Self {
        let started = Instant::now();
        let core = RendererCore::from_provider_with_nav(host, &mut glyph_shaper, provider, document_uris, start_index, nav_state, config);
        println!("HTML_CORE_STARTUP phase=core_ready elapsed_ms={}", started.elapsed().as_millis());
        let session = Self::from_core(core, glyph_shaper);
        println!("HTML_CORE_STARTUP phase=session_ready elapsed_ms={}", started.elapsed().as_millis());
        session
    }

    pub fn document(&self) -> &LaidOutDocument {
        self.core.document()
    }

    pub fn glyph_shaper(&self) -> &S {
        &self.glyph_shaper
    }

    pub fn revisions(&self) -> RendererRevisions {
        self.core.revisions()
    }

    /// Returns whether visible resources are still being loaded or decoded.
    pub fn has_pending_resources(&self) -> bool {
        self.core.has_pending_resources()
    }

    pub fn emit_state_snapshot(&mut self) {
        self.core.emit_state_snapshot();
    }

    pub fn apply(&mut self, command: RendererCommand) {
        self.core.apply(&mut self.glyph_shaper, command);
    }

    pub fn toc(&self) -> std::io::Result<Option<Vec<TocEntry>>> {
        self.core.toc()
    }

    pub fn document_toc(&self) -> Vec<TocEntry> {
        self.core.document_toc()
    }

    pub fn set_toc_anchor_strings_by_doc(&mut self, anchors_by_doc: Vec<Vec<String>>) {
        self.core.set_toc_anchor_strings_by_doc(anchors_by_doc);
    }

    pub fn current_cfi(&self) -> Option<String> {
        self.core.current_cfi()
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.core.resolve_href(href)
    }

    pub fn navigate_to_href(&mut self, href: &str) -> bool {
        self.core.navigate_to_href(&mut self.glyph_shaper, href)
    }

    pub fn navigate_history_back(&mut self) -> bool {
        self.core.navigate_history_back(&mut self.glyph_shaper)
    }

    pub fn navigate_history_forward(&mut self) -> bool {
        self.core.navigate_history_forward(&mut self.glyph_shaper)
    }

    pub fn set_position(&mut self, document: usize, glyph: Option<u32>) {
        self.core.set_position(&mut self.glyph_shaper, document, glyph);
    }

    pub fn set_cfi_position(&mut self, cfi: &str) -> bool {
        self.core.set_cfi_position(cfi)
    }

    pub fn restore_position_from_cfi(&mut self, cfi: &str) -> bool {
        self.set_cfi_position(cfi)
    }

    pub fn next_page(&mut self) {
        self.core.next_page(&mut self.glyph_shaper);
    }

    pub fn previous_page(&mut self) {
        self.core.prev_page(&mut self.glyph_shaper);
    }

    pub fn next_line(&mut self) {
        self.core.next_line();
    }

    pub fn previous_line(&mut self) {
        self.core.prev_line();
    }

    pub fn scroll_vertical(&mut self, delta_y: f64) -> bool {
        if delta_y < 0.0 {
            self.previous_line();
            true
        } else if delta_y > 0.0 {
            self.next_line();
            true
        } else {
            false
        }
    }

    pub fn next_document(&mut self) {
        self.core.next_document(&mut self.glyph_shaper);
    }

    pub fn previous_document(&mut self) {
        self.core.prev_document(&mut self.glyph_shaper);
    }

    pub fn root_font_size(&self) -> f32 {
        self.core.root_font_size()
    }

    pub fn set_root_font_size(&mut self, size: f32) {
        self.core.set_root_font_size(&mut self.glyph_shaper, size);
    }

    pub fn change_root_font_size(&mut self, delta: f32) {
        self.set_root_font_size(self.root_font_size() + delta);
    }

    pub fn column_width(&self) -> f64 {
        self.core.preferred_column_width()
    }

    pub fn set_column_width(&mut self, width: f64) {
        self.core.set_col_width(&mut self.glyph_shaper, width);
    }

    pub fn change_column_width(&mut self, delta: f64) {
        self.set_column_width(self.column_width() + delta);
    }

    pub fn scale(&self) -> f64 {
        self.core.scale()
    }

    pub fn set_scale(&mut self, scale: f64) {
        self.core.set_scale(scale);
    }

    pub fn change_scale(&mut self, delta: f64) {
        self.set_scale(self.scale() + delta);
    }

    pub fn search_active(&self) -> bool {
        self.core.search_active()
    }

    pub fn set_search_active(&mut self, active: bool) {
        self.core.set_search_active(active);
    }

    pub fn toggle_search(&mut self) {
        self.set_search_active(!self.search_active());
    }

    pub fn set_search_query(&mut self, query: &str) {
        self.core.set_search_query(query);
    }

    pub fn next_match(&mut self) {
        self.core.next_match(&mut self.glyph_shaper);
    }

    pub fn previous_match(&mut self) {
        self.core.prev_match(&mut self.glyph_shaper);
    }

    pub fn copy_selection(&self) -> bool {
        let result = if self.core.table_selection_active() { self.core.copy_table_selection() } else { self.core.copy_selection_to_clipboard() };
        match result {
            Ok(handled) => handled,
            Err(message) => {
                self.core.emit_operation_failed(RendererOperation::CopySelection, message);
                true
            }
        }
    }

    pub fn image_at(&self, position: Point) -> bool {
        self.core.hit_test_image(position).is_some()
    }

    pub fn copy_image_at(&self, position: Point) -> bool {
        let Some(image) = self.core.hit_test_image(position) else {
            return false;
        };
        if let Err(message) = self.core.copy_image_to_clipboard(image) {
            self.core.emit_operation_failed(RendererOperation::CopyImage, message);
        }
        true
    }

    /// Starts the complete primary-pointer interaction. Image copying takes
    /// precedence over text selection when `copy_image` is true.
    pub fn pointer_down(&mut self, position: Point, options: PointerDownOptions) -> bool {
        if self.core.table_selection_active() {
            if self.core.begin_table_selection_drag(position) {
                return true;
            }
            self.core.clear_table_selection();
        }
        if options.copy_image
            && let Some(image) = self.core.hit_test_image(position)
        {
            if let Err(message) = self.core.copy_image_to_clipboard(image) {
                self.core.emit_operation_failed(RendererOperation::CopyImage, message);
            }
            return true;
        }
        if self.core.open_image_at(position) {
            return true;
        }
        self.core.begin_selection_at(position, options.semantic_selection)
    }

    /// Updates both link hover state and an active selection.
    pub fn pointer_move(&mut self, position: Point, semantic_selection: bool) -> PointerMoveOutcome {
        let was_over_link = self.core.link_cursor_active();
        let over_link = self.core.link_at(position);
        self.core.set_link_cursor_active(over_link);
        let handled = self.core.update_table_selection_drag(position) || self.core.update_selection_at(position, semantic_selection);
        PointerMoveOutcome { handled, over_link, link_hover_changed: was_over_link != over_link }
    }

    /// Finishes selection and activates a link if the interaction remained a click.
    pub fn pointer_up(&mut self) -> bool {
        if self.core.finish_table_selection_drag() {
            return true;
        }
        let Some(glyph) = self.core.finish_selection() else {
            return false;
        };
        if self.core.activate_annotation_at_glyph(glyph) {
            return true;
        }
        self.core.handle_link_click(&mut self.glyph_shaper, glyph)
    }

    pub fn pointer_leave(&mut self) {
        self.core.set_link_cursor_active(false);
    }

    pub fn link_cursor_active(&self) -> bool {
        self.core.link_cursor_active()
    }

    pub fn origin(&self) -> Point {
        self.core.origin()
    }

    pub fn set_origin(&mut self, origin: Point) {
        self.core.set_origin(origin);
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette) {
        self.core.set_interaction_palette(palette);
    }

    pub fn to_layout_point(&self, position: Point) -> Point {
        self.core.to_layout_point(position)
    }

    pub fn selection_contains_point(&self, position: Point) -> bool {
        self.core.selection_contains_point(position)
    }

    pub fn table_at(&self, position: Point) -> bool {
        self.core.table_at(position)
    }

    pub fn table_selection_at(&self, position: Point) -> bool {
        self.core.table_selection_at(position)
    }

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        self.core.begin_table_selection_at(position)
    }

    pub fn clear_table_selection(&mut self) -> bool {
        self.core.clear_table_selection()
    }

    pub fn copy_table_at(&self, position: Point) -> bool {
        match self.core.copy_table_at(position) {
            Ok(handled) => handled,
            Err(message) => {
                self.core.emit_operation_failed(RendererOperation::CopySelection, message);
                true
            }
        }
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> bool {
        match self.core.copy_table_unstyled_html_at(position) {
            Ok(handled) => handled,
            Err(message) => {
                self.core.emit_operation_failed(RendererOperation::CopySelection, message);
                true
            }
        }
    }

    pub fn copy_table_styled_html_at(&self, position: Point) -> bool {
        match self.core.copy_table_styled_html_at(position) {
            Ok(handled) => handled,
            Err(message) => {
                self.core.emit_operation_failed(RendererOperation::CopySelection, message);
                true
            }
        }
    }

    pub fn copy_table_selection(&self) -> bool {
        match self.core.copy_table_selection() {
            Ok(handled) => handled,
            Err(message) => {
                self.core.emit_operation_failed(RendererOperation::CopySelection, message);
                true
            }
        }
    }

    /// Prepares layout and returns a frame that can be painted exactly once.
    pub fn prepare_frame(&mut self, viewport_size: Size) -> PreparedFrame<'_, S> {
        self.core.prepare_frame(&mut self.glyph_shaper, viewport_size);
        PreparedFrame { core: &mut self.core, glyph_shaper: &self.glyph_shaper }
    }
}

pub struct PreparedFrame<'a, S> {
    core: &'a mut RendererCore,
    glyph_shaper: &'a S,
}

impl<'a, S> PreparedFrame<'a, S> {
    pub fn glyph_shaper(&self) -> &S {
        self.glyph_shaper
    }

    pub fn paint(self, painter: &mut impl Painter) {
        self.core.paint_frame(painter);
    }

    pub fn cache_keys(&self) -> FrameDisplayCacheKeys {
        self.core.display_cache_keys()
    }

    /// Emits the frame into independently cacheable layers while preserving
    /// the renderer's established z-order.
    pub fn paint_layers(self, base_before_overlay: &mut impl Painter, overlay: &mut impl Painter, base_after_overlay: &mut impl Painter, paint_base: bool, paint_overlay: bool) {
        if paint_base {
            self.core.paint_base_before_overlay(base_before_overlay);
        }
        if paint_overlay {
            self.core.paint_interaction_overlay(overlay);
        }
        if paint_base {
            self.core.paint_base_after_overlay(base_after_overlay);
        }
    }

    pub fn into_parts(self) -> (&'a S, FramePainter<'a>) {
        (self.glyph_shaper, FramePainter { core: self.core })
    }
}

/// The paint-only half of a prepared frame. This lets a backend construct a
/// painter that borrows the glyph shaper without exposing renderer internals.
pub struct FramePainter<'a> {
    core: &'a mut RendererCore,
}

impl FramePainter<'_> {
    pub fn paint(self, painter: &mut impl Painter) {
        self.core.paint_frame(painter);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::text_backend::{CharacterPlacement, FontSlant, GlyphId, GlyphMetric, GlyphRegistry, ShapedLine, TextShapeRequest};
    use crate::{FileSystemProvider, Painter, RecordingPainter, RendererEvent};
    use kurbo::Rect;
    use peniko::{Color, Image};

    use super::*;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        line_shape_calls: usize,
        line_requests: Vec<CapturedLineRequest>,
        reject_lines: bool,
        active_line_runs: HashMap<u32, usize>,
    }

    struct CapturedLineRequest {
        text: String,
        font_sizes: Vec<f32>,
        placements: Vec<CharacterPlacement>,
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
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(crate::layout::ShapeError::rejected_metric)?;
            let glyph = glyph_metrics.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }

        fn shape_line(&mut self, request: TextShapeRequest<'_>) -> Result<Option<ShapedLine>, crate::layout::ShapeError> {
            self.line_shape_calls += 1;
            self.line_requests.push(CapturedLineRequest { text: request.text().to_owned(), font_sizes: request.styles().iter().map(|style| style.font_size()).collect(), placements: request.placements().to_vec() });
            if self.reject_lines {
                return Ok(None);
            }
            let raw_caret_stops = (0..=request.text().chars().count()).map(|index| index as f32 * 8.0).collect::<Vec<_>>();
            let caret_stops = request.adjusted_caret_stops(&raw_caret_stops).expect("test placements are valid");
            let cluster_boundaries = vec![true; caret_stops.len()].into();
            let run = self.active_line_runs.len() as u32;
            self.active_line_runs.insert(run, request.line_index());
            Ok(Some(ShapedLine { line_index: request.line_index(), text_range: request.text_range(), run, ascent: 12.0, caret_stops, cluster_boundaries }))
        }

        fn begin_line_shaping(&mut self) {
            self.active_line_runs.clear();
        }
    }

    #[derive(Default)]
    struct NativeFallbackPainter {
        recording: RecordingPainter,
        text_runs: usize,
    }

    impl Painter for NativeFallbackPainter {
        fn fill_rect(&mut self, rect: Rect, color: Color) {
            self.recording.fill_rect(rect, color);
        }

        fn draw_glyph(&mut self, glyph: GlyphId, origin: Point) {
            self.recording.draw_glyph(glyph, origin);
        }

        fn supports_text_runs(&self) -> bool {
            true
        }

        fn draw_text_run(&mut self, _run: u32, _origin: Point, _color: Option<u32>) {
            self.text_runs += 1;
        }

        fn draw_image(&mut self, image: &Image, hash: &[u8], rect: Rect) {
            self.recording.draw_image(image, hash, rect);
        }
    }

    #[derive(Default)]
    struct TestHost {
        events: RefCell<Vec<RendererEvent>>,
        repaints: Cell<usize>,
        fail_clipboard: Cell<bool>,
    }

    impl RendererHost for TestHost {
        fn request_repaint(&self) {
            self.repaints.set(self.repaints.get() + 1);
        }

        fn request_style(&self) {}

        fn schedule(&self, _delay: Duration, callback: Box<dyn FnOnce() + Send>) {
            callback();
        }

        fn schedule_repaint(&self, _delay: Duration) {}

        fn set_clipboard(&self, _text: &str) -> Result<(), String> {
            if self.fail_clipboard.get() { Err("test clipboard failure".to_owned()) } else { Ok(()) }
        }

        fn set_clipboard_image(&self, _width: usize, _height: usize, _rgba: Vec<u8>) -> Result<(), String> {
            Ok(())
        }

        fn emit(&self, event: RendererEvent) {
            self.events.borrow_mut().push(event);
        }
    }

    #[test]
    fn session_owns_navigation_shaping_and_pointer_choreography() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-session-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let first = fixture.join("one.html");
        let second = fixture.join("two.html");
        std::fs::write(&first, "<html><body><a href=\"two.html#target\">Next</a></body></html>").unwrap();
        std::fs::write(&second, "<html><body><h1 id=\"target\">Target</h1></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let uris = vec![first.to_string_lossy().into_owned(), second.to_string_lossy().into_owned()];
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host.clone(), TestShaper::default(), provider, uris, 0, Some("0:0"), crate::RendererInitialConfig::default());

        renderer.prepare_frame(Size::new(600.0, 800.0)).paint(&mut RecordingPainter::default());
        let root = renderer.core.document.render_view();
        let text = root.text();
        let addressing = root.addressing();
        let linked_glyph = (0..text.glyph_count() as u32).find(|glyph| addressing.link_for_glyph(*glyph).is_some()).expect("fixture should contain a linked glyph");
        let line_idx = renderer.core.doc().find_line_for_glyph(linked_glyph).unwrap();
        let screen = renderer.core.frame.last_line_positions.get(line_idx).unwrap();
        let line = renderer.core.document.render_view().text().line(line_idx).expect("linked line should be present");
        let position = Point::new(screen.point.x + renderer.core.text_geometry().glyph_x_in_line(&line, linked_glyph) + 0.1, screen.point.y + line.height() / 2.0);

        let hover = renderer.pointer_move(position, false);
        assert!(hover.over_link);
        assert!(hover.link_hover_changed);
        assert!(renderer.pointer_down(position, PointerDownOptions::default()));
        renderer.core.selection.selection_active = Some(linked_glyph + 1);
        renderer.core.update_selection_text();
        assert!(renderer.selection_contains_point(position));
        assert!(!renderer.selection_contains_point(Point::new(position.x, position.y + line.height() * 2.0)));
        renderer.core.selection.selection_active = Some(linked_glyph);
        renderer.core.update_selection_text();
        renderer.core.selection.selected_text_markdown = Some("Next".to_owned());
        host.fail_clipboard.set(true);
        assert!(renderer.copy_selection());
        assert!(host.events.borrow().iter().any(|event| matches!(
            event,
            RendererEvent::OperationFailed { operation: RendererOperation::CopySelection, message }
                if message == "test clipboard failure"
        )));
        host.fail_clipboard.set(false);
        assert!(renderer.pointer_up());
        assert_eq!(renderer.core.nav.current_doc_index, 1);

        let font_size = renderer.root_font_size();
        let column_width = renderer.column_width();
        let scale = renderer.scale();
        renderer.change_root_font_size(2.0);
        renderer.change_column_width(-20.0);
        renderer.change_scale(0.1);
        assert_eq!(renderer.root_font_size(), font_size + 2.0);
        assert_eq!(renderer.column_width(), column_width - 20.0);
        assert_eq!(renderer.scale(), scale + 0.1);
        assert!(!renderer.scroll_vertical(0.0));

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn viewport_relayout_paints_descendant_percentage_height() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-percentage-height-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.xht");
        std::fs::write(
            &document,
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><style><![CDATA[
                html { background-color: red; height: 100%; }
                body, p { height: 100%; margin: 0px; }
                p { background-color: green; color: white; }
            ]]></style></head><body><p>Test passes if the background of this page is green and if there is <strong>no red and no vertical scrollbar</strong>.</p></body></html>"#,
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        renderer.apply(RendererCommand::SetFontSize(16.0));
        renderer.apply(RendererCommand::SetColumnWidth(800.0));
        renderer.apply(RendererCommand::SetMaxColumnCount(Some(1)));
        renderer.apply(RendererCommand::SetImageSizingPolicy(html::pipeline::ImageSizingPolicy::WebCompatible));
        renderer.apply(RendererCommand::SetTextCompositionPolicy(html::pipeline::TextCompositionPolicy::WebCompatible));
        let mut painter = RecordingPainter::default();
        renderer.prepare_frame(Size::new(800.0, 600.0)).paint(&mut painter);

        assert!(painter.fills.iter().any(|(rect, color)| *color == Color::rgba8(0, 128, 0, 255) && rect.height() == 600.0), "fills={:?}", painter.fills);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn positioned_background_paints_after_an_earlier_inline_image() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-positioned-cover-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.xht");
        std::fs::write(
            &document,
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><style><![CDATA[
                div.a { width:300px; height:100px; background:green }
                div.b { width:300px; position:relative } div.b p { width:200% }
                div.b img { width:50%; height:100px }
                div.b span { position:absolute; top:0; left:0; background:green; width:300px; height:100px }
            ]]></style></head><body><p>preceding flow</p><div class="a"></div><div class="b"><p><img/><span></span></p></div></body></html>"#,
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        renderer.apply(RendererCommand::SetColumnWidth(800.0));
        renderer.apply(RendererCommand::SetMaxColumnCount(Some(1)));
        renderer.apply(RendererCommand::SetImageSizingPolicy(html::pipeline::ImageSizingPolicy::WebCompatible));
        renderer.apply(RendererCommand::SetTextCompositionPolicy(html::pipeline::TextCompositionPolicy::WebCompatible));
        let mut painter = RecordingPainter::default();
        renderer.prepare_frame(Size::new(800.0, 600.0)).paint(&mut painter);

        assert!(
            painter.fills.iter().any(|(rect, color)| { *color == Color::rgba8(0, 128, 0, 255) && (rect.width() - 300.0).abs() < 0.01 && (rect.height() - 100.0).abs() < 0.01 && rect.y0 > 100.0 }),
            "the positioned cover must retain the vertical translation of its later containing block; fills={:?}",
            painter.fills
        );
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn identical_prepared_frame_reuses_authoritative_shaped_lines() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-line-cache-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body><p>Kerning AV and office ligatures.</p><p>Second line.</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());

        renderer.prepare_frame(Size::new(600.0, 800.0)).paint(&mut RecordingPainter::default());
        let first_calls = renderer.glyph_shaper().line_shape_calls;
        assert_eq!(first_calls, 0, "view preparation must consume document-authoritative shaping");
        let scratch_capacities = {
            let scratch = &renderer.core.frame_scratch.line_shape;
            (
                scratch.source_runs.capacity(),
                scratch.text.capacity(),
                scratch.byte_offsets.capacity(),
                scratch.extra_advances.capacity(),
                scratch.advance_overrides.capacity(),
                scratch.baseline_shifts.capacity(),
                scratch.paints.capacity(),
                scratch.placements.capacity(),
            )
        };
        renderer.prepare_frame(Size::new(600.0, 800.0)).paint(&mut RecordingPainter::default());
        assert_eq!(renderer.glyph_shaper().line_shape_calls, first_calls, "an unchanged cached frame must not reshape its lines");
        renderer.prepare_frame(Size::new(620.0, 800.0)).paint(&mut RecordingPainter::default());
        assert_eq!(renderer.glyph_shaper().line_shape_calls, first_calls, "a position-only width resize must reuse shaped visible lines");
        renderer.change_column_width(-20.0);
        renderer.prepare_frame(Size::new(620.0, 800.0)).paint(&mut RecordingPainter::default());
        assert_eq!(renderer.glyph_shaper().line_shape_calls, first_calls, "a layout-changing resize must not invoke a second shaping owner");
        assert_eq!(renderer.core.frame_scratch.line_shape.source_run_rebuilds, 0, "authoritative runs need no view-owned source-run index");
        let retained_capacities = {
            let scratch = &renderer.core.frame_scratch.line_shape;
            (
                scratch.source_runs.capacity(),
                scratch.text.capacity(),
                scratch.byte_offsets.capacity(),
                scratch.extra_advances.capacity(),
                scratch.advance_overrides.capacity(),
                scratch.baseline_shifts.capacity(),
                scratch.paints.capacity(),
                scratch.placements.capacity(),
            )
        };
        assert!(retained_capacities.0 >= scratch_capacities.0);
        assert!(retained_capacities.1 >= scratch_capacities.1);
        assert!(retained_capacities.2 >= scratch_capacities.2);
        assert!(retained_capacities.3 >= scratch_capacities.3);
        assert!(retained_capacities.4 >= scratch_capacities.4);
        assert!(retained_capacities.5 >= scratch_capacities.5);
        assert!(retained_capacities.6 >= scratch_capacities.6);
        assert!(retained_capacities.7 >= scratch_capacities.7);

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn prepared_page_prefetch_does_not_invalidate_active_text_runs() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-prepared-page-runs-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        let paragraphs = (0..40).map(|index| format!("<p style='margin:0;height:24px'>Page line {index}</p>")).collect::<String>();
        std::fs::write(&document, format!("<html><body style='margin:0'>{paragraphs}</body></html>")).unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        let viewport = Size::new(320.0, 96.0);

        renderer.prepare_frame(viewport);
        assert!(!renderer.core.viewport.reached_end, "the fixture must span multiple prepared pages");
        let first_page_start = renderer.core.frame.current_page_start_line;
        let active_shape_calls = renderer.glyph_shaper.line_shape_calls;
        assert_eq!(active_shape_calls, 0, "prepared pages consume document-authoritative shaping");
        for fragment in &renderer.core.frame.shaped_lines {
            assert_eq!(renderer.glyph_shaper.active_line_runs.get(&fragment.shaped.run), Some(&fragment.shaped.line_index));
        }

        // The second identical preparation performs the delayed adjacent-page
        // prefetch. It must not clear or replace the active page's run store.
        renderer.prepare_frame(viewport);
        assert_eq!(renderer.glyph_shaper.line_shape_calls, active_shape_calls, "geometry prefetch must not shape the adjacent page");
        assert_eq!(renderer.core.frame.current_page_start_line, first_page_start);
        assert!(!renderer.core.prepared_pages.is_empty(), "the adjacent page should still be prepared geometrically");
        assert!(renderer.core.prepared_pages.iter().all(|page| page.frame.shaped_lines.is_empty() && page.frame.native_shape_failures.is_empty() && page.frame.line_shape_key.is_none()));
        for fragment in &renderer.core.frame.shaped_lines {
            assert_eq!(renderer.glyph_shaper.active_line_runs.get(&fragment.shaped.run), Some(&fragment.shaped.line_index), "the active frame must retain handles into the active run store");
        }

        renderer.next_page();
        renderer.prepare_frame(viewport);
        assert_ne!(renderer.core.frame.current_page_start_line, first_page_start);
        assert_eq!(renderer.glyph_shaper.line_shape_calls, active_shape_calls, "restoring a geometry-only page must not invoke view-owned shaping");
        for fragment in &renderer.core.frame.shaped_lines {
            assert_eq!(renderer.glyph_shaper.active_line_runs.get(&fragment.shaped.run), Some(&fragment.shaped.line_index));
        }

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn selection_boundary_affinity_preserves_the_gap_around_an_inline_image() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-inline-image-selection-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body style='margin:0'><p style='margin:0;white-space:nowrap'>a<img src='missing.png' style='width:20px;height:10px'>b</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        renderer.prepare_frame(Size::new(600.0, 800.0));

        let view = renderer.core.document.render_view().text();
        let (line_idx, before_range, after_range) = (0..view.line_count())
            .find_map(|line_idx| {
                let fragments = view.line_text_fragments(line_idx)?.collect::<Vec<_>>();
                (fragments.len() == 2 && fragments[0].glyphs().end == fragments[1].glyphs().start).then(|| (line_idx, fragments[0].glyphs(), fragments[1].glyphs()))
            })
            .expect("text on both sides of an inline image should remain independently positioned on one line");
        let line = view.line(line_idx).unwrap();
        let boundary = before_range.end;
        let before_image = renderer.core.text_geometry().glyph_x_in_line_trailing(&line, boundary);
        let after_image = renderer.core.text_geometry().glyph_x_in_line(&line, boundary);
        assert!(after_image > before_image, "leading and trailing affinity must preserve the replaced-content gap");
        assert_eq!(
            renderer.core.doc().line_text_intersections(line_idx, before_range.start, after_range.end).collect::<Vec<_>>(),
            vec![(before_range.start, before_range.end), (after_range.start, after_range.end)],
            "interaction overlays must preserve the replaced-content gap instead of merging both text fragments",
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn nested_atomic_selection_intersects_only_its_owning_line() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-atomic-selection-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body style='margin:0'><p style='margin:0;white-space:nowrap'>A<span style='display:inline-flex'>INNER</span>B</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        renderer.prepare_frame(Size::new(600.0, 800.0));

        let view = renderer.core.document.render_view().text();
        let glyph_for = |target| (0..view.glyph_count() as u32).find(|index| view.glyph_at(*index as usize).and_then(|glyph| view.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == target)).expect("fixture character must exist");
        let outer_line = renderer.core.doc().find_line_for_glyph(glyph_for('A')).unwrap();
        let nested = glyph_for('I');
        let nested_line = renderer.core.doc().find_line_for_glyph(nested).unwrap();

        assert_ne!(outer_line, nested_line);
        assert_eq!(renderer.core.doc().line_text_intersections(outer_line, nested, nested + 1).next(), None);
        assert_eq!(renderer.core.doc().line_text_intersections(nested_line, nested, nested + 1).next(), Some((nested, nested + 1)));
        assert_eq!(renderer.core.doc().find_line_for_glyph(glyph_for('B')), Some(outer_line));
        assert_eq!(renderer.core.visible_text(), "AINNERB", "speakable text must merge nested atomic lines back into source reading order");

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn native_line_rejection_uses_the_emergency_glyph_path_instead_of_hiding_text() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-native-rejection-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body><p>Still visible</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let shaper = TestShaper { reject_lines: true, ..TestShaper::default() };
        let mut renderer = RendererSession::from_provider_with_nav(host, shaper, provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        let mut painter = NativeFallbackPainter::default();
        renderer.prepare_frame(Size::new(600.0, 800.0)).paint(&mut painter);

        assert_eq!(painter.text_runs, 0);
        assert!(!painter.recording.glyphs.is_empty(), "a rejected native line must retain visible emergency glyph output");
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn view_does_not_reissue_document_shaping_requests() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-native-line-features-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(
            &document,
            r#"<html><head><style>
                body { margin: 0; }
                .justified { width: 150px; text-align: justify; }
                .tracked { letter-spacing: 3px; }
                .large { font-size: 30px; }
                .tabs { white-space: pre; }
                .raised { vertical-align: super; }
                .ellipsis { width: 55px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
            </style></head><body>
                <p class="justified">JUSTIFY alpha beta gamma delta epsilon zeta eta theta.</p>
                <p class="tracked">TRACKED</p>
                <p>MIXED <span class="large">LARGE</span> SMALL</p>
                <p>OFFSET <span class="raised">UP</span></p>
                <p class="tabs">TAB	STOP</p>
                <p class="ellipsis">ELLIPSIS OVERFLOW</p>
                <p>BEFORE<img src="missing.png" width="20" height="10">AFTER</p>
            </body></html>"#,
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let host = Rc::new(TestHost::default());
        let mut renderer = RendererSession::from_provider_with_nav(host, TestShaper::default(), provider, vec![document.to_string_lossy().into_owned()], 0, Some("0:0"), crate::RendererInitialConfig::default());
        renderer.prepare_frame(Size::new(600.0, 1000.0)).paint(&mut RecordingPainter::default());
        let requests = &renderer.glyph_shaper().line_requests;

        assert!(requests.is_empty(), "view preparation must not create a second document-shaping pipeline");

        std::fs::remove_dir_all(fixture).unwrap();
    }
}
