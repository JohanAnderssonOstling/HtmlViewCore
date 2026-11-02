use super::*;
use gpui::TestAppContext;
use skrift_column_paginator::{RendererInitialConfig, ResourceProvider};
use std::io;

struct SettingsBook;
impl ResourceProvider for SettingsBook {
    fn read_bytes(&self, _: &str) -> io::Result<Vec<u8>> {
        Ok(format!("<html><body><p>{}</p></body></html>", "Readable paragraph. ".repeat(60)).into_bytes())
    }
    fn exists(&self, _: &str) -> bool {
        true
    }
    fn resolve(&self, _: &str, href: &str) -> String {
        href.into()
    }
    fn list_html_candidates(&self, _: &str) -> io::Result<Vec<String>> {
        Ok(vec![])
    }
}

#[gpui::test]
fn settings_frame_keeps_the_previous_document_before_applying_latest_appearance(cx: &mut TestAppContext) {
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(SettingsBook), vec!["chapter.html".into()], 0, None, RendererInitialConfig::default(), window, cx));
    visual.run_until_parked();
    let (shape, position, paint) = document.read_with(visual, |view, _| (view.renderer.revisions().shape, view.current_cfi(), view.prepared_paint.clone().unwrap()));
    visual.update(|window, cx| {
        document.update(cx, |view, cx| {
            for font_size in [22.0, 24.0, 26.0] {
                view.queue_appearance(RendererCommand::SetFontSize(font_size), cx);
            }
            assert_eq!(view.appearance.commands.len(), 1);
            assert!(matches!(view.appearance.commands[0], RendererCommand::SetFontSize(26.0)));
            assert_eq!(view.renderer.revisions().shape, shape, "changing the setting must not synchronously reflow");
        });
        let _ = window.draw(cx);
        document.read_with(cx, |view, _| {
            assert_eq!(view.renderer.revisions().shape, shape, "the settings frame must paint before reflow");
            assert!(Arc::ptr_eq(&view.prepared_paint.as_ref().unwrap().base, &paint.base));
            assert!(!view.appearance.commands.is_empty());
        });
    });
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    visual.run_until_parked();
    document.read_with(visual, |view, _| {
        assert!(view.appearance.commands.is_empty());
        assert!(view.renderer.revisions().shape > shape);
        assert_eq!(view.current_cfi(), position, "deferred settings preserve the reading position");
        assert_eq!(view.reading_document(), Some(0));
    });
}

#[gpui::test]
fn viewport_changes_reflow_immediately_when_resize_debouncing_is_disabled(cx: &mut TestAppContext) {
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(SettingsBook), vec!["chapter.html".into()], 0, None, RendererInitialConfig::default(), window, cx));
    visual.run_until_parked();
    let layout = document.read_with(visual, |view, _| view.renderer.revisions().layout);
    visual.update(|window, cx| {
        document.update(cx, |view, cx| {
            view.queue_appearance(RendererCommand::SetColumnViewportPlan { available_width: 400.0, column_width: 300.0, column_count: 1, column_gap: 24.0 }, cx);
            assert!(view.appearance.commands.is_empty());
            assert!(view.renderer.revisions().layout > layout);
        });
        let _ = window.draw(cx);
        document.read_with(cx, |view, _| {
            assert!(view.appearance.commands.is_empty());
            assert!(view.renderer.revisions().layout > layout);
        });
    });
}

#[gpui::test]
fn another_settings_change_gets_its_own_frame_before_reflow(cx: &mut TestAppContext) {
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(SettingsBook), vec!["chapter.html".into()], 0, None, RendererInitialConfig::default(), window, cx));
    visual.run_until_parked();
    let shape = document.read_with(visual, |view, _| view.renderer.revisions().shape);
    visual.update(|window, cx| {
        document.update(cx, |view, cx| view.queue_appearance(RendererCommand::SetFontSize(22.0), cx));
        let _ = window.draw(cx);
        document.update(cx, |view, cx| view.queue_appearance(RendererCommand::SetFontSize(26.0), cx));
        window.simulate_next_frame(cx);
        document.read_with(cx, |view, _| assert_eq!(view.renderer.revisions().shape, shape, "a newer setting must get a frame before its queued reflow runs"));
        let _ = window.draw(cx);
        window.simulate_next_frame(cx);
        document.read_with(cx, |view, _| {
            assert!(view.appearance.commands.is_empty());
            assert!(view.renderer.revisions().shape > shape);
        });
    });
}

#[gpui::test]
fn settings_can_paint_while_the_first_document_frame_is_still_pending(cx: &mut TestAppContext) {
    let (document, visual) = cx.add_window_view(|window, cx| {
        let mut view = HtmlView::from_provider(Arc::new(SettingsBook), vec!["chapter.html".into()], 0, None, RendererInitialConfig::default(), window, cx);
        view.queue_appearance(RendererCommand::SetFontSize(28.0), cx);
        view
    });
    visual.update(|window, cx| {
        let _ = window.draw(cx);
        document.read_with(cx, |view, _| {
            assert!(view.prepared_paint.is_none(), "settings should not wait for the first document display list");
            assert_eq!(view.appearance.commands.len(), 1);
        });
        window.simulate_next_frame(cx);
        let _ = window.draw(cx);
        document.read_with(cx, |view, _| {
            assert!(view.appearance.commands.is_empty());
            assert!(view.prepared_paint.is_some());
        });
    });
}
