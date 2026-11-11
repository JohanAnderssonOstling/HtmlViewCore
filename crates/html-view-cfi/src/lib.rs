//! EPUB CFI syntax and laid-out source-position resolution.
//!
//! Layout supplies generic source paths; this module owns EPUB escaping,
//! spine addressing, range syntax, and recovery policy.

#[cfg(test)]
use html_pipeline::engine::LaidOutDocument;
use html::document::{
    SourceElementStep, SourcePosition as DocumentSourcePosition, SourceTextPosition,
};
use html::layout::RenderView;
use html_view_types::SourcePosition;
use std::fmt::Write;

mod syntax;
use syntax::{Location, ParsedPath, needs_escape, parse_fragment, synthetic_index};

mod publication;
pub use publication::PublicationCfi;

/// Serializes either a text address or an element address using EPUB CFI.
pub fn source_position_cfi(position: &SourcePosition) -> String {
    let mut result = format!("epubcfi(/6/{}!", (position.document + 1) * 2);
    for step in &position.path {
        write!(result, "/{step}").unwrap();
    }
    if let Some(offset) = position.utf16_offset {
        write!(result, ":{offset}").unwrap();
    }
    result.push(')');
    result
}

/// The start of a point or range CFI, without optional assertions.
pub fn parse_source_position(cfi: &str) -> Option<SourcePosition> {
    let fragment = parse_fragment(cfi)?;
    let document = synthetic_index(&fragment.package)?;
    let path = match fragment.location? {
        Location::Point(path) | Location::Range(path, _) => path,
    };
    Some(SourcePosition {
        document,
        path: path.steps,
        utf16_offset: path.offset,
    })
}

#[derive(Clone, Copy)]
enum ParseBoundary {
    Glyph,
    GlyphOrDocumentEnd,
}

pub fn parse_cfi_spine_only(cfi: &str) -> Option<usize> {
    synthetic_index(&parse_fragment(cfi)?.package)
}

pub fn generate_cfi<'a>(
    doc: impl Into<RenderView<'a>>,
    doc_index: usize,
    glyph_idx: u32,
) -> Option<String> {
    let doc = doc.into();
    let source = doc
        .addressing()
        .source_position_for_glyph(glyph_idx, false)?;
    let mut result = format!("epubcfi(/6/{}!", (doc_index + 1) * 2);
    append_document_position(&mut result, &source, 0);
    result.push(')');
    Some(result)
}

/// Builds an EPUB CFI from a parse-level source address.
pub fn generate_cfi_from_source_position(
    doc_index: usize,
    position: &SourceTextPosition,
) -> String {
    let mut result = format!("epubcfi(/6/{}!", (doc_index + 1) * 2);
    for step in position.element_steps() {
        write!(result, "/{step}").unwrap();
    }
    write!(
        result,
        "/{}:{})",
        position.text_step(),
        position.utf16_offset()
    )
    .unwrap();
    result
}

fn append_document_position(
    result: &mut String,
    position: &DocumentSourcePosition<'_>,
    skip: usize,
) {
    for element in position.elements().skip(skip) {
        append_element(result, element);
    }
    write!(
        result,
        "/{}:{}",
        position.text_step(),
        position.utf16_offset()
    )
    .unwrap();
}

fn append_element(result: &mut String, element: SourceElementStep<'_>) {
    write!(result, "/{}", element.step()).unwrap();
    if let Some(id) = element.id() {
        result.push('[');
        for character in id.chars() {
            if needs_escape(character) {
                result.push('^');
            }
            result.push(character);
        }
        result.push(']');
    }
}

/// Generates a standard EPUB CFI range for an exclusive glyph interval.
pub fn generate_cfi_range<'a>(
    doc: impl Into<RenderView<'a>>,
    doc_index: usize,
    start: u32,
    end: u32,
) -> Option<String> {
    let doc = doc.into();
    if start >= end {
        return None;
    }
    let addressing = doc.addressing();
    let start = addressing.source_position_for_glyph(start, false)?;
    let end = addressing.source_position_for_glyph(end - 1, true)?;
    Some(generate_cfi_range_from_source_positions(
        doc_index, &start, &end,
    ))
}

