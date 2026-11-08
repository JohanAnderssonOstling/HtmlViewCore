//! A window over the renderer, for looking at what a change did.
//!
//! This opens a real reader over a spine, driven through the same public
//! session a host uses, and lets you turn pages in it.
//!
//!     cargo run -p html-view-harness            # the built-in spine
//!     cargo run -p html-view-harness -- <dir>   # a directory of HTML files
//!
//! Keys: → / ← turn a page, ↓ / ↑ scroll a line, n / p change document,
//! g toggles column rules, + / - changes font size, [ / ] changes column
//! width, Ctrl/Cmd+C copies, and q quits. Drag to select.

mod fixture;
mod paint;
mod text;

use std::rc::Rc;
use std::sync::Arc;

use skrift_column_paginator::{
    FileSystemProvider, FootnotePopupAnchor, NoteDisplay, PointerDownOptions, RendererEvent,
    ReadingPosition, RendererServices, RendererEffects, ClipboardWrite, RendererInitialConfig,
    RendererSession, RendererCommand, ResourceProvider, SelectionMode,
};
use kurbo::{Point, Size};
use peniko::Color;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use paint::BufferPainter;
use text::{FontShaper, RasterCache};

const STATUS_HEIGHT: f64 = 28.0;
const POPUP_PADDING: f64 = 12.0;

#[derive(Clone, Copy)]
struct PopupGeometry {
    rect: kurbo::Rect,
    content_origin: Point,
}

impl PopupGeometry {
    fn contains(self, point: Point) -> bool {
        point.x >= self.rect.x0
            && point.x <= self.rect.x1
            && point.y >= self.rect.y0
            && point.y <= self.rect.y1
    }

    fn local_point(self, point: Point) -> Point {
        point - self.content_origin.to_vec2()
    }
}

/// What the renderer tells the host, kept so the status line can show it.
#[derive(Default)]
struct HostState {
    repaint: bool,
    position: Option<ReadingPosition>,
    footnote_anchor: Option<FootnotePopupAnchor>,
}

struct Reader {
    session: RendererSession<FontShaper>,
    rasters: RasterCache,
    state: HostState,
    clipboard: arboard::Clipboard,
    effects: RendererEffects,
    uris: Vec<String>,
}

impl Reader {
    fn open(
        provider: Arc<dyn ResourceProvider>,
        uris: Vec<String>,
        start: Option<(usize, f32, f64)>,
    ) -> Result<Self, String> {
        let clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
        let shaper = FontShaper::from_system_fonts()?;
        let (doc_index, font_size, column_width) = start.unwrap_or((0, 17.0, 340.0));
        let config = RendererInitialConfig {
            font_size,
            column_width,
            max_column_count: Some(4),
            note_display: NoteDisplay::default(),
            ..RendererInitialConfig::default()
        };
        let mut session = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            shaper,
            provider.clone(),
            uris.clone(),
            doc_index,
            None,
            config,
        )?;
        session.set_note_popup_width(Some(336.0));
        Ok(Self {
            session,
            rasters: RasterCache::default(),
            state: HostState::default(),
            clipboard,
            effects: RendererEffects::default(),
            uris,
        })
    }
    fn flush_effects(&mut self) {
        self.session.drain_effects(&mut self.effects);
        self.state.repaint |= std::mem::take(&mut self.effects.repaint);
        self.effects.restyle = false;
        for event in self.effects.events.drain(..) {
            match event {
                RendererEvent::ReadingPositionChanged(position) => {
                    self.state.position = Some(position)
                }
                RendererEvent::FootnoteOpened(preview) => {
                    self.state.footnote_anchor = preview.anchor;
                    self.state.repaint = true;
                }
                RendererEvent::OperationFailed { operation, message } => {
                    eprintln!("{operation}: {message}")
                }
                _ => {}
            }
        }
        for write in self.effects.clipboard.drain(..) {
            let result = match write {
                ClipboardWrite::Text(text) => self
                    .clipboard
                    .set_text(text)
                    .map_err(|error| error.to_string()),
                ClipboardWrite::Image { .. } | ClipboardWrite::Svg(_) => {
                    Err("the harness does not copy images".to_owned())
                }
            };
            if let Err(message) = result {
                eprintln!("clipboard: {message}");
            }
        }
        // The harness shapes with system fonts only.
        self.effects.font_loads.clear();
    }
}

struct Harness {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    reader: Reader,
    rules: bool,
    selecting: bool,
    selecting_note: bool,
    pointer: Point,
    popup_geometry: Option<PopupGeometry>,
    modifiers: ModifiersState,
}

