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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use html_view_core::{
    FileSystemProvider, FootnotePopupAnchor, NoteDisplay, PointerDownOptions, RendererEvent,
    RendererHost, RendererInitialConfig, RendererSession, ResourceProvider, SelectionMode,
};
use kurbo::{Point, Size};
use peniko::Color;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use paint::BufferPainter;
use text::{ActiveDocument, FontShaper};

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
    position: Option<(usize, Option<u32>)>,
    footnote_anchor: Option<FootnotePopupAnchor>,
}

struct HarnessHost {
    state: RefCell<HostState>,
    clipboard: RefCell<arboard::Clipboard>,
    /// The renderer says which document it is about to shape or paint, and the
    /// shaper reads its table for that one. Popup notes can come from another
    /// spine item, and a glyph id only means something alongside the document
    /// it was shaped from.
    active_document: ActiveDocument,
}

impl RendererHost for HarnessHost {
    fn note_popup_width(&self) -> Option<f64> {
        Some(336.0)
    }

    fn request_repaint(&self) {
        self.state.borrow_mut().repaint = true;
    }

    fn request_style(&self) {}

    fn schedule(&self, _delay: Duration, _work: Box<dyn FnOnce() + Send>) {}

    fn schedule_repaint(&self, _delay: Duration) {
        self.state.borrow_mut().repaint = true;
    }

    fn schedule_frame_work(&self, _delay: Duration, work: Box<dyn FnOnce() + Send>) {
        work();
        self.state.borrow_mut().repaint = true;
    }

    fn resource_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        None
    }

    fn set_clipboard(&self, text: &str) -> Result<(), String> {
        self.clipboard
            .borrow_mut()
            .set_text(text.to_owned())
            .map_err(|error| error.to_string())
    }

    fn set_clipboard_image(
        &self,
        _width: usize,
        _height: usize,
        _rgba: Vec<u8>,
    ) -> Result<(), String> {
        Err("the harness does not copy images".to_owned())
    }

    fn set_glyph_document(&self, doc: usize) {
        self.active_document.set(doc);
    }

    fn emit(&self, event: RendererEvent) {
        match event {
            RendererEvent::PositionChanged { doc, glyph } => {
                self.state.borrow_mut().position = Some((doc, glyph));
            }
            RendererEvent::FootnoteOpened(preview) => {
                let mut state = self.state.borrow_mut();
                state.footnote_anchor = preview.anchor;
                state.repaint = true;
            }
            _ => {}
        }
    }
}

struct Reader {
    session: RendererSession<FontShaper>,
    host: Rc<HarnessHost>,
    uris: Vec<String>,
}

impl Reader {
    fn open(
        provider: Arc<dyn ResourceProvider>,
        uris: Vec<String>,
        start: Option<(usize, f32, f64)>,
    ) -> Result<Self, String> {
        let active_document = ActiveDocument::default();
        let host = Rc::new(HarnessHost {
            state: RefCell::new(HostState::default()),
            clipboard: RefCell::new(
                arboard::Clipboard::new().map_err(|error| error.to_string())?,
            ),
            active_document: active_document.clone(),
        });
        let shaper = FontShaper::from_system_fonts(active_document)?;
        let (doc_index, font_size, column_width) = start.unwrap_or((0, 17.0, 340.0));
        let config = RendererInitialConfig {
            font_size,
            column_width,
            max_column_count: Some(4),
            note_display: NoteDisplay::default(),
            ..RendererInitialConfig::default()
        };
        let session = RendererSession::from_provider_with_nav(
            host.clone(),
            shaper,
            provider.clone(),
            uris.clone(),
            doc_index,
            None,
            config,
        );
        Ok(Self {
            session,
            host,
            uris,
        })
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
        let (doc, glyph) = self.reader.host.state.borrow().position.unwrap_or((0, None));
        let uri = self.reader.uris.get(doc).map(String::as_str).unwrap_or("?");
        format!("{} of {}   {uri}   glyph {}", doc + 1, self.reader.uris.len(), glyph.map_or_else(|| "-".to_owned(), |glyph| glyph.to_string()))
    }

    fn draw(&mut self) {
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
            self.reader.host.state.borrow().footnote_anchor,
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
        let mut painter = BufferPainter::new(&mut buffer, width, height, shaper);
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
                        self.reader
                            .session
                            .note_pointer_move(
                                geometry.local_point(self.pointer),
                                SelectionMode::Plain,
                            );
                    }
                    self.request_redraw();
                } else if self.selecting {
                    self.reader
                        .session
                        .pointer_move(self.pointer, SelectionMode::Plain);
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
                            self.reader.session.note_pointer_up();
                        } else {
                            self.selecting = false;
                            self.reader.session.pointer_up();
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
                if self.reader.session.scroll_vertical(scroll) {
                    self.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.on_key(event_loop, event.logical_key);
                self.request_redraw();
            }
            _ => {}
        }
    }
}

impl Harness {
    fn close_popup(&mut self) {
        self.reader.host.state.borrow_mut().footnote_anchor = None;
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
            | Key::Named(NamedKey::PageDown) => session.next_page(),
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::PageUp) => {
                session.previous_page()
            }
            Key::Named(NamedKey::ArrowDown) => session.next_line(),
            Key::Named(NamedKey::ArrowUp) => session.previous_line(),
            Key::Named(NamedKey::Escape) => {
                if self.reader.host.state.borrow().footnote_anchor.is_some() {
                    self.close_popup();
                } else {
                    event_loop.exit();
                }
            }
            Key::Character("n") => session.next_document(),
            Key::Character("p") => session.previous_document(),
            Key::Character("q") => event_loop.exit(),
            Key::Character("g") => self.rules = !self.rules,
            Key::Character("+") | Key::Character("=") => session.change_root_font_size(1.0),
            Key::Character("-") => session.change_root_font_size(-1.0),
            Key::Character("]") => session.change_column_width(20.0),
            Key::Character("[") => session.change_column_width(-20.0),
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
