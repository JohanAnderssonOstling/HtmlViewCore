//! EPUB CFI syntax and laid-out source-position resolution.
//!
//! Layout supplies generic source paths; this module owns EPUB escaping,
//! spine addressing, range syntax, and recovery policy.

#[cfg(test)]
use html_pipeline::engine::LaidOutDocument;
use html::document::{SourcePosition as DocumentSourcePosition, SourceTextPosition};
use html::layout::RenderView;
use html_view_types::SourcePosition;
use std::borrow::Cow;
use std::fmt::Write;

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

/// The start of a point or range CFI, without optional ID assertions.
pub fn parse_source_position(cfi: &str) -> Option<SourcePosition> {
    let document = parse_cfi_spine_only(cfi)?;
    let (_, content) = cfi
        .strip_prefix("epubcfi(")?
        .strip_suffix(')')?
        .split_once('!')?;
    let parts = split_range_components(content);
    let start = match parts.as_slice() {
        [point] => Cow::Borrowed(*point),
        [common, start, _] => Cow::Owned(format!(
            "{}/{}",
            common.trim_end_matches('/'),
            start.trim_start_matches('/')
        )),
        _ => return None,
    };
    let mut plain = String::new();
    let mut escaped = false;
    let mut assertion = false;
    for character in start.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '^' => escaped = true,
            '[' => assertion = true,
            ']' => assertion = false,
            _ if !assertion => plain.push(character),
            _ => {}
        }
    }
    if escaped || assertion {
        return None;
    }
    let (path, utf16_offset) = match plain.split_once(':') {
        Some((path, offset)) => (path, Some(leading_usize(offset)?)),
        None => (plain.as_str(), None),
    };
    let path = path
        .split('/')
        .filter(|step| !step.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<usize>, _>>()
        .ok()?;
    if path.contains(&0) {
        return None;
    }
    Some(SourcePosition {
        document,
        path,
        utf16_offset,
    })
}

#[derive(Clone, Copy)]
enum ParseBoundary {
    Glyph,
    GlyphOrDocumentEnd,
}

pub fn parse_cfi_spine_only(cfi: &str) -> Option<usize> {
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (spine_part, _) = inner.split_once('!')?;
    let step = spine_part
        .strip_prefix("/6/")?
        .split('[')
        .next()?
        .parse::<usize>()
        .ok()?;
    (step >= 2 && step.is_multiple_of(2)).then(|| step / 2 - 1)
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
    append_document_position(&mut result, &source);
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

fn append_document_position(result: &mut String, position: &DocumentSourcePosition<'_>) {
    for element in position.elements() {
        write!(result, "/{}", element.step()).unwrap();
        if let Some(id) = element.id() {
            result.push('[');
            for character in id.chars() {
                if matches!(character, '^' | '[' | ']' | '(' | ')' | ',' | ';' | '=') {
                    result.push('^');
                }
                result.push(character);
            }
            result.push(']');
        }
    }
    write!(
        result,
        "/{}:{}",
        position.text_step(),
        position.utf16_offset()
    )
    .unwrap();
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
    let mut start_path = String::new();
    let mut end_path = String::new();
    append_document_position(&mut start_path, start);
    append_document_position(&mut end_path, end);
    let start_segments = start_path.split('/').filter(|segment| !segment.is_empty());
    let end_segments = end_path.split('/').filter(|segment| !segment.is_empty());
    let common_len = start_segments
        .clone()
        .zip(end_segments.clone())
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = format!("epubcfi(/6/{}!/", (doc_index + 1) * 2);
    append_segments(&mut result, start_segments.clone().take(common_len));
    result.push_str(",/");
    append_segments(&mut result, start_segments.skip(common_len));
    result.push_str(",/");
    append_segments(&mut result, end_segments.skip(common_len));
    result.push(')');
    result
}

fn append_segments<'a>(result: &mut String, segments: impl Iterator<Item = &'a str>) {
    for (index, segment) in segments.enumerate() {
        if index > 0 {
            result.push('/');
        }
        result.push_str(segment);
    }
}

