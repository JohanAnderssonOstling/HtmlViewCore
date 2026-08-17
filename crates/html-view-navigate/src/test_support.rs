//! Shared helpers for unit tests of the renderer's extracted components.

use std::cell::RefCell;
use std::time::Duration;

use html_view_types::{RendererEvent, RendererHost};

/// A [`RendererHost`] that records what it was told, for components whose
/// observable behavior is which events they emit.
#[derive(Default)]
pub(crate) struct RecordingHost {
    events: RefCell<Vec<RendererEvent>>,
}

impl RecordingHost {
    pub(crate) fn events(&self) -> Vec<RendererEvent> {
        self.events.borrow().clone()
    }

    pub(crate) fn count(&self) -> usize {
        self.events.borrow().len()
    }

    pub(crate) fn clear(&self) {
        self.events.borrow_mut().clear();
    }
}

impl RendererHost for RecordingHost {
    fn request_repaint(&self) {}
    fn request_style(&self) {}
    fn schedule(&self, _delay: Duration, callback: Box<dyn FnOnce() + Send>) {
        callback();
    }
    fn schedule_repaint(&self, _delay: Duration) {}
    fn schedule_frame_work(&self, _delay: Duration, work: Box<dyn FnOnce() + Send>) { work(); }
    fn resource_waker(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> { None }
    fn set_clipboard(&self, _text: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_clipboard_image(&self, _width: usize, _height: usize, _rgba: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn emit(&self, event: RendererEvent) {
        self.events.borrow_mut().push(event);
    }
}

/// History availability pairs, in emission order.
pub(crate) fn availability(host: &RecordingHost) -> Vec<(bool, bool)> {
    host.events()
        .into_iter()
        .filter_map(|event| match event {
            RendererEvent::HistoryAvailability { back, forward } => Some((back, forward)),
            _ => None,
        })
        .collect()
}
