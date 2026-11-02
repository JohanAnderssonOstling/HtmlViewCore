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
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(book, vec!["one.html".into(), "two.html".into()], 0, None, RendererInitialConfig::default(), window, cx));
    visual.run_until_parked();
    document.update(visual, |view, _| {
        assert!(view.host.fonts.borrow().generation() >= 2, "declarations and loaded faces reach the backend");
        assert!(view.renderer.revisions().shape >= 2, "completed fonts must reflow the document");
        let position = view.current_cfi();
        view.renderer.apply(RendererCommand::RefreshFonts);
        assert_eq!(view.current_cfi(), position);
        view.renderer.next_document();
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
    let (document, visual) = cx.add_window_view(|window, cx| HtmlView::from_provider(book, vec!["one.html".into()], 0, None, config, window, cx));
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 0);
    document.update(visual, |view, _| {
        assert!(view.host.font_requests.borrow().is_empty());
        assert!(!view.host.fonts.borrow().has_loaded_active_faces());
        view.renderer.apply(RendererCommand::SetReaderStyleOverrides(Default::default()));
    });
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 1);
    document.update(visual, |view, _| {
        assert!(view.host.fonts.borrow().has_loaded_active_faces());
        let position = view.current_cfi();
        let overrides = skrift_column_paginator::ReaderStyleOverrides { serif_font_family: Some("Publisher, serif".into()), ..Default::default() };
        view.renderer.apply(RendererCommand::SetReaderStyleOverrides(overrides));
        assert!(!view.host.fonts.borrow().allows_publication_family_list(Some("Publisher, serif")));
        assert!(view.host.fonts.borrow().allows_publication_family_list(Some("Publisher, sans-serif")));
        assert_eq!(view.current_cfi(), position);
    });
    visual.run_until_parked();
    document.update(visual, |view, _| {
        view.renderer.apply(RendererCommand::SetReaderStyleOverrides(Default::default()));
        assert!(view.host.fonts.borrow().has_loaded_active_faces());
    });
    visual.run_until_parked();
    assert_eq!(provider.font_reads.load(Ordering::SeqCst), 1, "returning to publisher fonts reuses the loaded face");
}