/// Serialize addresses already resolved by an engine operation.
pub fn generate_cfi_range_from_source_positions(
    doc_index: usize,
    start: &DocumentSourcePosition<'_>,
    end: &DocumentSourcePosition<'_>,
) -> String {
    let common_len = start
        .elements()
        .zip(end.elements())
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = format!("epubcfi(/6/{}!", (doc_index + 1) * 2);
    for element in start.elements().take(common_len) {
        append_element(&mut result, element);
    }
    result.push(',');
    append_document_position(&mut result, start, common_len);
    result.push(',');
    append_document_position(&mut result, end, common_len);
    result.push(')');
    result
}

pub fn parse_cfi<'a>(doc: impl Into<RenderView<'a>>, cfi: &str) -> Option<(usize, u32)> {
    let fragment = parse_fragment(cfi)?;
    let document = synthetic_index(&fragment.package)?;
    let Location::Point(path) = fragment.location? else {
        return None;
    };
    Some((
        document,
        resolve_document_position(doc.into(), &path, ParseBoundary::Glyph)?,
    ))
}

/// Resolves a standard range CFI to an exclusive glyph interval.
pub fn parse_cfi_range<'a>(doc: impl Into<RenderView<'a>>, cfi: &str) -> Option<(usize, u32, u32)> {
    let fragment = parse_fragment(cfi)?;
    let document = synthetic_index(&fragment.package)?;
    let Location::Range(start, end) = fragment.location? else {
        return None;
    };
    let doc = doc.into();
    let start = resolve_document_position(doc, &start, ParseBoundary::GlyphOrDocumentEnd)?;
    let end = resolve_document_position(doc, &end, ParseBoundary::GlyphOrDocumentEnd)?;
    (start < end).then_some((document, start, end))
}

fn resolve_document_position(
    doc: RenderView<'_>,
    path: &ParsedPath,
    boundary: ParseBoundary,
) -> Option<u32> {
    if path.steps.is_empty() {
        return Some(0);
    }
    let (elements, text_step) = match path.steps.last().copied() {
        Some(last) if !last.is_multiple_of(2) => (&path.steps[..path.steps.len() - 1], Some(last)),
        _ => (&path.steps[..], None),
    };
    doc.addressing().resolve_source_position(
        elements,
        text_step,
        path.offset,
        matches!(boundary, ParseBoundary::GlyphOrDocumentEnd),
    )
}

#[cfg(test)]
mod tests {
    use html_pipeline::engine::text_paint;

    use html::text::{FontMetrics, ShapedRun, TextRunShapeRequest, ShapedGlyph};

    use super::*;
    use html_pipeline::engine::LayoutConstraints;
    use html::{
        text::{FontSlant, GlyphHandle, GlyphResourceGeneration, GlyphShaper},
    };
    use html_pipeline::engine::GlyphResourceStore;
    use html_pipeline::DocumentFactory;
    use std::collections::HashMap;

    #[test]
    fn source_positions_roundtrip_text_elements_and_document_start() {
        for position in [
            SourcePosition {
                document: 2,
                path: vec![4, 12, 1],
                utf16_offset: Some(17),
            },
            SourcePosition {
                document: 1,
                path: vec![4, 8, 2],
                utf16_offset: None,
            },
            SourcePosition {
                document: 0,
                path: Vec::new(),
                utf16_offset: None,
            },
        ] {
            assert_eq!(
                parse_source_position(&source_position_cfi(&position)),
                Some(position)
            );
        }
    }

