use html::selection::SelectionInput;
use super::*;
use crate::{
    AnnotationStyle, BookSearchTarget, RendererAnnotation, RendererCommand, RendererSession,
    SearchOptions,
};

#[test]
fn publication_cfis_cover_resume_search_annotations_selection_and_narration() {
    let documents = vec!["one.html".to_owned(), "two.html".to_owned()];
    let (provider, _) = CountingProvider::new(vec![
        (
            "one.html",
            "<html><head></head><body><p>needle first chapter</p></body></html>",
        ),
        (
            "two.html",
            "<html><head></head><body><p>needle second chapter</p></body></html>",
        ),
    ]);
    let bases = Some(vec![Some("/8[spine]/2".into()), Some("/8[spine]/8".into())]);
    let mapping = crate::cfi::PublicationCfi::new(bases.clone());
    let config = RendererInitialConfig {
        publication_cfi_bases: bases,
        ..Default::default()
    };
    let services = RendererServices::default();
    let mut shaper = TestShaper::default();
    let mut core = RendererCore::from_provider_with_nav(
        services.clone(),
        &mut shaper,
        provider.clone(),
        documents.clone(),
        0,
        Some("epubcfi(/8/8!/4/2/1:0)"),
        config,
    )
    .expect("fixture document must load");
    let viewport = Size::new(600.0, 400.0);
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(core.nav.current_document(), 1);
    assert!(
        core.nav
            .current_cfi(core.nav_view())
            .unwrap()
            .starts_with("epubcfi(/8[spine]/8")
    );

    let hits = crate::search_publication_with_cfi(
        provider.as_ref(),
        &documents,
        &mapping,
        "needle",
        SearchOptions::default(),
        || false,
        |_| {},
    );
    assert_eq!(hits.len(), 2);
    let BookSearchTarget::SourceCfi(first) = &hits[0].target else {
        panic!("CFI search target")
    };
    assert!(first.starts_with("epubcfi(/8[spine]/2!"));
    core.apply(&mut shaper, RendererCommand::NavigateToCfi(first.clone()));
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(core.nav.current_document(), 0);
    let internal = crate::cfi::generate_cfi_range(&core.page, 0, 0, 6).unwrap();
    let full = mapping.to_publication(&internal).unwrap();
    core.apply(
        &mut shaper,
        RendererCommand::UpsertAnnotation(RendererAnnotation {
            id: "publication-note".into(),
            cfi_range: full,
            exact_text: "needle".into(),
            prefix: None,
            suffix: None,
            style: AnnotationStyle::Highlight,
            color: [255, 255, 0, 100],
        }),
    );
    assert_eq!(core.annotations.glyph_range("publication-note"), Some(0..6));

    core.interact_selection(SelectionInput::SetRange {
        range: 0..6,
        mode: crate::SelectionMode::Semantic,
    });
    let session = RendererSession::from_core(core, shaper);
    let (_, _, _, items) = session.reading_items().unwrap();
    let narration = session.reading_target_cfi(&items[0]).unwrap();
    assert!(narration.starts_with("epubcfi(/8[spine]/2!"));
    let (selected, _) = session.selected_passage().unwrap();
    assert!(selected.starts_with("epubcfi(/8[spine]/2!"));
}
