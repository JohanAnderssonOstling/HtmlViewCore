mod appearance;
#[cfg(test)]
mod appearance_tests;
#[cfg(test)]
mod font_tests;
#[cfg(test)]
mod navigation_tests;
mod ruler;
pub use ruler::ReadingRulerStyle;

use core::panic::Location;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use web_time::Instant;

use crate::ChapterPreparationWorker;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, Element,
    ElementId, Entity, EventEmitter, FocusHandle, GlobalElementId, Hitbox, HitboxBehavior,
    ImageFormat, InspectorElementId, IntoElement, KeyContext, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point as GpuiPoint, Render, ScrollDelta,
    ScrollWheelEvent, Style, Window, WindowTextSystem, relative,
};
use html::export::TableExportFormat;
use image::RgbaImage;
use kurbo::{Point, Size};
use skrift_column_paginator::{
    BlockingJob, BlockingJobSpawner, ClipboardWrite, ContentHit, DisplayCacheKey, FontFace,
    FrameDisplayCacheKeys, FrameGeometryCacheKey, InteractionPalette, PointerDownOptions,
    ReadingFragment, ReadingTarget, RendererCommand, RendererEffects, RendererEvent,
    RendererServices, RendererSession, ResourceProvider, SelectionMode, VisibleTextMatches,
};

use gpui::AppContext;
use html_gpui::TextShapeCacheStats;
use skrift_column_paginator::text_backend::GlyphShaper;
use html_gpui::backend::{
    GpuiCommandPainter, GpuiScene, GpuiTextCache, GpuiTextResources, ImageCache, LoadedFontFace,
    face_key, load_font_face,
};

/// A window point in the note's own coordinates, which is where its geometry
/// and therefore its hit testing live.
fn note_local(position: GpuiPoint<Pixels>, origin: GpuiPoint<Pixels>) -> Point {
    Point::new(
        f32::from(position.x - origin.x) as f64,
        f32::from(position.y - origin.y) as f64,
    )
}

fn selection_mode(semantic: bool) -> SelectionMode {
    if semantic {
        SelectionMode::Semantic
    } else {
        SelectionMode::Plain
    }
}

/// Horizontal padding `components::reader_footnote_panel` applies. A note is
/// laid out to the panel's inner width, so this has to agree with it.
const FOOTNOTE_PANEL_PADDING: f64 = 18.0;

/// A note already laid out and turned into paint commands.
///
/// Built once, when a note is opened, and painted many times. It holds no
/// entity handle: preparing it during layout would re-enter the view that
/// produced it, which is being rendered in the same tree.
#[derive(Clone)]
pub struct PreparedNote {
    scene: GpuiScene,
    height: f32,
}

/// Draws a note from a footnote preview, sized to the height it occupies at
/// the width it was laid out to.
pub struct HtmlNoteElement {
    view: Entity<HtmlView>,
    prepared: PreparedNote,
}

impl HtmlNoteElement {
    pub fn new(view: Entity<HtmlView>, prepared: PreparedNote) -> Self {
        Self { view, prepared }
    }
}

pub struct NotePrepaint {
    hitbox: Hitbox,
    overlay: GpuiScene,
}

impl IntoElement for HtmlNoteElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HtmlNoteElement {
    type RequestLayoutState = ();
    type PrepaintState = NotePrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = gpui::px(self.prepared.height.max(1.0)).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let overlay = self.view.update(cx, |view, _| view.prepare_note_overlay());
        NotePrepaint { hitbox, overlay }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.prepared.scene.paint(bounds, window, cx);
            prepaint.overlay.paint(bounds, window, cx);
        });

        // Selecting in a note is the page's interaction applied to a second
        // view, so the events it needs are the page's three.
        let origin = bounds.origin;
        let hitbox = prepaint.hitbox.clone();
        let target = self.view.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            let position = event.position;
            let semantic = event.modifiers.control;
            target.update(cx, |view, cx| {
                if view.note_pointer_down(position, origin, semantic, cx) {
                    cx.notify();
                    cx.stop_propagation();
                }
            });
        });

        let hitbox = prepaint.hitbox.clone();
        let target = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                return;
            }
            let position = event.position;
            let semantic = event.modifiers.control;
            target.update(cx, |view, cx| {
                if view.note_pointer_move(position, origin, semantic, cx) {
                    cx.notify();
                }
            });
        });

        let target = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            target.update(cx, |view, cx| {
                if view.note_pointer_up(event.position, origin, event.modifiers.control, cx) {
                    cx.notify();
                }
            });
        });
    }
}

struct HtmlDocumentElement {
    view: Entity<HtmlView>,
    focus: FocusHandle,
    link_hovered: bool,
}

struct HtmlDocumentPrepaint {
    hitbox: Hitbox,
    paint: PreparedPaint,
    ruler: Vec<(kurbo::Rect, gpui::Hsla)>,
}

enum HostMessage {
    ResourceReady,
    FontLoaded { key: String, result: Result<LoadedFontFace, String> },
}

struct GpuiHost {
    text_system: Arc<WindowTextSystem>,
    font_requests: HashSet<String>,
    messages: async_channel::Sender<HostMessage>,
    executor: gpui::BackgroundExecutor,
    chapter_preparation: ChapterPreparationWorker,
    resource_repaint_pending: Arc<AtomicBool>,
}

