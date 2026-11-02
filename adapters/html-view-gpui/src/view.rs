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
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use web_time::Instant;

use crate::ChapterPreparationWorker;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, Element,
    ElementId, Entity, EventEmitter, FocusHandle, GlobalElementId, Hitbox, HitboxBehavior,
    ImageFormat, InspectorElementId, IntoElement, KeyContext, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point as GpuiPoint, Render, ScrollDelta,
    ScrollWheelEvent, Style, Window, WindowTextSystem, relative,
};
use image::RgbaImage;
use kurbo::{Point, Size};
use skrift_column_paginator::{
    BlockingJob, BlockingJobSpawner, DisplayCacheKey, FontFace, FrameGeometryCacheKey, InteractionPalette,
    PointerDownOptions, ReaderStyleOverrides, ReadingFragment, ReadingTarget, RendererCommand,
    RendererEvent, RendererHost, RendererSession, ResourceProvider, SelectionMode, VisibleSourceText,
};

use gpui::AppContext;
use html_gpui::TextShapeCacheStats;
use skrift_column_paginator::text_backend::{GlyphResourceGeneration, GlyphShaper};
use html_gpui::backend::{
    GlyphStore, GpuiCommandPainter, GpuiTextCache, ImageCache, LoadedFontFace, PaintCommand,
    PublicationFonts, RunStore, SharedPublicationFonts, face_key, load_font_face, paint_commands,
    publication_faces_for_overrides,
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
    commands: Arc<Vec<PaintCommand>>,
    glyph_store: GlyphStore,
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
    overlay: Arc<Vec<PaintCommand>>,
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
            paint_commands(
                self.prepared.commands.as_ref(),
                &self.prepared.glyph_store,
                bounds,
                window,
                cx,
            );
            paint_commands(
                prepaint.overlay.as_ref(),
                &self.prepared.glyph_store,
                bounds,
                window,
                cx,
            );
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
                if view.note_pointer_down(position, origin, semantic) {
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
                if view.note_pointer_move(position, origin, semantic) {
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
                if view.note_pointer_up(event.position, origin, event.modifiers.control) {
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
    Repaint,
    ResourceReady,
    FontReady(LoadedFontFace),
    FontSelectionChanged,
    Event(RendererEvent),
    ClipboardText(String),
    ClipboardImage {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    ClipboardSvg(Vec<u8>),
}

struct GpuiHost {
    fonts: SharedPublicationFonts,
    text_system: Arc<WindowTextSystem>,
    font_requests: RefCell<HashSet<String>>,
    font_overrides: RefCell<ReaderStyleOverrides>,
    document_fonts: RefCell<HashMap<usize, Vec<FontFace>>>,
    glyph_document: Cell<usize>,
    repaint_pending: Cell<bool>,
    messages: async_channel::Sender<HostMessage>,
    executor: gpui::BackgroundExecutor,
    chapter_preparation: ChapterPreparationWorker,
    scheduled_repaint: Arc<AtomicBool>,
    resource_repaint_pending: Arc<AtomicBool>,
    /// Width the footnote panel will show a note at, learned from the document
    /// bounds each paint. The panel is a flex child of the document body, so
    /// this is only known once the body has been laid out.
    note_width: Cell<Option<f64>>,
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
            fonts: Rc::new(RefCell::new(PublicationFonts::default())),
            text_system,
            font_requests: RefCell::default(),
            font_overrides: RefCell::default(),
            document_fonts: RefCell::default(),
            glyph_document: Cell::new(0),
            repaint_pending: Cell::new(false),
            messages,
            executor,
            chapter_preparation,
            scheduled_repaint: Arc::new(AtomicBool::new(false)),
            resource_repaint_pending: Arc::new(AtomicBool::new(false)),
            note_width: Cell::new(None),
        }
    }

    fn send(&self, message: HostMessage) {
        let _ = self.messages.try_send(message);
    }
}

impl RendererHost for GpuiHost {
    fn set_font_overrides(&self, overrides: &ReaderStyleOverrides) {
        *self.font_overrides.borrow_mut() = overrides.clone();
        let declarations = self.document_fonts.borrow();
        let faces = publication_faces_for_overrides(
            declarations
                .get(&self.glyph_document.get())
                .map_or(&[], Vec::as_slice),
            overrides,
        );
        let mut fonts = self.fonts.borrow_mut();
        fonts.set_reader_overrides(overrides);
        fonts.set_active(&faces);
    }

    fn set_glyph_document(&self, document: usize) {
        self.glyph_document.set(document);
        let declarations = self.document_fonts.borrow();
        let faces = publication_faces_for_overrides(
            declarations.get(&document).map_or(&[], Vec::as_slice),
            &self.font_overrides.borrow(),
        );
        self.fonts.borrow_mut().set_active(&faces);
    }

    fn prepare_font_faces(&self, faces: &[FontFace], provider: Arc<dyn ResourceProvider>) {
        self.document_fonts
            .borrow_mut()
            .insert(self.glyph_document.get(), faces.to_vec());
        let faces = publication_faces_for_overrides(faces, &self.font_overrides.borrow());
        let selection_changed = {
            let mut fonts = self.fonts.borrow_mut();
            let had_loaded_faces = fonts.has_loaded_active_faces();
            let changed = fonts.set_active(&faces);
            changed && (had_loaded_faces || fonts.has_loaded_active_faces())
        };
        if selection_changed {
            self.send(HostMessage::FontSelectionChanged);
        }
        let pending = {
            let mut requested = self.font_requests.borrow_mut();
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
            let work = async move {
                match load_font_face(face, provider, &installed).await {
                    Ok(loaded) => {
                        let _ = messages.send(HostMessage::FontReady(loaded)).await;
                    }
                    Err(error) => eprintln!("publication font unavailable: {error}"),
                }
            };
            #[cfg(not(target_arch = "wasm32"))]
            self.executor.spawn(work).detach();
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(work);
        }
    }

    fn note_popup_width(&self) -> Option<f64> {
        self.note_width.get()
    }

    fn request_repaint(&self) {
        if !self.repaint_pending.replace(true) {
            self.send(HostMessage::Repaint);
        }
    }

    fn request_style(&self) {
        if !self.repaint_pending.replace(true) {
            self.send(HostMessage::Repaint);
        }
    }

    fn schedule(&self, delay: Duration, callback: Box<dyn FnOnce() + Send>) {
        let timer = self.executor.timer(delay);
        self.executor
            .spawn(async move {
                timer.await;
                callback();
            })
            .detach();
    }

    fn schedule_repaint(&self, delay: Duration) {
        if self.scheduled_repaint.swap(true, Ordering::Relaxed) {
            return;
        }
        let timer = self.executor.timer(delay);
        let messages = self.messages.clone();
        let scheduled_repaint = self.scheduled_repaint.clone();
        self.executor
            .spawn(async move {
                timer.await;
                scheduled_repaint.store(false, Ordering::Relaxed);
                let _ = messages.send(HostMessage::Repaint).await;
            })
            .detach();
    }

    fn blocking_job_spawner(&self) -> Arc<dyn BlockingJobSpawner> {
        Arc::new(GpuiBlockingJobSpawner {
            executor: self.executor.clone(),
        })
    }

    fn chapter_preparation_spawner(&self) -> Arc<dyn BlockingJobSpawner> {
        Arc::new(self.chapter_preparation.clone())
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

    fn set_clipboard(&self, text: &str) -> Result<(), String> {
        self.send(HostMessage::ClipboardText(text.to_owned()));
        Ok(())
    }

    fn set_clipboard_image(
        &self,
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    ) -> Result<(), String> {
        self.send(HostMessage::ClipboardImage {
            width,
            height,
            rgba,
        });
        Ok(())
    }

    fn set_clipboard_svg(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.send(HostMessage::ClipboardSvg(bytes));
        Ok(())
    }

    fn emit(&self, event: RendererEvent) {
        self.send(HostMessage::Event(event));
    }
}

pub struct HtmlView {
    host: Rc<GpuiHost>,
    renderer: RendererSession<GpuiTextCache>,
    appearance: appearance::PendingAppearance,
    focus: FocusHandle,
    images: ImageCache,
    document_origin: Cell<GpuiPoint<Pixels>>,
    paint_stats: PaintStats,
    layout_geometry: Option<HtmlLayoutGeometry>,
    prepared_viewport: Option<Size>,
    visible_page_key: Option<(usize, FrameGeometryCacheKey)>,
    prepared_paint: Option<PreparedPaint>,
    document_paint_resources: PaintResources,
    note_paint_resources: Option<PaintResources>,
    display_dirty: bool,
    pointer_active: bool,
    pointer_tap_start: Option<(GpuiPoint<Pixels>, Instant, bool)>,
    wheel_scroll_enabled: bool,
    ruler: ruler::ReadingRuler,
}

// Keep native text resources for the active document and open note even when
// shaping a different document replaces the shaper's current registry.
#[derive(Clone)]
struct PaintResources {
    generation: GlyphResourceGeneration,
    glyph_store: GlyphStore,
    run_store: RunStore,
}

impl PaintResources {
    fn capture(shaper: &GpuiTextCache) -> Self {
        Self {
            generation: shaper.resource_generation(),
            glyph_store: shaper.store(),
            run_store: shaper.run_store(),
        }
    }
}

#[derive(Clone)]
struct PreparedPaint {
    base: Arc<Vec<PaintCommand>>,
    overlay: Arc<Vec<PaintCommand>>,
    glyph_store: GlyphStore,
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
        paint_commands(paint.base.as_ref(), &paint.glyph_store, bounds, window, cx);
        paint_commands(
            paint.overlay.as_ref(),
            &paint.glyph_store,
            bounds,
            window,
            cx,
        );
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
    ) -> Self {
        let (messages, receiver) = async_channel::unbounded();
        let host = Rc::new(GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        ));
        let shaper = GpuiTextCache::for_window(window, cx)
            .with_publication_fonts(host.fonts.clone());
        let renderer = RendererSession::from_provider_with_nav(
            host.clone(),
            shaper,
            provider,
            document_uris,
            start_index,
            nav_state,
            config,
        );
        Self::from_renderer(host, receiver, renderer, window, cx)
    }

    pub fn from_preparation(
        prepared: skrift_column_paginator::RendererPreparation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, String> {
        Self::new_with_renderer(window, cx, move |host, shaper| {
            RendererSession::from_preparation(host, shaper, prepared)
        })
    }

    /// Complete fallible shaping before registering the GPUI entity.
    pub fn create_from_preparation(
        prepared: skrift_column_paginator::RendererPreparation,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>, String> {
        let (messages, receiver) = async_channel::unbounded();
        let host = Rc::new(GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        ));
        let shaper = GpuiTextCache::for_window(window, cx)
            .with_publication_fonts(host.fonts.clone());
        let renderer = RendererSession::from_preparation(host.clone(), shaper, prepared)?;
        Ok(cx.new(|cx| Self::from_renderer(host, receiver, renderer, window, cx)))
    }

    fn new_with_renderer(
        window: &mut Window,
        cx: &mut Context<Self>,
        make_renderer: impl FnOnce(
            Rc<GpuiHost>,
            GpuiTextCache,
        ) -> Result<RendererSession<GpuiTextCache>, String>,
    ) -> Result<Self, String> {
        let (messages, receiver) = async_channel::unbounded();
        let host = Rc::new(GpuiHost::new(
            messages,
            cx.background_executor().clone(),
            window.text_system().clone(),
            ChapterPreparationWorker::for_app(cx),
        ));
        let shaper = GpuiTextCache::for_window(window, cx)
            .with_publication_fonts(host.fonts.clone());
        let renderer = make_renderer(host.clone(), shaper)?;
        Ok(Self::from_renderer(host, receiver, renderer, window, cx))
    }

    fn from_renderer(
        host: Rc<GpuiHost>,
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
        let document_paint_resources = PaintResources::capture(renderer.glyph_shaper());
        Self {
            host,
            renderer,
            document_paint_resources,
            note_paint_resources: None,
            appearance: appearance::PendingAppearance::default(),
            focus,
            images: ImageCache::default(),
            document_origin: Cell::new(GpuiPoint::default()),
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
        }
    }

    fn sync_paint_resources(&mut self) {
        let shaper = self.renderer.glyph_shaper();
        let generation = shaper.resource_generation();
        if self.renderer.document_resource_generation() == generation {
            self.document_paint_resources = PaintResources::capture(shaper);
        }
        match self.renderer.note_resource_generation() {
            Some(note_generation) if note_generation == generation => {
                self.note_paint_resources = Some(PaintResources::capture(shaper));
            }
            Some(_) => {}
            None => self.note_paint_resources = None,
        }
    }

    pub fn apply(&mut self, command: RendererCommand, _cx: &mut Context<Self>) {
        self.sync_paint_resources();
        self.renderer.apply(command);
        self.sync_paint_resources();
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
    pub fn visible_source_text(&self) -> Option<VisibleSourceText> {
        self.renderer.visible_source_text()
    }
    pub fn source_cfi_range(&self, range: Range<u32>) -> Option<String> {
        self.renderer.source_cfi_range(range)
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
    pub fn advance_reading_document(&mut self, _cx: &mut Context<Self>) {
        self.renderer.advance_reading_document();
    }

    pub fn follow_reading_target(&mut self, target: ReadingTarget, _cx: &mut Context<Self>) {
        if self.renderer.follow_reading_target(target) {
            self.reset_ruler(true);
        }
    }

    /// Reflows to the width supplied by the containing GPUI layout without
    /// replacing the reader's preferred column width.
    pub fn set_available_width(&mut self, width: f64, _cx: &mut Context<Self>) {
        if self.renderer.set_available_width(width) {
            self.display_dirty = true;
        }
    }

    /// Returns whether the renderer is still loading or decoding resources.
    pub fn has_pending_resources(&self) -> bool {
        self.renderer.has_pending_resources()
    }

    pub fn set_interaction_palette(
        &mut self,
        palette: InteractionPalette,
        _cx: &mut Context<Self>,
    ) {
        self.renderer.set_interaction_palette(palette);
    }

    /// Opens the note a reference points at, as activating the reference
    /// would. Emits `FootnoteOpened` and reports whether a note was found.
    pub fn open_note(&mut self, href: &str, cx: &mut Context<Self>) -> bool {
        self.sync_paint_resources();
        let Some(preview) = self.renderer.note_preview(href) else {
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
            self.renderer.next_page();
        } else {
            self.renderer.previous_page();
        }
        self.display_dirty = true;
        // Pagination (especially a backward turn) settles during composition.
        // Compose once here so boundary detection and rapid repeated turns use
        // the destination, without waiting for asynchronous repaint messages.
        if let Some(viewport) = self.prepared_viewport {
            self.prepared_paint = Some(self.compose_viewport_display_list(viewport));
            self.display_dirty = false;
        }
        let moved =
            before != self.renderer.current_cfi() || self.renderer.reading_document().is_none();
        self.ruler_page_turn_completed(forward, moved);
        cx.notify();
        moved
    }

    /// Retain the displayed command buffers and glyphs without copying pixels.
    pub fn page_snapshot(&mut self) -> Option<HtmlPageSnapshot> {
        Some(HtmlPageSnapshot {
            paint: self.prepared_paint.clone()?,
            ruler: self.prepare_ruler(),
        })
    }

    pub fn next_line(&mut self, _cx: &mut Context<Self>) {
        self.renderer.next_line();
    }

    pub fn previous_line(&mut self, _cx: &mut Context<Self>) {
        self.renderer.previous_line();
    }

    pub fn next_section(&mut self, _cx: &mut Context<Self>) {
        self.renderer.next_document();
    }

    pub fn previous_section(&mut self, _cx: &mut Context<Self>) {
        self.renderer.previous_document();
    }

    /// Follows the document's link history, reporting whether it moved.
    pub fn navigate_history_back(&mut self, _cx: &mut Context<Self>) -> bool {
        self.renderer.navigate_history_back()
    }

    pub fn navigate_history_forward(&mut self, _cx: &mut Context<Self>) -> bool {
        self.renderer.navigate_history_forward()
    }

    pub fn change_root_font_size(&mut self, delta: f32, _cx: &mut Context<Self>) {
        self.renderer.change_root_font_size(delta);
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.renderer.resolve_href(href)
    }

    /// Restricts position events to publisher TOC anchors and immediately
    /// republishes the current position using the new filter.
    pub fn set_toc_anchor_strings_by_doc(
        &mut self,
        anchors_by_doc: Vec<Vec<String>>,
        _cx: &mut Context<Self>,
    ) {
        self.renderer.set_toc_anchor_strings_by_doc(anchors_by_doc);
        self.renderer.emit_state_snapshot();
    }

    pub fn navigate_to_href(&mut self, href: &str, _cx: &mut Context<Self>) -> bool {
        self.renderer.navigate_to_href(href)
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn selection_contains(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .selection_contains_point(self.layout_point(position))
    }

    pub fn select_word_at(&mut self, position: GpuiPoint<Pixels>, cx: &mut Context<Self>) -> bool {
        let selected = self.renderer.select_word_at(self.layout_point(position));
        if selected {
            cx.notify();
        }
        selected
    }

    pub fn select_element_at(
        &mut self,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self.renderer.select_element_at(self.layout_point(position));
        if selected {
            cx.notify();
        }
        selected
    }

    pub fn begin_selection_handle(&mut self, start: bool) -> bool {
        self.renderer.begin_selection_handle(start)
    }

    pub fn update_touch_selection(&mut self, position: GpuiPoint<Pixels>, cx: &mut Context<Self>) {
        if self
            .renderer
            .update_touch_selection(self.layout_point(position))
        {
            cx.notify();
        }
    }

    pub fn end_touch_selection(&mut self) {
        self.renderer.end_touch_selection();
    }

    pub fn clear_text_selection(&mut self, cx: &mut Context<Self>) {
        self.renderer.clear_text_selection();
        cx.notify();
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
    pub fn copy_selection_as_markdown(&self) -> bool {
        self.renderer.copy_selection()
    }

    fn copy_selection_as_plain_text(&self) -> bool {
        let text = self
            .renderer
            .note_selection_text()
            .or_else(|| self.renderer.selected_passage().map(|(_, text)| text));
        let Some(text) = text else {
            return self.renderer.copy_table_selection();
        };
        self.host.set_clipboard(&text).is_ok()
    }

    pub fn selection_endpoints(&self) -> (Option<GpuiPoint<Pixels>>, Option<GpuiPoint<Pixels>>) {
        let origin = self.document_origin.get();
        let convert = |point: Point| {
            GpuiPoint::new(
                origin.x + gpui::px(point.x as f32),
                origin.y + gpui::px(point.y as f32),
            )
        };
        let (start, end) = self.renderer.selection_endpoints();
        (start.map(convert), end.map(convert))
    }

    pub fn table_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer.table_at(self.layout_point(position))
    }

    pub fn image_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer.image_at(self.layout_point(position))
    }

    pub fn copy_image_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer.copy_image_at(self.layout_point(position))
    }

    pub fn equation_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer.equation_at(self.layout_point(position))
    }

    pub fn copy_equation_latex_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .copy_equation_latex_at(self.layout_point(position))
    }

    pub fn table_selection_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .table_selection_at(self.layout_point(position))
    }

    pub fn begin_table_selection_at(&mut self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .begin_table_selection_at(self.layout_point(position))
    }

    pub fn copy_table_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer.copy_table_at(self.layout_point(position))
    }

    pub fn copy_table_unstyled_html_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .copy_table_unstyled_html_at(self.layout_point(position))
    }

    pub fn copy_table_styled_html_at(&self, position: GpuiPoint<Pixels>) -> bool {
        self.renderer
            .copy_table_styled_html_at(self.layout_point(position))
    }

    pub fn copy_table_selection(&self) -> bool {
        self.renderer.copy_table_selection()
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
        let origin = self.document_origin.get();
        let point = |x: f64, y: f64| {
            GpuiPoint::new(origin.x + gpui::px(x as f32), origin.y + gpui::px(y as f32))
        };
        Some(Bounds::from_corners(
            point(bounds.x0, bounds.y0),
            point(bounds.x1, bounds.y1),
        ))
    }

    pub fn text_shape_cache_stats(&self) -> TextShapeCacheStats {
        self.renderer.glyph_shaper().shape_cache_stats()
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
    fn prepare_note_overlay(&mut self) -> Arc<Vec<PaintCommand>> {
        let mut painter = GpuiCommandPainter::commands_only(
            self.renderer.glyph_shaper().store(),
            self.renderer.glyph_shaper().run_store(),
            Vec::new(),
        );
        self.renderer.paint_note_selection(&mut painter);
        Arc::new(painter.into_commands())
    }

    fn note_pointer_down(
        &mut self,
        position: GpuiPoint<Pixels>,
        origin: GpuiPoint<Pixels>,
        semantic: bool,
    ) -> bool {
        self.renderer
            .note_pointer_down(note_local(position, origin), selection_mode(semantic))
    }

    fn note_pointer_move(
        &mut self,
        position: GpuiPoint<Pixels>,
        origin: GpuiPoint<Pixels>,
        semantic: bool,
    ) -> bool {
        self.renderer
            .note_pointer_move(note_local(position, origin), selection_mode(semantic))
    }

    fn note_pointer_up(&mut self, position: GpuiPoint<Pixels>, origin: GpuiPoint<Pixels>, semantic: bool) -> bool {
        self.renderer.note_pointer_up(note_local(position, origin), selection_mode(semantic))
    }

    /// Copies what is selected inside the note, if anything.
    pub fn copy_note_selection(&self) -> bool {
        let Some(text) = self.renderer.note_selection_text() else {
            return false;
        };
        self.host.set_clipboard(&text).is_ok()
    }

    /// Drops the open note so nothing selected in it outlives the panel.
    pub fn close_note(&mut self) {
        self.renderer.close_note();
        self.note_paint_resources = None;
    }

    pub fn prepare_note(&mut self) -> Option<PreparedNote> {
        self.sync_paint_resources();
        let scene = self.renderer.note_scene()?;
        let resources = self.note_paint_resources.as_ref()?;
        if Some(resources.generation) != self.renderer.note_resource_generation() {
            return None;
        }
        let glyph_store = resources.glyph_store.clone();
        let run_store = resources.run_store.clone();
        let mut painter =
            GpuiCommandPainter::new(&mut self.images, glyph_store, run_store, Vec::new())
                .image_theme(self.renderer.image_theme());
        scene.paint(&mut painter);
        let commands = painter.into_commands();
        println!(
            "HTML_NOTE_PREPARED commands={} height={:.1}",
            commands.len(),
            scene.content_height()
        );
        Some(PreparedNote {
            commands: Arc::new(commands),
            glyph_store: resources.glyph_store.clone(),
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
        self.document_origin.set(bounds.origin);
        if !self.appearance.commands.is_empty() {
            return self
                .prepared_paint
                .clone()
                .unwrap_or_else(|| PreparedPaint {
                    base: Arc::default(),
                    overlay: Arc::default(),
                    glyph_store: self.renderer.glyph_shaper().store(),
                });
        }
        self.host.note_width.set(Some(
            (f32::from(bounds.size.width) as f64 - FOOTNOTE_PANEL_PADDING * 2.0).max(1.0),
        ));
        let viewport = Size::new(
            f32::from(bounds.size.width) as f64,
            f32::from(bounds.size.height) as f64,
        );
        let viewport_ready = self.prepared_viewport == Some(viewport);
        // Commands invalidate geometry synchronously, but their repaint event
        // reaches this view asynchronously. A parent redraw in that interval
        // must compose the new page before querying its ruler/selection bounds.
        if viewport_ready
            && !self.display_dirty
            && !self.host.repaint_pending.get()
            && let Some(paint) = &self.prepared_paint
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
            self.host.emit(RendererEvent::VisiblePageChanged);
        }
        self.prepared_paint = Some(paint.clone());
        self.display_dirty = false;
        if self.layout_geometry != previous_geometry {
            cx.notify();
        }
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
        let glyph_store = self.document_paint_resources.glyph_store.clone();
        let run_store = self.document_paint_resources.run_store.clone();
        let mut base_painter = GpuiCommandPainter::new(
            &mut self.images,
            glyph_store.clone(),
            run_store.clone(),
            Vec::new(),
        )
        .image_theme(image_theme);
        let mut overlay_painter =
            GpuiCommandPainter::commands_only(glyph_store.clone(), run_store, Vec::new());
        let layer_started = Instant::now();
        frame.paint_all_layers(&mut base_painter, &mut overlay_painter);
        let layer_us = layer_started.elapsed().as_micros();
        let base = Arc::new(base_painter.into_commands());
        let overlay = Arc::new(overlay_painter.into_commands());
        let command_count = base.len() + overlay.len();
        let paint = PreparedPaint {
            base,
            overlay,
            glyph_store,
        };
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
            HostMessage::Repaint => {
                self.host.repaint_pending.set(false);
                self.display_dirty = true;
                cx.notify();
            }
            HostMessage::ResourceReady => {
                self.host
                    .resource_repaint_pending
                    .store(false, Ordering::Relaxed);
                self.renderer.update_resources();
                self.display_dirty = true;
                cx.notify();
            }
            HostMessage::FontSelectionChanged => {
                self.renderer.apply(RendererCommand::RefreshFonts);
                self.display_dirty = true;
                cx.notify();
            }
            HostMessage::FontReady(loaded) => {
                let result = self
                    .host
                    .fonts
                    .borrow_mut()
                    .register(loaded, &self.host.text_system);
                match result {
                    Ok(true) => {
                        self.renderer.apply(RendererCommand::RefreshFonts);
                        self.display_dirty = true;
                        cx.notify();
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("could not register publication font: {error}"),
                }
            }
            HostMessage::Event(event) => cx.emit(event),
            HostMessage::ClipboardText(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            HostMessage::ClipboardImage {
                width,
                height,
                rgba,
            } => {
                let Some(buffer) = RgbaImage::from_raw(width as u32, height as u32, rgba) else {
                    return;
                };
                if let Ok(image) = gpui::Image::from_rgba(&buffer) {
                    cx.write_to_clipboard(ClipboardItem::new_image(&image));
                }
            }
            HostMessage::ClipboardSvg(bytes) => {
                let image = gpui::Image::from_bytes(ImageFormat::Svg, bytes);
                cx.write_to_clipboard(ClipboardItem::new_image(&image));
            }
        }
        self.sync_paint_resources();
    }

    fn layout_point(&self, position: GpuiPoint<Pixels>) -> Point {
        let origin = self.document_origin.get();
        self.renderer.to_layout_point(Point::new(
            f32::from(position.x - origin.x) as f64,
            f32::from(position.y - origin.y) as f64,
        ))
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
        let position = self.layout_point(event.position);
        let image_hit = self.renderer.image_at(position);
        let options = PointerDownOptions {
            selection_mode: selection_mode(event.modifiers.control),
            copy_image: false,
        };
        self.pointer_active = self.renderer.pointer_down(position, options);
        self.pointer_tap_start =
            self.pointer_active
                .then_some((event.position, Instant::now(), image_hit));
        if self.pointer_active {
            Self::stop_event(cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let position = self.layout_point(event.position);
        let outcome = self
            .renderer
            .pointer_move(position, selection_mode(event.modifiers.control));
        if outcome.link_hover_changed {
            cx.notify();
        }
        if event.dragging() && outcome.handled {
            Self::stop_event(cx);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pointer_active {
            return;
        }
        self.pointer_active = false;
        let handled = self.renderer.pointer_up(self.layout_point(event.position), selection_mode(event.modifiers.control));
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
        if !self.renderer.scroll_vertical(y as f64) {
            return;
        }
        Self::stop_event(cx);
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
                self.copy_selection_as_markdown()
            } else {
                self.copy_selection_as_plain_text()
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
    }
}

impl Render for HtmlView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.schedule_appearance(window, cx);
        HtmlDocumentElement {
            view: cx.entity(),
            focus: self.focus.clone(),
            link_hovered: self.renderer.link_cursor_active(),
        }
    }
}
