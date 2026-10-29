//! A reader window holding one document, for exercising footnote behaviour by
//! hand.
//!
//! Click a numbered marker: the note is laid out on its own, to the width of
//! the panel below, and painted there with the document's own typography. The
//! note bodies carry a pink background in the fixture, so seeing one in the
//! page means it was not held back, and seeing one in the panel is correct.
//!
//! Select text in the note by dragging; press `c` to copy it.
//! Press `1`-`4` to open a note by reference without clicking. Press `m` to switch between popup and as-authored presentation. Under
//! as-authored the notes read in place and a marker navigates instead.
//!
//! Run with: `cargo run --release --example footnote_popup -p html-view-gpui`

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window, div, px, rgb, size};
use skrift_column_paginator::{FileSystemProvider, FootnotePreview, NoteDisplay, RendererCommand, RendererEvent, RendererInitialConfig, ResourceProvider};
use html_view_gpui::{HtmlNoteElement, HtmlView, PreparedNote};

const FIXTURE: &str = "footnotes.xhtml";

struct Harness {
    document: Entity<HtmlView>,
    note: Option<(FootnotePreview, PreparedNote)>,
    display: NoteDisplay,
    focus: FocusHandle,
}

impl Focusable for Harness {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = match self.display {
            NoteDisplay::Popup => "popup  (press m for as-authored)",
            NoteDisplay::AsAuthored => "as-authored  (press m for popup)",
        };

        let mut root = div()
            .track_focus(&self.focus)
            .key_context("FootnoteHarness")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                if let Some(href) = match key {
                    "1" => Some("#fn-block"),
                    "2" => Some("#fn-mixed"),
                    "3" => Some("#fn-list"),
                    "4" => Some("#fn-narrow"),
                    _ => None,
                } {
                    let opened = this.document.update(cx, |view, cx| view.open_note(href, cx));
                    eprintln!("HARNESS_OPEN_NOTE href={href} opened={opened}");
                    cx.notify();
                    return;
                }
                if key == "c" {
                    let copied = this.document.update(cx, |view, _| view.copy_note_selection());
                    eprintln!("HARNESS_COPY_NOTE_SELECTION copied={copied}");
                    return;
                }
                if key == "m" {
                    this.display = match this.display {
                        NoteDisplay::Popup => NoteDisplay::AsAuthored,
                        NoteDisplay::AsAuthored => NoteDisplay::Popup,
                    };
                    // A mode switch re-lays-out the document, so any note held
                    // open describes a document that no longer exists.
                    this.note = None;
                    let display = this.display;
                    this.document.update(cx, |view, cx| view.apply(RendererCommand::SetNoteDisplay(display), cx));
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .child(div().flex_none().px(px(18.0)).py(px(8.0)).bg(rgb(0xf2f3f5)).text_color(rgb(0x40454f)).child(format!("notes: {mode}")))
            .child(div().flex_1().overflow_hidden().child(self.document.clone()));

        if let Some((preview, prepared)) = self.note.clone() {
            root = root.child(
                div()
                    .flex_none()
                    .max_h(px(260.0))
                    .px(px(18.0))
                    .py(px(12.0))
                    .bg(rgb(0xf7f7f9))
                    .border_t_1()
                    .border_color(rgb(0xd8dae0))
                    .child(div().text_color(rgb(0x6b7280)).child(format!("note  {}", preview.href)))
                    .child(HtmlNoteElement::new(self.document.clone(), prepared)),
            );
        }
        root
    }
}

fn main() {
    let fixture_root: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/fixtures/footnote-popup");
    assert!(fixture_root.join(FIXTURE).exists(), "fixture missing at {}", fixture_root.display());

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(1100.0), px(860.0)), cx);
        cx.open_window(gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(bounds)), ..Default::default() }, |window, cx| {
            let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
            let config = RendererInitialConfig { font_size: 19.0, column_width: 900.0, max_column_count: Some(1), ..RendererInitialConfig::default() };
            let uri = fixture_root.join(FIXTURE).canonicalize().expect("fixture path").to_string_lossy().into_owned();
            let document = cx.new(|cx| HtmlView::from_provider(provider, vec![uri], 0, None, config, window, cx));

            cx.new(|cx| {
                cx.subscribe(&document, |harness: &mut Harness, _, event: &RendererEvent, cx| {
                    eprintln!("HARNESS_EVENT {event:?}");
                    if let RendererEvent::FootnoteOpened(preview) = event {
                        eprintln!("HARNESS_NOTE_OPENED href={} anchored={}", preview.href, preview.anchor.is_some());
                        // Prepared here, outside rendering: preparing during
                        // layout would re-enter the view being rendered.
                        let Some(prepared) = harness.document.update(cx, |view, _| view.prepare_note()) else { return };
                        if std::env::var("HARNESS_PREPARE_ONLY").is_ok() {
                            // Built, then dropped: nothing is stored and nothing
                            // renders, isolating creation from painting.
                            eprintln!("HARNESS_PREPARE_ONLY dropping prepared note");
                            drop(prepared);
                            return;
                        }
                        harness.note = Some((preview.clone(), prepared));
                        cx.notify();
                    }
                })
                .detach();
                let focus = cx.focus_handle();
                Harness { document, note: None, display: NoteDisplay::Popup, focus }
            })
        })
        .expect("open footnote harness window");
        cx.activate(true);
    });
}