impl Harness {
    fn status(&self) -> String {
        let state = &self.reader.state;
        let doc = state
            .position
            .as_ref()
            .map_or(0, |position| position.source.document);
        let uri = self.reader.uris.get(doc).map(String::as_str).unwrap_or("?");
        let location = state
            .position
            .as_ref()
            .map_or(1, |position| position.location);
        format!(
            "{} of {}   {uri}   location {location}",
            doc + 1,
            self.reader.uris.len()
        )
    }

    fn draw(&mut self) {
        self.reader.flush_effects();
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (width, height) = (size.width.max(1) as usize, size.height.max(1) as usize);
        // Read off what the harness itself draws before the frame borrows the
        // session for painting.
        let (column_width, status, rules, footnote_anchor, note_scene) = (
            self.reader.session.column_width(),
            self.status(),
            self.rules,
            self.reader.state.footnote_anchor,
            self.reader.session.note_scene(),
        );
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        surface
            .resize(
                std::num::NonZeroU32::new(width as u32).unwrap(),
                std::num::NonZeroU32::new(height as u32).unwrap(),
            )
            .expect("the surface must resize with the window");
        let mut buffer = surface
            .buffer_mut()
            .expect("the surface must hand out a buffer");

        let page = Size::new(width as f64, (height as f64 - STATUS_HEIGHT).max(1.0));
        self.popup_geometry = footnote_anchor.zip(note_scene.as_ref()).map(|(anchor, scene)| {
            let anchor = FootnotePopupAnchor::at_click(anchor.point, page);
            let popup_size = Size::new(
                scene.size().width + POPUP_PADDING * 2.0,
                scene.content_height() + POPUP_PADDING * 2.0,
            );
            let origin = anchor.popup_origin(popup_size);
            PopupGeometry {
                rect: kurbo::Rect::from_origin_size(origin, popup_size),
                content_origin: origin + kurbo::Vec2::new(POPUP_PADDING, POPUP_PADDING),
            }
        });
        // A prepared frame hands out the shaper beside the painting half,
        // which is exactly what a painter drawing glyphs by id needs.
        let (shaper, frame) = self.reader.session.prepare_frame(page).into_parts();
        let mut painter =
            BufferPainter::new(&mut buffer, width, height, shaper, &mut self.reader.rasters);
        painter.clear(Color::rgba8(0xff, 0xff, 0xff, 0xff));
        frame.paint_with_note_selection(&mut painter, |painter| {
            if rules {
                let mut x = 0.0;
                while x < width as f64 {
                    painter.fill(
                        kurbo::Rect::new(x, 0.0, x + 1.0, page.height),
                        Color::rgba8(0xdd, 0xe1, 0xe8, 0xff),
                    );
                    x += column_width;
                }
            }

            if let (Some(geometry), Some(scene)) = (self.popup_geometry, note_scene) {
                painter.fill(
                    geometry.rect + kurbo::Vec2::new(4.0, 5.0),
                    Color::rgba8(0x25, 0x2a, 0x32, 0x45),
                );
                painter.fill(
                    geometry.rect,
                    Color::rgba8(0xff, 0xfd, 0xf4, 0xff),
                );
                painter.set_offset(geometry.content_origin);
                scene.paint(painter);
            }
        });
        painter.set_offset(Point::ZERO);

        painter.fill(
            kurbo::Rect::new(0.0, page.height, width as f64, height as f64),
            Color::rgba8(0xf2, 0xf4, 0xf7, 0xff),
        );
        painter.fill(
            kurbo::Rect::new(0.0, page.height, width as f64, page.height + 1.0),
            Color::rgba8(0xd6, 0xdb, 0xe3, 0xff),
        );
        painter.label(
            &status,
            12.0,
            height as f64 - 9.0,
            12.0,
            Color::rgba8(0x3a, 0x42, 0x50, 0xff),
        );

        buffer.present().expect("the buffer must be presentable");
    }
}