/// Bridges the renderer's runtime-neutral finite-job contract to GPUI's
/// platform executor. On the web that executor owns the Web Worker pool; on
/// native platforms it uses GPUI's shared background workers.
struct GpuiBlockingJobSpawner {
    executor: gpui::BackgroundExecutor,
}

impl BlockingJobSpawner for GpuiBlockingJobSpawner {
    fn spawn(&self, job: BlockingJob) {
        self.executor
            .spawn(async move {
                job();
            })
            .detach();
    }
}

impl GpuiHost {
    fn new(
        messages: async_channel::Sender<HostMessage>,
        executor: gpui::BackgroundExecutor,
        text_system: Arc<WindowTextSystem>,
        chapter_preparation: ChapterPreparationWorker,
    ) -> Self {
        Self {
            text_system,
            font_requests: HashSet::new(),
            messages,
            executor,
            chapter_preparation,
            resource_repaint_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Loads publication faces still needed by the reader's font policy.
    fn prepare_font_faces(
        &mut self,
        faces: &[FontFace],
        provider: Arc<dyn ResourceProvider>,
    ) {
        let pending = {
            let requested = &mut self.font_requests;
            faces
                .iter()
                .filter(|face| requested.insert(face_key(face)))
                .cloned()
                .collect::<Vec<_>>()
        };
        if pending.is_empty() {
            return;
        }
        let installed = self.text_system.all_font_names();
        for face in pending {
            let provider = provider.clone();
            let installed = installed.clone();
            let messages = self.messages.clone();
            let key = face_key(&face);
            let work = async move {
                let result = load_font_face(face, provider, &installed).await;
                let _ = messages.send(HostMessage::FontLoaded { key, result }).await;
            };
            #[cfg(not(target_arch = "wasm32"))]
            self.executor.spawn(work).detach();
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(work);
        }
    }

    fn services(&self) -> RendererServices {
        RendererServices {
            blocking_jobs: Arc::new(GpuiBlockingJobSpawner {
                executor: self.executor.clone(),
            }),
            chapter_preparation: Arc::new(self.chapter_preparation.clone()),
            resource_waker: self.resource_waker(),
        }
    }

    fn resource_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let messages = self.messages.clone();
        let pending = self.resource_repaint_pending.clone();
        Some(Arc::new(move || {
            if !pending.swap(true, Ordering::Relaxed) {
                let _ = messages.try_send(HostMessage::ResourceReady);
            }
        }))
    }
}

pub struct HtmlView {
    host: GpuiHost,
    renderer: RendererSession<GpuiTextCache>,
    effects: RendererEffects,
    appearance: appearance::PendingAppearance,
    focus: FocusHandle,
    images: ImageCache,
    document_origin: GpuiPoint<Pixels>,
    paint_stats: PaintStats,
    layout_geometry: Option<HtmlLayoutGeometry>,
    prepared_viewport: Option<Size>,
    visible_page_key: Option<(usize, FrameGeometryCacheKey)>,
    prepared_paint: Option<(FrameDisplayCacheKeys, PreparedPaint)>,
    document_paint_resources: GpuiTextResources,
    note_paint_resources: Option<GpuiTextResources>,
    display_dirty: bool,
    pointer_active: bool,
    pointer_tap_start: Option<(GpuiPoint<Pixels>, Instant, bool)>,
    wheel_scroll_enabled: bool,
    ruler: ruler::ReadingRuler,
}

#[derive(Clone)]
struct PreparedPaint {
    base: GpuiScene,
    overlay: GpuiScene,
}

/// Passive retained page, used while the live view navigates to its successor.
#[derive(Clone)]
pub struct HtmlPageSnapshot {
    paint: PreparedPaint,
    ruler: Vec<(kurbo::Rect, gpui::Hsla)>,
}

impl IntoElement for HtmlPageSnapshot {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for HtmlPageSnapshot {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        paint_page(&self.paint, bounds, window, cx);
        ruler::paint_ruler(&self.ruler, bounds, window);
    }
}

fn paint_page(paint: &PreparedPaint, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    window.paint_quad(gpui::fill(bounds, gpui::white()));
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        paint.base.paint(bounds, window, cx);
        paint.overlay.paint(bounds, window, cx);
    });
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaintStats {
    pub compositions: u64,
}

/// Effective post-layout column geometry for the most recently prepared
/// viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HtmlLayoutGeometry {
    pub viewport_width: f64,
    pub viewport_height: f64,
    pub column_width: f64,
    pub column_gap: f64,
    pub column_count: u8,
    pub scale: f64,
}

impl HtmlLayoutGeometry {
    fn from_display_key(key: DisplayCacheKey) -> Self {
        Self {
            viewport_width: f64::from_bits(key.geometry.viewport_width_bits),
            viewport_height: f64::from_bits(key.geometry.viewport_height_bits),
            column_width: f64::from_bits(key.geometry.column_width_bits),
            column_gap: f64::from_bits(key.geometry.column_gap_bits),
            column_count: key.geometry.column_count,
            scale: f64::from_bits(key.geometry.scale_bits),
        }
    }
}

