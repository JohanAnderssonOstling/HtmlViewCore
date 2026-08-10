//! A window over the renderer, for looking at what a change did.
//!
//! Continuous mode is a claim about where things land on a screen, and the
//! cheapest way to be wrong about it is to assert on numbers that look
//! plausible. This opens a real reader over a spine, driven through the same
//! public session a host uses, and lets you turn pages in it.
//!
//!     cargo run -p html-view-harness            # the built-in spine
//!     cargo run -p html-view-harness -- <dir>   # a directory of HTML files
//!
//! Keys: → / ← turn a page, ↓ / ↑ scroll a line, n / p change document,
//! c switch between continuous and one document at a time, g column rules,
//! + / - font size, [ / ] column width, q quit. Drag to select.

mod fixture;
mod paint;
mod text;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use html_view_core::{FileSystemProvider, RendererSession, NoteDisplay, PointerDownOptions, RendererEvent, RendererHost, RendererInitialConfig, ResourceProvider};
use kurbo::{Point, Size};
use peniko::Color;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use paint::BufferPainter;
use text::FontShaper;

const STATUS_HEIGHT: f64 = 28.0;

/// What the renderer tells the host, kept so the status line can show it.
#[derive(Default)]
struct HostState {
    repaint: bool,
    position: Option<(usize, Option<u32>)>,
}

struct HarnessHost {
    state: RefCell<HostState>,
}

impl RendererHost for HarnessHost {
    fn request_repaint(&self) {
        self.state.borrow_mut().repaint = true;
    }

    fn request_style(&self) {}

    fn schedule(&self, _delay: Duration, _work: Box<dyn FnOnce() + Send>) {}

    fn schedule_repaint(&self, _delay: Duration) {
        self.state.borrow_mut().repaint = true;
    }

    fn set_clipboard(&self, text: &str) -> Result<(), String> {
        println!("copied: {text}");
        Ok(())
    }

    fn set_clipboard_image(&self, _width: usize, _height: usize, _rgba: Vec<u8>) -> Result<(), String> {
        Err("the harness does not copy images".to_owned())
    }

    fn emit(&self, event: RendererEvent) {
        if let RendererEvent::PositionChanged { doc, glyph } = event {
            self.state.borrow_mut().position = Some((doc, glyph));
        }
    }
}

struct Reader {
    session: RendererSession<FontShaper>,
    host: Rc<HarnessHost>,
    uris: Vec<String>,
    provider: Arc<dyn ResourceProvider>,
    continuous: bool,
}

impl Reader {
    fn open(provider: Arc<dyn ResourceProvider>, uris: Vec<String>, continuous: bool, start: Option<(usize, f32, f64)>) -> Result<Self, String> {
        let host = Rc::new(HarnessHost { state: RefCell::new(HostState::default()) });
        let shaper = FontShaper::from_system_fonts()?;
        let (doc_index, font_size, column_width) = start.unwrap_or((0, 17.0, 340.0));
        let config = RendererInitialConfig {
            font_size,
            column_width,
            max_column_count: Some(4),
            note_display: NoteDisplay::default(),
            continuous_spine: continuous,
            ..RendererInitialConfig::default()
        };
        let session = RendererSession::from_provider_with_nav(host.clone(), shaper, provider.clone(), uris.clone(), doc_index, None, config);
        Ok(Self { session, host, uris, provider, continuous })
    }

    /// Continuity is fixed when a reader is built, so switching it opens the
    /// spine again at the same place.
    fn set_continuous(&mut self, continuous: bool) {
        let doc_index = self.host.state.borrow().position.map_or(0, |(doc, _)| doc);
        let start = Some((doc_index, self.session.root_font_size(), self.session.column_width()));
        if let Ok(reader) = Reader::open(self.provider.clone(), self.uris.clone(), continuous, start) {
            *self = reader;
        }
    }
}

struct Harness {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    reader: Reader,
    rules: bool,
    selecting: bool,
    pointer: Point,
}

impl Harness {
    fn status(&self) -> String {
        let (doc, glyph) = self.reader.host.state.borrow().position.unwrap_or((0, None));
        let uri = self.reader.uris.get(doc).map(String::as_str).unwrap_or("?");
        let mode = if self.reader.continuous { "continuous spine" } else { "one document at a time" };
        format!("{mode}   ·   {} of {}   {uri}   glyph {}", doc + 1, self.reader.uris.len(), glyph.map_or_else(|| "-".to_owned(), |glyph| glyph.to_string()))
    }