pub fn parse_cfi<'a>(doc: impl Into<RenderView<'a>>, cfi: &str) -> Option<(usize, u32)> {
    let doc = doc.into();
    let doc_index = parse_cfi_spine_only(cfi)?;
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (_, document_part) = inner.split_once('!')?;
    Some((
        doc_index,
        parse_document_position(doc, document_part, ParseBoundary::Glyph)?,
    ))
}

/// Resolves a standard range CFI to an exclusive glyph interval.
pub fn parse_cfi_range<'a>(doc: impl Into<RenderView<'a>>, cfi: &str) -> Option<(usize, u32, u32)> {
    let doc = doc.into();
    let doc_index = parse_cfi_spine_only(cfi)?;
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (_, document_range) = inner.split_once('!')?;
    let parts = split_range_components(document_range);
    if parts.len() != 3 {
        return None;
    }
    let combine = |relative: &str| {
        format!(
            "{}/{}",
            parts[0].trim_end_matches('/'),
            relative.trim_start_matches('/')
        )
    };
    let start =
        parse_document_position(doc, &combine(parts[1]), ParseBoundary::GlyphOrDocumentEnd)?;
    let end = parse_document_position(doc, &combine(parts[2]), ParseBoundary::GlyphOrDocumentEnd)?;
    (start < end).then_some((doc_index, start, end))
}

fn parse_document_position(
    doc: RenderView<'_>,
    document_part: &str,
    boundary: ParseBoundary,
) -> Option<u32> {
    let mut segments = document_part
        .split('/')
        .filter(|segment| !segment.is_empty())
        .peekable();
    let mut steps = Vec::new();
    let mut character_offset = None;
    while let Some(segment) = segments.next() {
        let step = if segments.peek().is_none() {
            match segment.split_once(':') {
                Some((step, offset)) => {
                    character_offset = Some(leading_usize(offset)?);
                    step
                }
                None => segment,
            }
        } else {
            segment
        };
        steps.push(leading_usize(step)?);
    }
    if steps.is_empty() {
        return Some(0);
    }
    let (elements, text_step) = match steps.last().copied() {
        Some(last) if !last.is_multiple_of(2) => (&steps[..steps.len() - 1], Some(last)),
        _ => (&steps[..], None),
    };
    doc.addressing().resolve_source_position(
        elements,
        text_step,
        character_offset,
        matches!(boundary, ParseBoundary::GlyphOrDocumentEnd),
    )
}

fn split_range_components(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut assertion_depth = 0usize;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '^' => escaped = true,
            '[' => assertion_depth += 1,
            ']' => assertion_depth = assertion_depth.saturating_sub(1),
            ',' if assertion_depth == 0 => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn leading_usize(value: &str) -> Option<usize> {
    let length = value.bytes().take_while(u8::is_ascii_digit).count();
    value[..length].parse().ok()
}

#[cfg(test)]
mod tests {
    use html_pipeline::engine::text_paint;

    use html::text::{FontMetrics, GlyphRequest, ShapedGlyph};

    use super::*;
    use html_pipeline::engine::LayoutConstraints;
    use html::{
        text::{FontSlant, GlyphHandle, GlyphResourceGeneration, GlyphShaper, ShapeError},
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
        append_checkpoint: Option<(HashMap<(char, u32), GlyphHandle>, GlyphResourceGeneration)>,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.generation = GlyphResourceGeneration::default();
        }

        fn resource_generation(&self) -> GlyphResourceGeneration {
            self.generation
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::text::ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.generation.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.generation = glyph_store;
            }
        }

        fn shape_glyph(&mut self, request: GlyphRequest<'_>) -> Result<ShapedGlyph, ShapeError> {
            let key = (request.character, request.style.font_size.to_bits());
            let advance = request.style.font_size * 0.5;
            let metrics = FontMetrics {
                ascent: request.style.font_size * 0.75,
                descent: request.style.font_size * 0.25,
            };
            if let Some(glyph) = self.glyphs.get(&key) {
                return Ok(ShapedGlyph::new(*glyph, advance, metrics));
            }
            let glyph = GlyphHandle(self.glyphs.len() as u32);
            self.glyphs.insert(key, glyph);
            Ok(ShapedGlyph::new(glyph, advance, metrics))
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
        assert_eq!(split_range_components("/4[id^,part],/1:0,/1:2").len(), 3);
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