    #[test]
    fn source_position_uses_range_start_and_ignores_escaped_assertions() {
        assert_eq!(
            parse_source_position("epubcfi(/6/2!/4[body^,name],/12[section]/1:0,/12[section]/1:8)"),
            Some(SourcePosition {
                document: 0,
                path: vec![4, 12, 1],
                utf16_offset: Some(0)
            })
        );
        assert_eq!(
            parse_source_position("epubcfi(/6/2!/4/8[sec^]tion]/1:12)"),
            Some(SourcePosition {
                document: 0,
                path: vec![4, 8, 1],
                utf16_offset: Some(12)
            })
        );
        assert!(parse_source_position("epubcfi(/6/2!/4[unclosed/1:0)").is_none());
    }

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphHandle>,
        generation: GlyphResourceGeneration,
    }

    impl GlyphShaper for TestShaper {
        fn resource_generation(&self) -> GlyphResourceGeneration {
            self.generation
        }

        fn shape_run(&mut self, request: TextRunShapeRequest<'_>) -> ShapedRun {
            ShapedRun::characters(request, |character, style| {
                let key = (character, style.font_size.to_bits());
                let advance = style.font_size * 0.5;
                let metrics = FontMetrics {
                    ascent: style.font_size * 0.75,
                    descent: style.font_size * 0.25,
                };
                if let Some(glyph) = self.glyphs.get(&key) {
                    return ShapedGlyph::new(*glyph, advance, metrics);
                }
                let glyph = GlyphHandle(self.glyphs.len() as u32);
                self.glyphs.insert(key, glyph);
                ShapedGlyph::new(glyph, advance, metrics)
            })
        }
    }

    fn layout_html(source: &str, resources: &mut GlyphResourceStore) -> LaidOutDocument<'static> {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory
            .parse_with_new_pipeline(source, None)
            .shape(&mut shaper, resources)
            .expect("fixture shapes")
            .layout(
                LayoutConstraints::new(600.0, 20.0).expect("valid constraints"),
                resources,
            )
            .expect("test layout succeeds")
    }

    fn glyph_for_character(
        document: &LaidOutDocument,
        wanted: char,
        resources: &GlyphResourceStore,
    ) -> u32 {
        let text = document.render_view(resources).text();
        (0..text.glyph_count() as u32)
            .find(|index| {
                text_paint::glyph_at(text, *index as usize)
                    .and_then(|glyph| text_paint::glyph_metric(text, glyph))
                    .is_some_and(|metric| metric.ch() == wanted)
            })
            .expect("fixture character exists")
    }

    #[test]
    fn spine_steps_are_even_and_one_based() {
        assert_eq!(parse_cfi_spine_only("epubcfi(/6/2!/4/1:0)"), Some(0));
        assert_eq!(parse_cfi_spine_only("epubcfi(/6/10!/4/1:0)"), Some(4));
        assert_eq!(parse_cfi_spine_only("epubcfi(/6/1!/4/1:0)"), None);
    }

    #[test]
    fn escaped_commas_do_not_split_ranges() {
        assert_eq!(
            syntax::components("/4[id^,part],/1:0,/1:2", b',')
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn ranges_round_trip_utf16_boundaries() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><p>A😀B</p></body></html>",
            &mut document_resources,
        );
        let start = glyph_for_character(&document, '😀', &document_resources);
        let cfi = generate_cfi_range(
            (&document).render_view(&document_resources),
            0,
            start,
            start + 1,
        )
        .expect("range generated");

        assert!(
            cfi.contains(",/1:1,/1:3"),
            "emoji endpoints use UTF-16 offsets: {cfi}"
        );
        assert_eq!(
            parse_cfi_range((&document).render_view(&document_resources), &cfi),
            Some((0, start, start + 1))
        );
    }

    #[test]
    fn adjacent_text_nodes_share_one_source_chunk() {
        let mut document_resources = GlyphResourceStore::default();
        let document = layout_html(
            "<html><body><p>one<!--split-->two</p></body></html>",
            &mut document_resources,
        );
        let start = glyph_for_character(&document, 't', &document_resources);
        let cfi = generate_cfi_range(
            (&document).render_view(&document_resources),
            0,
            start,
            start + 2,
        )
        .expect("range generated");

        assert!(
            cfi.contains(",/1:3,/1:5"),
            "adjacent text nodes use cumulative offsets: {cfi}"
        );
        assert_eq!(
            parse_cfi_range((&document).render_view(&document_resources), &cfi),
            Some((0, start, start + 2))
        );
    }
}