impl IntoElement for HtmlDocumentElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HtmlDocumentElement {
    type RequestLayoutState = ();
    type PrepaintState = HtmlDocumentPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::View(self.view.entity_id()))
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        window.set_focus_handle(&self.focus, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let (paint, ruler) = self.view.update(cx, |view, cx| {
            let paint = view.prepared_paint_for_bounds(bounds, cx);
            (paint, view.prepare_ruler())
        });
        HtmlDocumentPrepaint {
            hitbox,
            paint,
            ruler,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        paint_page(&prepaint.paint, bounds, window, cx);
        ruler::paint_ruler(&prepaint.ruler, bounds, window);

        if self.link_hovered {
            window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);
        }

        let mut key_context = KeyContext::default();
        key_context.add("ReaderDocument");
        window.set_key_context(key_context);

        let hitbox = prepaint.hitbox.clone();
        let mouse_down_target = self.view.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            mouse_down_target.update(cx, |view, cx| {
                view.on_mouse_down(event, window, cx);
            });
        });

        let hitbox = prepaint.hitbox.clone();
        let mouse_move_target = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let pointer_active = mouse_move_target.read(cx).pointer_active;
            if !pointer_active && !hitbox.is_hovered(window) {
                return;
            }
            mouse_move_target.update(cx, |view, cx| {
                view.on_mouse_move(event, window, cx);
            });
        });

        let mouse_up_target = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !mouse_up_target.read(cx).pointer_active
            {
                return;
            }
            mouse_up_target.update(cx, |view, cx| {
                view.on_mouse_up(event, window, cx);
            });
        });

        let hitbox = prepaint.hitbox.clone();
        let scroll_target = self.view.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                return;
            }
            if scroll_target.read(cx).wheel_scroll_enabled {
                scroll_target.update(cx, |view, cx| {
                    view.on_scroll(event, window, cx);
                });
            }
        });

        let focus = self.focus.clone();
        let key_target = self.view.clone();
        window.on_key_event(move |event: &KeyDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !focus.is_focused(window) {
                return;
            }
            key_target.update(cx, |view, cx| {
                view.on_key_down(event, window, cx);
            });
        });
    }
}

/// A short tap on ordinary document text that activated no link or image.
#[derive(Clone, Copy, Debug)]
pub struct DocumentTap;

impl EventEmitter<RendererEvent> for HtmlView {}
impl EventEmitter<DocumentTap> for HtmlView {}




impl HtmlView {
    pub fn from_provider(
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        start_index: usize,
        nav_state: Option<&str>,
        config: skrift_column_paginator::RendererInitialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, String> {
        let (messages, receiver) = async_channel::unbounded();
        let host = GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        );
        let shaper = GpuiTextCache::for_window(window, cx);
        let renderer = RendererSession::from_provider_with_nav(
            host.services(),
            shaper,
            provider,
            document_uris,
            start_index,
            nav_state,
            config,
        )?;
        Ok(Self::from_renderer(host, receiver, renderer, window, cx))
    }

    pub fn from_preparation(
        prepared: skrift_column_paginator::RendererPreparation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, String> {
        Self::new_with_renderer(window, cx, move |services, shaper| {
            RendererSession::from_preparation(services, shaper, prepared)
        })
    }

    /// Complete fallible shaping before registering the GPUI entity.
    pub fn create_from_preparation(
        prepared: skrift_column_paginator::RendererPreparation,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>, String> {
        let (messages, receiver) = async_channel::unbounded();
        let host = GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        );
        let shaper = GpuiTextCache::for_window(window, cx);
        let renderer = RendererSession::from_preparation(host.services(), shaper, prepared)?;
        Ok(cx.new(|cx| Self::from_renderer(host, receiver, renderer, window, cx)))
    }

    fn new_with_renderer(
        window: &mut Window,
        cx: &mut Context<Self>,
        make_renderer: impl FnOnce(
            RendererServices,
            GpuiTextCache,
        ) -> Result<RendererSession<GpuiTextCache>, String>,
    ) -> Result<Self, String> {
        let (messages, receiver) = async_channel::unbounded();
        let host = GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        );
        let shaper = GpuiTextCache::for_window(window, cx);
        let renderer = make_renderer(host.services(), shaper)?;
        Ok(Self::from_renderer(host, receiver, renderer, window, cx))
    }

