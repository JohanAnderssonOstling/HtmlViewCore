use super::*;
use gpui::{TestAppContext, px, size};
use skrift_column_paginator::RendererInitialConfig;
use std::io;

struct Chapters;

impl ResourceProvider for Chapters {
    fn read_bytes(&self, uri: &str) -> io::Result<Vec<u8>> {
        let count = if uri == "long.html" { 100 } else { 3 };
        let text = (0..count).map(|index| format!("<p>Chapter paragraph {index}.</p>")).collect::<String>();
        Ok(format!("<html><head><style>body {{ margin: 24px }} p {{ margin: 16px 0; line-height: 28px }}</style></head><body>{text}</body></html>").into_bytes())
    }

    fn exists(&self, _: &str) -> bool {
        true
    }

    fn resolve(&self, _: &str, href: &str) -> String {
        href.to_owned()
    }

    fn list_html_candidates(&self, _: &str) -> io::Result<Vec<String>> {
        Ok(vec!["short.html".into(), "next.html".into()])
    }
}

#[gpui::test]
fn backward_returns_to_short_chapters_start_at_the_top(cx: &mut TestAppContext) {
    let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(Chapters), vec!["short.html".into(), "next.html".into()], 0, None, config, window, cx));
    visual.simulate_resize(size(px(600.0), px(500.0)));
    visual.run_until_parked();
    let original = document.read_with(visual, |view, _| view.renderer.visible_reading_lines());
    for section_command in [false, true] {
        document.update(visual, |view, cx| view.next_section(cx));
        visual.run_until_parked();
        document.update(visual, |view, cx| {
            assert_eq!(view.reading_document(), Some(1));
            if section_command {
                view.previous_section(cx);
            } else {
                view.previous_page(cx);
            }
        });
        visual.run_until_parked();
        document.update(visual, |view, _| {
            assert_eq!(view.reading_document(), Some(0));
            let lines = view.renderer.visible_reading_lines();
            assert_eq!(lines, original, "returning to a short chapter must preserve its original top alignment and authored spacing");
        });
    }
}

#[gpui::test]
fn backward_returns_to_long_chapters_keep_the_last_page(cx: &mut TestAppContext) {
    let config = RendererInitialConfig { max_column_count: Some(1), ..Default::default() };
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(Arc::new(Chapters), vec!["long.html".into(), "next.html".into()], 1, None, config, window, cx));
    visual.simulate_resize(size(px(600.0), px(500.0)));
    visual.run_until_parked();
    document.update(visual, |view, cx| view.previous_page(cx));
    visual.run_until_parked();
    document.update(visual, |view, _| {
        assert_eq!(view.reading_document(), Some(0));
        assert!(view.renderer.visible_reading_lines()[0].glyphs.start > 0, "a long chapter must still reopen at its last page");
    });
}