impl ApplicationHandler for Harness {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes()
            .with_title("html-view spine harness")
            .with_inner_size(winit::dpi::LogicalSize::new(1180.0, 760.0));
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .expect("a window must open"),
        );
        let context =
            softbuffer::Context::new(window.clone()).expect("a drawing context must exist");
        self.surface = Some(
            softbuffer::Surface::new(&context, window.clone())
                .expect("a drawing surface must exist"),
        );
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Point::new(position.x, position.y);
                if self.selecting_note {
                    if let Some(geometry) = self.popup_geometry {
                        self.reader.session.note_pointer_move(
                            geometry.local_point(self.pointer),
                            SelectionMode::Plain,
                            self.selecting_note,
                            geometry.contains(self.pointer),
                        );
                    }
                    self.request_redraw();
                } else if self.selecting {
                    self.reader.session.pointer_move(
                        self.pointer,
                        SelectionMode::Plain,
                        self.selecting,
                    );
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                match state {
                    ElementState::Pressed => {
                        if let Some(geometry) = self
                            .popup_geometry
                            .filter(|geometry| geometry.contains(self.pointer))
                        {
                            self.selecting_note = true;
                            self.selecting = false;
                            self.reader.session.note_pointer_down(
                                geometry.local_point(self.pointer),
                                SelectionMode::Plain,
                            );
                        } else {
                            self.close_popup();
                            self.selecting = true;
                            self.reader
                                .session
                                .pointer_down(self.pointer, PointerDownOptions::default());
                        }
                    }
                    ElementState::Released => {
                        if self.selecting_note {
                            self.selecting_note = false;
                            if let Some(geometry) = self.popup_geometry {
                                self.reader.session.note_pointer_up(geometry.local_point(self.pointer), SelectionMode::Plain);
                            }
                        } else {
                            self.selecting = false;
                            self.reader.session.pointer_up(self.pointer, SelectionMode::Plain);
                        }
                    }
                }
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, lines) => -f64::from(lines),
                    winit::event::MouseScrollDelta::PixelDelta(position) => -position.y,
                };
                let step = if scroll < 0.0 {
                    -1
                } else if scroll > 0.0 {
                    1
                } else {
                    0
                };
                if step != 0 {
                    self.reader.session.apply(RendererCommand::NavigateLine(step));
                    self.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.on_key(event_loop, event.logical_key);
                self.request_redraw();
            }
            _ => {}
        }
        self.reader.flush_effects();
        if std::mem::take(&mut self.reader.state.repaint) {
            self.request_redraw();
        }
    }
}

impl Harness {
    fn close_popup(&mut self) {
        self.reader.state.footnote_anchor = None;
        self.reader.session.close_note();
        self.popup_geometry = None;
        self.selecting_note = false;
    }

    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        let session = &mut self.reader.session;
        match key.as_ref() {
            Key::Named(NamedKey::ArrowRight)
            | Key::Named(NamedKey::Space)
            | Key::Named(NamedKey::PageDown) => session.apply(RendererCommand::NavigatePage(1)),
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::PageUp) => {
                session.apply(RendererCommand::NavigatePage(-1))
            }
            Key::Named(NamedKey::ArrowDown) => session.apply(RendererCommand::NavigateLine(1)),
            Key::Named(NamedKey::ArrowUp) => session.apply(RendererCommand::NavigateLine(-1)),
            Key::Named(NamedKey::Escape) => {
                if self.reader.state.footnote_anchor.is_some() {
                    self.close_popup();
                } else {
                    event_loop.exit();
                }
            }
            Key::Character("n") => session.apply(RendererCommand::NavigateDocument(1)),
            Key::Character("p") => session.apply(RendererCommand::NavigateDocument(-1)),
            Key::Character("q") => event_loop.exit(),
            Key::Character("g") => self.rules = !self.rules,
            Key::Character("+") | Key::Character("=") => session.apply(RendererCommand::SetFontSize(
                session.render_view().root_font_size() + 1.0,
            )),
            Key::Character("-") => session.apply(RendererCommand::SetFontSize(
                session.render_view().root_font_size() - 1.0,
            )),
            Key::Character("]") => session.apply(RendererCommand::SetColumnWidth(
                session.column_width() + 20.0,
            )),
            Key::Character("[") => session.apply(RendererCommand::SetColumnWidth(
                session.column_width() - 20.0,
            )),
            Key::Character("c")
                if self.modifiers.control_key() || self.modifiers.super_key() =>
            {
                session.copy_selection();
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), String> {
    let root = match std::env::args().nth(1) {
        Some(path) => std::path::PathBuf::from(path),
        None => fixture::write_builtin_spine()?,
    };
    let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
    let mut uris = provider.list_html_candidates(&root.to_string_lossy()).map_err(|error| format!("{}: {error}", root.display()))?;
    uris.sort();
    if uris.is_empty() {
        return Err(format!("no HTML documents under {}", root.display()));
    }
    println!("{} documents from {}", uris.len(), root.display());

    let reader = Reader::open(provider, uris, None)?;
    let event_loop = EventLoop::new().map_err(|error| error.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut harness = Harness {
        window: None,
        surface: None,
        reader,
        rules: false,
        selecting: false,
        selecting_note: false,
        pointer: Point::ZERO,
        popup_geometry: None,
        modifiers: ModifiersState::default(),
    };
    event_loop
        .run_app(&mut harness)
        .map_err(|error| error.to_string())
}
