//! Let settings paint before running synchronous document reflow.

use std::mem::{discriminant, take};

use gpui::{Context, Window};
use skrift_column_paginator::RendererCommand;

use super::HtmlView;

#[derive(Default)]
pub(super) struct PendingAppearance {
    pub(super) commands: Vec<RendererCommand>,
    scheduled: bool,
    revision: usize,
}

impl HtmlView {
    /// Retain the latest value of each appearance command until the settings
    /// frame has painted. Navigation commands still use `apply` directly.
    pub fn queue_appearance(&mut self, command: RendererCommand, cx: &mut Context<Self>) {
        let kind = discriminant(&command);
        self.appearance.commands.retain(|pending| discriminant(pending) != kind);
        self.appearance.commands.push(command);
        self.appearance.revision = self.appearance.revision.wrapping_add(1);
        cx.notify();
    }

    pub(super) fn schedule_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance.commands.is_empty() || self.appearance.scheduled {
            return;
        }
        self.appearance.scheduled = true;
        let revision = self.appearance.revision;
        let view = cx.weak_entity();
        // Scheduling from render guarantees the current settings frame is
        // presented before the callback can begin its expensive reflow.
        window.on_next_frame(move |_, cx| {
            let _ = view.update(cx, |view, cx| {
                view.appearance.scheduled = false;
                if view.appearance.revision != revision {
                    cx.notify();
                    return;
                }
                for command in take(&mut view.appearance.commands) {
                    view.renderer.apply(command);
                }
                view.display_dirty = true;
                cx.notify();
            });
        });
    }
}
