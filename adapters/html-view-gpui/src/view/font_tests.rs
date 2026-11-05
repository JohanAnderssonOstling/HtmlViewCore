use super::*;
use gpui::TestAppContext;
use skrift_column_paginator::{RendererInitialConfig, ResourceProvider};
use std::io;
use std::sync::atomic::AtomicUsize;

struct FontBook {
    font_reads: AtomicUsize,
}
impl ResourceProvider for FontBook {
    fn read_bytes(&self, uri: &str) -> io::Result<Vec<u8>> {
        if uri == "reader.ttf" {
            self.font_reads.fetch_add(1, Ordering::SeqCst);
            return Ok(include_bytes!("../../tests/fixtures/NV_Garamond-Regular.ttf").to_vec());
        }
        Ok(format!(
            r#"<html><head><style>
            @font-face {{ font-family: Publisher; src: url(reader.ttf) }}
            p {{ font-family: Publisher, serif; margin: 0 }}
        </style></head><body><p>Chapter {uri} text</p></body></html>"#
        )
        .into_bytes())
    }
    fn exists(&self, _: &str) -> bool {
        false
    }
    fn resolve(&self, _: &str, href: &str) -> String {
        href.into()
    }
    fn list_html_candidates(&self, _: &str) -> io::Result<Vec<String>> {
        Ok(vec![])
    }
}

#[gpui::test]
fn publication_fonts_load_refresh_shaping_and_reuse_across_chapters(cx: &mut TestAppContext) {
    let provider = Arc::new(FontBook { font_reads: AtomicUsize::new(0) });
    let book = provider.clone();
    let (document, visual) = cx.add_window_view(|window, cx| {
        HtmlView::from_provider(
            book,
            vec!["one.html".into(), "two.html".into()],
            0,
            None,
            RendererInitialConfig::default(),
            window,
            cx,
        )
        .expect("fixture document must load")
    });
    visual.run_until_parked();
    document.update(visual, |view, cx| {
        assert!(
            view.renderer.revisions().layout >= 2,
            "completed fonts must reflow the document"
        );
        let position = view.current_cfi();
        view.apply(RendererCommand::RefreshFonts, cx);
        assert_eq!(view.current_cfi(), position);
        view.next_section(cx);
    });
    visual.run_until_parked();
    document.update(visual, |view, _| assert_eq!(view.reading_document(), Some(1)));
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 1, "matching faces are reused across chapters");
}

#[gpui::test]
fn reader_font_overrides_disable_loading_and_can_restore_cached_publisher_fonts(cx: &mut TestAppContext) {
    let provider = Arc::new(FontBook { font_reads: AtomicUsize::new(0) });
    let book = provider.clone();
    let mut config = RendererInitialConfig::default();
    // Deliberately collide with the publication alias: this must select the
    // reader's installed font/fallback, never the book's embedded face.
    config.style_overrides.font_family = Some("Publisher, serif".into());
    let (document, visual) = cx.add_window_view(|window, cx| {
        HtmlView::from_provider(book, vec!["one.html".into()], 0, None, config, window, cx)
            .expect("fixture document must load")
    });
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 0);
    document.update(visual, |view, cx| {
        assert!(view.host.font_requests.is_empty());
        view.apply(
            RendererCommand::SetReaderStyleOverrides(Default::default()),
            cx,
        );
    });
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 1);
    document.update(visual, |view, cx| {
        let position = view.current_cfi();
        let overrides = skrift_column_paginator::ReaderStyleOverrides {
            serif_font_family: Some("Publisher, serif".into()),
            ..Default::default()
        };
        view.apply(RendererCommand::SetReaderStyleOverrides(overrides), cx);
        assert_eq!(view.current_cfi(), position);
    });
    visual.run_until_parked();
    document.update(visual, |view, cx| {
        view.apply(
            RendererCommand::SetReaderStyleOverrides(Default::default()),
            cx,
        );
    });
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 1, "returning to publisher fonts reuses the loaded face");
}