    fn from_renderer(
        host: GpuiHost,
        receiver: async_channel::Receiver<HostMessage>,
        mut renderer: RendererSession<GpuiTextCache>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        renderer.emit_state_snapshot();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        cx.spawn(async move |this, cx| {
            while let Ok(message) = receiver.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_host_message(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let document_paint_resources = renderer.glyph_shaper().resources();
        let mut view = Self {
            host,
            renderer,
            effects: RendererEffects::default(),
            document_paint_resources,
            note_paint_resources: None,
            appearance: appearance::PendingAppearance::default(),
            focus,
            images: ImageCache::default(),
            document_origin: GpuiPoint::default(),
            paint_stats: PaintStats::default(),
            layout_geometry: None,
            prepared_viewport: None,
            visible_page_key: None,
            prepared_paint: None,
            display_dirty: true,
            pointer_active: false,
            pointer_tap_start: None,
            wheel_scroll_enabled: true,
            ruler: ruler::ReadingRuler::default(),
        };
        view.flush_effects(cx);
        view
    }

    fn flush_effects(&mut self, cx: &mut Context<Self>) {
        self.renderer.drain_effects(&mut self.effects);
        for request in self.effects.font_loads.drain(..) {
            if request.use_publication_fonts {
                self.host.prepare_font_faces(&request.faces, request.provider);
            }
        }
        for write in self.effects.clipboard.drain(..) {
            match write {
                ClipboardWrite::Text(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
                ClipboardWrite::Image {
                    width,
                    height,
                    rgba,
                } => {
                    let Some(buffer) = RgbaImage::from_raw(width as u32, height as u32, rgba)
                    else {
                        continue;
                    };
                    if let Ok(image) = gpui::Image::from_rgba(&buffer) {
                        cx.write_to_clipboard(ClipboardItem::new_image(&image));
                    }
                }
                ClipboardWrite::Svg(bytes) => {
                    let image = gpui::Image::from_bytes(ImageFormat::Svg, bytes);
                    cx.write_to_clipboard(ClipboardItem::new_image(&image));
                }
            }
        }
        for event in self.effects.events.drain(..) {
            cx.emit(event);
        }
        let repaint = std::mem::take(&mut self.effects.repaint);
        let restyle = std::mem::take(&mut self.effects.restyle);
        if repaint {
            self.display_dirty = true;
        }
        if repaint || restyle {
            cx.notify();
        }
    }

    fn sync_paint_resources(&mut self) {
        let shaper = self.renderer.glyph_shaper();
        let generation = shaper.resource_generation();
        if self.renderer.document_resource_generation() == generation {
            self.document_paint_resources = shaper.resources();
        }
        match self.renderer.note_resource_generation() {
            Some(note_generation) if note_generation == generation => {
                self.note_paint_resources = Some(shaper.resources());
            }
            Some(_) => {}
            None => self.note_paint_resources = None,
        }
    }

    pub fn apply(&mut self, command: RendererCommand, cx: &mut Context<Self>) {
        self.sync_paint_resources();
        self.renderer.apply(command);
        self.sync_paint_resources();
        self.flush_effects(cx);
    }

    pub fn current_cfi(&self) -> Option<String> {
        self.renderer.current_cfi()
    }
    pub fn reading_document(&self) -> Option<usize> {
        self.renderer.reading_document()
    }
    pub fn visible_page_key(&self) -> Option<(usize, FrameGeometryCacheKey)> {
        self.renderer.visible_page_key()
    }
    pub fn match_visible_text(
        &self,
        patterns: &skrift_column_paginator::TextPatternMatcher,
    ) -> VisibleTextMatches<'_> {
        self.renderer.match_visible_text(patterns)
    }
    pub fn reading_items(&self) -> Option<(usize, usize, u32, Vec<ReadingTarget>)> {
        self.renderer.reading_items()
    }
    pub fn reading_chunk(
        &self,
        target: &ReadingTarget,
        glyph_start: u32,
        max_chars: usize,
        select: impl FnOnce(&str) -> Option<Range<usize>>,
    ) -> Option<(ReadingFragment, Option<u32>)> {
        self.renderer.render_view().addressing().reading_chunk(
            target,
            glyph_start,
            max_chars,
            select,
        )
    }
    pub fn reading_target_cfi(&self, target: &ReadingTarget) -> Option<String> {
        self.renderer.reading_target_cfi(target)
    }

    pub fn navigation_revision(&self) -> u64 {
        self.renderer.navigation_revision()
    }
    pub fn advance_reading_document(&mut self, cx: &mut Context<Self>) {
        self.renderer.advance_reading_document();
        self.flush_effects(cx);
    }

    pub fn follow_reading_target(&mut self, target: ReadingTarget, cx: &mut Context<Self>) {
        if self.renderer.follow_reading_target(target) {
            self.reset_ruler(true);
        }
        self.flush_effects(cx);
    }

    /// Reflows to the width supplied by the containing GPUI layout without
    /// replacing the reader's preferred column width.
    pub fn set_available_width(&mut self, width: f64, cx: &mut Context<Self>) {
        if self.renderer.set_available_width(width) {
            self.display_dirty = true;
        }
        self.flush_effects(cx);
    }

    /// Returns whether the renderer is still loading or decoding resources.
    pub fn has_pending_resources(&self) -> bool {
        self.renderer.has_pending_resources()
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette, cx: &mut Context<Self>) {
        self.renderer.set_interaction_palette(palette);
        self.flush_effects(cx);
    }

    /// Opens the note a reference points at, as activating the reference
    /// would. Emits `FootnoteOpened` and reports whether a note was found.
    pub fn open_note(&mut self, href: &str, cx: &mut Context<Self>) -> bool {
        self.sync_paint_resources();
        let preview = self.renderer.note_preview(href);
        self.flush_effects(cx);
        let Some(preview) = preview else {
            return false;
        };
        self.sync_paint_resources();
        cx.emit(RendererEvent::FootnoteOpened(preview));
        true
    }

    pub fn next_page(&mut self, cx: &mut Context<Self>) -> bool {
        self.turn_page(true, cx)
    }

    /// Controls whether this view consumes vertical wheel gestures itself.
    /// Embedders with explicit pagination can disable this so wheel gestures
    /// continue to the surrounding page instead.
    pub fn set_wheel_scroll_enabled(&mut self, enabled: bool) {
        self.wheel_scroll_enabled = enabled;
    }

    pub fn previous_page(&mut self, cx: &mut Context<Self>) -> bool {
        self.turn_page(false, cx)
    }

    fn turn_page(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let before = self.renderer.current_cfi();
        if forward {
            self.renderer.apply(RendererCommand::NavigatePage(1));
        } else {
            self.renderer.apply(RendererCommand::NavigatePage(-1));
        }
        self.display_dirty = true;
        // Pagination (especially a backward turn) settles during composition.
        // Compose once here so boundary detection and rapid repeated turns use
        // the destination before the next frame.
        if let Some(viewport) = self.prepared_viewport {
            self.compose_viewport_display_list(viewport);
            self.display_dirty = false;
        }
        let moved =
            before != self.renderer.current_cfi() || self.renderer.reading_document().is_none();
        self.ruler_page_turn_completed(forward, moved);
        cx.notify();
        self.flush_effects(cx);
        moved
    }

    /// Retain the displayed command buffers and glyphs without copying pixels.
    pub fn page_snapshot(&mut self) -> Option<HtmlPageSnapshot> {
        Some(HtmlPageSnapshot {
            paint: self.prepared_paint.as_ref()?.1.clone(),
            ruler: self.prepare_ruler(),
        })
    }

    pub fn next_line(&mut self, cx: &mut Context<Self>) {
        self.renderer.apply(RendererCommand::NavigateLine(1));
        self.flush_effects(cx);
    }

    pub fn previous_line(&mut self, cx: &mut Context<Self>) {
        self.renderer.apply(RendererCommand::NavigateLine(-1));
        self.flush_effects(cx);
    }

    pub fn next_section(&mut self, cx: &mut Context<Self>) {
        self.renderer.apply(RendererCommand::NavigateDocument(1));
        self.flush_effects(cx);
    }

    pub fn previous_section(&mut self, cx: &mut Context<Self>) {
        self.renderer.apply(RendererCommand::NavigateDocument(-1));
        self.flush_effects(cx);
    }

    pub fn change_root_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let size = self.renderer.render_view().root_font_size() + delta;
        self.renderer.apply(RendererCommand::SetFontSize(size));
        self.flush_effects(cx);
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.renderer.resolve_href(href)
    }

    pub fn navigate_to_href(&mut self, href: &str, cx: &mut Context<Self>) -> bool {
        let result = self.renderer.navigate_to_href(href);
        self.flush_effects(cx);
        result
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn selection_contains(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .selection_contains_point(self.document_point(position))
    }

    pub fn select_word_at(&mut self, position: GpuiPoint<Pixels>, cx: &mut Context<Self>) -> bool {
        let selected = self.renderer.select_word_at(self.document_point(position));
        if selected {
            cx.notify();
        }
        self.flush_effects(cx);
        selected
    }

    pub fn select_element_at(
        &mut self,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self.renderer.select_element_at(self.document_point(position));
        if selected {
            cx.notify();
        }
        self.flush_effects(cx);
        selected
    }

    pub fn begin_selection_handle(&mut self, start: bool, cx: &mut Context<Self>) -> bool {
        let result = self.renderer.begin_selection_handle(start);
        self.flush_effects(cx);
        result
    }

    pub fn update_touch_selection(&mut self, position: GpuiPoint<Pixels>, cx: &mut Context<Self>) {
        if self
            .renderer
            .update_touch_selection(self.document_point(position))
        {
            cx.notify();
        }
        self.flush_effects(cx);
    }

    pub fn end_touch_selection(&mut self, cx: &mut Context<Self>) {
        self.renderer.end_touch_selection();
        self.flush_effects(cx);
    }

    pub fn clear_text_selection(&mut self, cx: &mut Context<Self>) {
        self.renderer.clear_text_selection();
        cx.notify();
        self.flush_effects(cx);
    }

    pub fn selected_passage(&self) -> Option<(String, String)> {
        self.renderer.selected_passage()
    }

    pub fn has_text_selection(&self) -> bool {
        self.renderer.has_text_selection()
    }

    pub fn selection_cfi(&self) -> Option<String> {
        self.renderer.selection_cfi()
    }

    /// Copies the selection as Markdown, also available through Ctrl/Cmd+Shift+C.
    pub fn copy_selection_as_markdown(&mut self, cx: &mut Context<Self>) -> bool {
        let result = self.renderer.copy_selection();
        self.flush_effects(cx);
        result
    }

    fn copy_selection_as_plain_text(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self
            .renderer
            .note_selection_text()
            .or_else(|| self.renderer.selected_passage().map(|(_, text)| text));
        let Some(text) = text else {
            let copied = self.renderer.copy_table_selection();
            self.flush_effects(cx);
            return copied;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    pub fn selection_endpoints(&self) -> (Option<GpuiPoint<Pixels>>, Option<GpuiPoint<Pixels>>) {
        let origin = self.document_origin;
        let convert = |point: Point| {
            GpuiPoint::new(
                origin.x + gpui::px(point.x as f32),
                origin.y + gpui::px(point.y as f32),
            )
        };
        let (start, end) = self.renderer.selection_endpoints();
        (start.map(convert), end.map(convert))
    }

    pub fn content_at(&self, position: GpuiPoint<Pixels>) -> ContentHit {
        self.renderer.content_at(self.document_point(position))
    }

    pub fn copy_image_at(&mut self, position: GpuiPoint<Pixels>, cx: &mut Context<Self>) -> bool {
        let result = self.renderer.copy_image_at(self.document_point(position));
        self.flush_effects(cx);
        result
    }

    pub fn copy_equation_latex_at(
        &mut self,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .renderer
            .copy_equation_latex_at(self.document_point(position));
        self.flush_effects(cx);
        result
    }

    pub fn table_selection_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .table_selection_at(self.document_point(position))
    }

    pub fn begin_table_selection_at(
        &mut self,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .renderer
            .begin_table_selection_at(self.document_point(position));
        self.flush_effects(cx);
        result
    }

    pub fn copy_table_at(
        &mut self,
        position: GpuiPoint<Pixels>,
        format: TableExportFormat,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self.renderer
            .copy_table_at(self.document_point(position), format);
        self.flush_effects(cx);
        result
    }

    pub fn copy_table_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let result = self.renderer.copy_table_selection();
        self.flush_effects(cx);
        result
    }

    pub fn paint_stats(&self) -> PaintStats {
        self.paint_stats
    }

    /// Geometry produced by the renderer for the last laid-out viewport.
    pub fn layout_geometry(&self) -> Option<HtmlLayoutGeometry> {
        self.layout_geometry
    }

    /// Where the current selection sits in the window.
    ///
    /// For a caller placing something that must not cover the passage: the
    /// pointer position a selection ends at is only its last line, so a panel
    /// placed against the pointer sits on top of everything above it.
    pub fn selection_bounds(&self) -> Option<Bounds<Pixels>> {
        self.window_bounds(self.renderer.selection_bounds())
    }

    /// The same, for an annotation already drawn on the page.
    pub fn annotation_bounds(&self, id: &str) -> Option<Bounds<Pixels>> {
        self.window_bounds(self.renderer.annotation_bounds(id))
    }

    /// Renderer geometry is in the surface's own coordinates; the window is
    /// what a panel placed over the reader is positioned in.
    fn window_bounds(&self, bounds: Option<kurbo::Rect>) -> Option<Bounds<Pixels>> {
        let bounds = bounds?;
        let origin = self.document_origin;
        let point = |x: f64, y: f64| {
            GpuiPoint::new(origin.x + gpui::px(x as f32), origin.y + gpui::px(y as f32))
        };
        Some(Bounds::from_corners(
            point(bounds.x0, bounds.y0),
            point(bounds.x1, bounds.y1),
        ))
    }

    pub fn text_shape_cache_stats(&self) -> TextShapeCacheStats {
        self.renderer
            .glyph_shaper()
            .shape_cache_stats()
    }

    /// Paints a note from a footnote preview into its own command buffer.
    /// Its glyphs were shaped by this view's cache while the note was laid
    /// out, so they resolve against the same store the page uses.
    /// Turns a note into paint commands once, for an element to draw many
    /// times. Call this when a note opens, never while rendering: it borrows
    /// this view, which is itself in the tree being rendered.
    ///
    /// Height and paint order come from the engine's scene rather than from
    /// anything worked out here.
    /// The open note's selection highlight, rebuilt each frame because it
    /// changes as the pointer drags.
    fn prepare_note_overlay(&mut self) -> GpuiScene {
        let mut painter =
            GpuiCommandPainter::commands_only(self.renderer.glyph_shaper().resources());
        self.renderer.paint_note_selection(&mut painter);
        painter.finish()
    }

    fn note_pointer_down(
        &mut self,
        position: GpuiPoint<Pixels>,
        origin: GpuiPoint<Pixels>,
        semantic: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .renderer
            .note_pointer_down(note_local(position, origin), selection_mode(semantic));
        self.flush_effects(cx);
        result
    }

    fn note_pointer_move(
        &mut self,
        position: GpuiPoint<Pixels>,
        origin: GpuiPoint<Pixels>,
        semantic: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .renderer
            .note_pointer_move(note_local(position, origin), selection_mode(semantic));
        self.flush_effects(cx);
        result
    }

    fn note_pointer_up(
        &mut self,
        position: GpuiPoint<Pixels>,
        origin: GpuiPoint<Pixels>,
        semantic: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .renderer
            .note_pointer_up(note_local(position, origin), selection_mode(semantic));
        self.flush_effects(cx);
        result
    }

    /// Copies what is selected inside the note, if anything.
    pub fn copy_note_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.renderer.note_selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// Drops the open note so nothing selected in it outlives the panel.
    pub fn close_note(&mut self, cx: &mut Context<Self>) {
        self.sync_paint_resources();
        self.renderer.close_note();
        self.sync_paint_resources();
        self.note_paint_resources = None;
        self.flush_effects(cx);
    }

    pub fn prepare_note(&mut self) -> Option<PreparedNote> {
        self.sync_paint_resources();
        let scene = self.renderer.note_scene()?;
        let resources = self.note_paint_resources.as_ref()?;
        if Some(resources.generation()) != self.renderer.note_resource_generation() {
            return None;
        }
        let mut painter = GpuiCommandPainter::new(&mut self.images, resources.clone())
            .image_theme(self.renderer.image_theme());
        scene.paint(&mut painter);
        let recorded = painter.finish();
        println!(
            "HTML_NOTE_PREPARED commands={} height={:.1}",
            recorded.len(),
            scene.content_height()
        );
        Some(PreparedNote {
            scene: recorded,
            height: scene.content_height() as f32,
        })
    }

    /// Returns a display list composed for the final post-layout bounds.
    /// `compose_viewport_display_list` is deliberately pipeline-free, so a
    /// resized frame never paints a snapshot for the previous viewport.
    fn prepared_paint_for_bounds(
        &mut self,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> PreparedPaint {
        self.flush_effects(cx);
        self.document_origin = bounds.origin;
        if !self.appearance.commands.is_empty() {
            return self
                .prepared_paint
                .as_ref()
                .map(|(_, paint)| paint.clone())
                .unwrap_or_else(|| {
                    let empty =
                        GpuiCommandPainter::commands_only(self.renderer.glyph_shaper().resources())
                            .finish();
                    PreparedPaint {
                        base: empty.clone(),
                        overlay: empty,
                    }
                });
        }
        self.renderer.set_note_popup_width(Some(
            (f32::from(bounds.size.width) as f64 - FOOTNOTE_PANEL_PADDING * 2.0).max(1.0),
        ));
        let viewport = Size::new(
            f32::from(bounds.size.width) as f64,
            f32::from(bounds.size.height) as f64,
        );
        let viewport_ready = self.prepared_viewport == Some(viewport);
        // Drained repaint requests invalidate the retained commands before a
        // parent redraw can query the page's ruler or selection bounds.
        if viewport_ready
            && !self.display_dirty
            && let Some((_, paint)) = &self.prepared_paint
        {
            return paint.clone();
        }
        let previous_geometry = self.layout_geometry;
        self.sync_ruler_navigation();
        let mut paint = self.compose_viewport_display_list(viewport);
        if self.preserve_ruler_after_reflow(
            !viewport_ready || self.layout_geometry != previous_geometry,
        ) {
            paint = self.compose_viewport_display_list(viewport);
        }
        self.prepared_viewport = Some(viewport);
        let visible_page_key = self.renderer.visible_page_key();
        if self.visible_page_key != visible_page_key {
            self.visible_page_key = visible_page_key;
            cx.emit(RendererEvent::VisiblePageChanged);
        }
        self.display_dirty = false;
        if self.layout_geometry != previous_geometry {
            cx.notify();
        }
        self.flush_effects(cx);
        paint
    }

    /// Composes pagination and GPUI commands from retained document state.
    /// This may not poll resources or invoke the HTML pipeline.
    fn compose_viewport_display_list(&mut self, viewport: Size) -> PreparedPaint {
        self.sync_paint_resources();
        let total_started = Instant::now();
        let renderer_started = Instant::now();
        let image_theme = self.renderer.image_theme();
        let frame = self.renderer.prepare_viewport(viewport);
        let renderer_us = renderer_started.elapsed().as_micros();
        let keys = frame.cache_keys();
        self.layout_geometry = Some(HtmlLayoutGeometry::from_display_key(keys.base));
        let (mut base, mut overlay) = self
            .prepared_paint
            .take()
            .map(|(cached, paint)| ((cached.base, paint.base), (cached.overlay, paint.overlay)))
            .unzip();
        let mut base_painter = base
            .as_ref()
            .is_none_or(|(key, _)| *key != keys.base)
            .then(|| {
                let painter =
                    GpuiCommandPainter::new(&mut self.images, self.document_paint_resources.clone())
                        .image_theme(image_theme);
                match base.take() {
                    Some((_, scene)) => painter.reusing(scene),
                    None => painter,
                }
            });
        let mut overlay_painter = overlay
            .as_ref()
            .is_none_or(|(key, _)| *key != keys.overlay)
            .then(|| {
                let painter = GpuiCommandPainter::commands_only(self.document_paint_resources.clone());
                match overlay.take() {
                    Some((_, scene)) => painter.reusing(scene),
                    None => painter,
                }
            });
        let layer_started = Instant::now();
        match (&mut base_painter, &mut overlay_painter) {
            (Some(base), Some(overlay)) => frame.paint_all_layers(base, overlay),
            (Some(base), None) => frame.paint_base_layer(base),
            (None, Some(overlay)) => frame.paint_overlay_layer(overlay),
            (None, None) => {}
        }
        let layer_us = layer_started.elapsed().as_micros();
        let base = base
            .map(|(_, scene)| scene)
            .unwrap_or_else(|| base_painter.unwrap().finish());
        let overlay = overlay
            .map(|(_, scene)| scene)
            .unwrap_or_else(|| overlay_painter.unwrap().finish());
        let command_count = base.len() + overlay.len();
        let paint = PreparedPaint { base, overlay };
        self.prepared_paint = Some((keys, paint.clone()));
        self.paint_stats.compositions += 1;
        let total_us = total_started.elapsed().as_micros();
        if std::env::var_os("BOKHEIM_PROFILE_RESIZE").is_some()
            || std::env::var_os("GPUI_KOBO_PROFILE").is_some()
            || total_us >= 10_000
        {
            println!(
                "HTML_GPUI_SNAPSHOT_PREPARE total_us={total_us} renderer_prepare_us={renderer_us} command_layers_us={layer_us} commands={command_count}"
            );
        }
        paint
    }

    fn handle_host_message(&mut self, message: HostMessage, cx: &mut Context<Self>) {
        self.sync_paint_resources();
        {
            use std::cell::Cell;
            thread_local!(static COUNT: Cell<u64> = const { Cell::new(0) });
            COUNT.with(|c| {
                let n = c.get() + 1;
                c.set(n);
                if n % 2000 == 0 {
                    eprintln!(
                        "HOST_MESSAGE_FLOOD count={n} last={:?}",
                        std::mem::discriminant(&message)
                    );
                }
            });
        }
        match message {
            HostMessage::ResourceReady => {
                self.host
                    .resource_repaint_pending
                    .store(false, Ordering::Relaxed);
                self.renderer.update_resources();
                self.display_dirty = true;
                cx.notify();
            }
            HostMessage::FontLoaded { key, result } => {
                let result = result.and_then(|loaded| {
                    self.renderer.glyph_shaper_mut().register_publication_font(loaded)
                });
                match result {
                    Ok(true) => {
                        self.renderer.apply(RendererCommand::RefreshFonts);
                        self.display_dirty = true;
                        cx.notify();
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.host.font_requests.remove(&key);
                        eprintln!("publication font unavailable: {error}");
                    }
                }
            }
        }
        self.sync_paint_resources();
        self.flush_effects(cx);
    }

    fn document_point(&self, position: GpuiPoint<Pixels>) -> Point {
        let origin = self.document_origin;
        Point::new(
            f32::from(position.x - origin.x) as f64,
            f32::from(position.y - origin.y) as f64,
        )
    }

    fn stop_event(cx: &mut Context<Self>) {
        cx.stop_propagation();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.focus.focus(window, cx);
        // A click that lands on nothing selectable still leaves propagation
        // running, and any focusable ancestor would then take the focus back
        // and with it the document's key context. Claiming the default is how
        // GPUI's own `track_focus` tells parents that focus is already spoken
        // for, so the keyboard keeps working after a click on a margin.
        window.prevent_default();
        let position = self.document_point(event.position);
        let options = PointerDownOptions {
            selection_mode: selection_mode(event.modifiers.control),
            copy_image: false,
        };
        let outcome = self.renderer.pointer_down(position, options);
        self.pointer_active = outcome.handled;
        self.pointer_tap_start =
            self.pointer_active
                .then_some((event.position, Instant::now(), outcome.image));
        if self.pointer_active {
            Self::stop_event(cx);
        }
        self.flush_effects(cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let position = self.document_point(event.position);
        let outcome = self
            .renderer
            .pointer_move(position, selection_mode(event.modifiers.control));
        if outcome.link_hover_changed {
            cx.notify();
        }
        if event.dragging() && outcome.handled {
            Self::stop_event(cx);
        }
        self.flush_effects(cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pointer_active {
            return;
        }
        self.pointer_active = false;
        let handled = self.renderer.pointer_up(self.document_point(event.position), selection_mode(event.modifiers.control));
        if let Some((start, started_at, image_hit)) = self.pointer_tap_start.take() {
            let moved = f32::from(event.position.x - start.x).abs() > 8.0
                || f32::from(event.position.y - start.y).abs() > 8.0;
            if !handled
                && !image_hit
                && !moved
                && event.click_count == 1
                && started_at.elapsed() < Duration::from_millis(600)
            {
                cx.emit(DocumentTap);
            }
        }
        Self::stop_event(cx);
        self.flush_effects(cx);
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The embedding reader owns page gestures. Consuming their live
        // pixels here would navigate a line for every move and prevent the
        // parent from tracking or completing the page turn.
        if window.is_page_scroll_swipe() || matches!(event.delta, ScrollDelta::Pages(_)) {
            return;
        }
        let y = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pages(delta) => {
                delta.y
                    * self.layout_geometry.map_or_else(
                        || f32::from(window.viewport_size().height),
                        |geometry| geometry.viewport_height as f32,
                    )
            }
        };
        let step = if y < 0.0 {
            -1
        } else if y > 0.0 {
            1
        } else {
            0
        };
        if step != 0 {
            self.renderer.apply(RendererCommand::NavigateLine(step));
        }
        self.flush_effects(cx);
        if step != 0 {
            Self::stop_event(cx);
        }
    }

    /// Handles only what the view owns exclusively.
    ///
    /// Reader navigation is not resolved here. The embedding reader binds
    /// reader key bindings to GPUI actions in the `ReaderDocument`
    /// key context and drives the operations through this type's public API,
    /// so that reader keys are visible to GPUI's action system rather than
    /// being consumed before dispatch reaches it.
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let copy_modifier = modifiers.secondary()
            && !modifiers.alt
            && !modifiers.function
            && !(modifiers.control && modifiers.platform);
        let handled = if key == "c" && copy_modifier {
            if modifiers.shift {
                self.copy_selection_as_markdown(cx)
            } else {
                self.copy_selection_as_plain_text(cx)
            }
        } else if key == "escape" {
            self.renderer.clear_table_selection()
        } else {
            false
        };
        if handled {
            cx.notify();
            Self::stop_event(cx);
        }
        self.flush_effects(cx);
    }
}

impl Render for HtmlView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_effects(cx);
        self.schedule_appearance(window, cx);
        HtmlDocumentElement {
            view: cx.entity(),
            focus: self.focus.clone(),
            link_hovered: self.renderer.link_cursor_active(),
        }
    }
}