    fn draw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        let (width, height) = (size.width.max(1) as usize, size.height.max(1) as usize);
        // Read off what the harness itself draws before the frame borrows the
        // session for painting.
        let (column_width, status, rules) = (self.reader.session.column_width(), self.status(), self.rules);
        let Some(surface) = self.surface.as_mut() else { return };
        surface.resize(std::num::NonZeroU32::new(width as u32).unwrap(), std::num::NonZeroU32::new(height as u32).unwrap()).expect("the surface must resize with the window");
        let mut buffer = surface.buffer_mut().expect("the surface must hand out a buffer");

        let page = Size::new(width as f64, (height as f64 - STATUS_HEIGHT).max(1.0));
        // A prepared frame hands out the shaper beside the painting half,
        // which is exactly what a painter drawing glyphs by id needs.
        let (shaper, frame) = self.reader.session.prepare_frame(page).into_parts();
        let mut painter = BufferPainter::new(&mut buffer, width, height, shaper);
        painter.clear(Color::rgba8(0xff, 0xff, 0xff, 0xff));
        frame.paint(&mut painter);

        if rules {
            let mut x = 0.0;
            while x < width as f64 {
                painter.fill(kurbo::Rect::new(x, 0.0, x + 1.0, page.height), Color::rgba8(0xdd, 0xe1, 0xe8, 0xff));
                x += column_width;
            }
        }

        painter.fill(kurbo::Rect::new(0.0, page.height, width as f64, height as f64), Color::rgba8(0xf2, 0xf4, 0xf7, 0xff));
        painter.fill(kurbo::Rect::new(0.0, page.height, width as f64, page.height + 1.0), Color::rgba8(0xd6, 0xdb, 0xe3, 0xff));
        painter.label(&status, 12.0, height as f64 - 9.0, 12.0, Color::rgba8(0x3a, 0x42, 0x50, 0xff));

        buffer.present().expect("the buffer must be presentable");
    }
}

impl ApplicationHandler for Harness {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes().with_title("html-view spine harness").with_inner_size(winit::dpi::LogicalSize::new(1180.0, 760.0));
        let window = Rc::new(event_loop.create_window(attributes).expect("a window must open"));
        let context = softbuffer::Context::new(window.clone()).expect("a drawing context must exist");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("a drawing surface must exist"));
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Point::new(position.x, position.y);
                if self.selecting {
                    self.reader.session.pointer_move(self.pointer, false);
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                match state {
                    ElementState::Pressed => {
                        self.selecting = true;
                        self.reader.session.pointer_down(self.pointer, PointerDownOptions::default());
                    }
                    ElementState::Released => {
                        self.selecting = false;
                        self.reader.session.pointer_up();
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
    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        let session = &mut self.reader.session;
        match key.as_ref() {
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::Space) | Key::Named(NamedKey::PageDown) => session.next_page(),
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::PageUp) => session.previous_page(),
            Key::Named(NamedKey::ArrowDown) => session.next_line(),
            Key::Named(NamedKey::ArrowUp) => session.previous_line(),
            Key::Named(NamedKey::Escape) => event_loop.exit(),
            Key::Character("n") => session.next_document(),
            Key::Character("p") => session.previous_document(),
            Key::Character("q") => event_loop.exit(),
            Key::Character("g") => self.rules = !self.rules,
            Key::Character("+") | Key::Character("=") => session.change_root_font_size(1.0),
            Key::Character("-") => session.change_root_font_size(-1.0),
            Key::Character("]") => session.change_column_width(20.0),
            Key::Character("[") => session.change_column_width(-20.0),
            Key::Character("c") => {
                let continuous = !self.reader.continuous;
                self.reader.set_continuous(continuous);
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

    let continuous = std::env::var("HARNESS_CONTINUOUS").map_or(true, |value| value != "0");
    let reader = Reader::open(provider, uris, continuous, None)?;
    let event_loop = EventLoop::new().map_err(|error| error.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut harness = Harness { window: None, surface: None, reader, rules: false, selecting: false, pointer: Point::ZERO };
    event_loop.run_app(&mut harness).map_err(|error| error.to_string())
}
