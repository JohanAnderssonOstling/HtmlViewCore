//! EPUB CFI syntax and laid-out source-position resolution.
//!
//! Layout supplies generic source paths; this module owns EPUB escaping,
//! spine addressing, range syntax, and recovery policy.

use html::layout::LaidOutDocument;

#[derive(Clone, Copy)]
enum GlyphBoundary {
    Before,
    After,
}

#[derive(Clone, Copy)]
enum ParseBoundary {
    Glyph,
    GlyphOrDocumentEnd,
}

pub fn parse_cfi_spine_only(cfi: &str) -> Option<usize> {
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (spine_part, _) = inner.split_once('!')?;
    let step = spine_part.strip_prefix("/6/")?.split('[').next()?.parse::<usize>().ok()?;
    (step >= 2 && step.is_multiple_of(2)).then(|| step / 2 - 1)
}

fn escape_assertion(value: &str) -> String {
    value.chars().flat_map(|character| if matches!(character, '^' | '[' | ']' | '(' | ')' | ',' | ';' | '=') { [Some('^'), Some(character)] } else { [Some(character), None] }).flatten().collect()
}

pub fn generate_cfi(doc: &LaidOutDocument, doc_index: usize, glyph_idx: u32) -> Option<String> {
    let position = generate_document_position(doc, glyph_idx, GlyphBoundary::Before)?;
    Some(format!("epubcfi(/6/{}!{position})", (doc_index + 1) * 2))
}

/// Builds an EPUB CFI from a parse-level source address. Whole-book search
/// later resolves this same DOM path to a laid-out glyph.
pub fn generate_cfi_from_source_position(doc_index: usize, position: &html::parse::SourceTextPosition) -> String {
    let mut document_position = String::new();
    for step in position.element_steps() {
        document_position.push_str(&format!("/{step}"));
    }
    document_position.push_str(&format!("/{}:{}", position.text_step(), position.utf16_offset()));
    format!("epubcfi(/6/{}!{document_position})", (doc_index + 1) * 2)
}

fn generate_document_position(doc: &LaidOutDocument, glyph_idx: u32, boundary: GlyphBoundary) -> Option<String> {
    let position = doc
        .render_view()
        .addressing()
        .source_position_for_glyph(glyph_idx, matches!(boundary, GlyphBoundary::After))?;
    let mut result = String::new();
    for element in position.elements() {
        result.push_str(&format!("/{}", element.step()));
        if let Some(id) = element.id() {
            result.push_str(&format!("[{}]", escape_assertion(id)));
        }
    }
    result.push_str(&format!("/{}:{}", position.text_step(), position.utf16_offset()));
    Some(result)
}

/// Generates a standard EPUB CFI range for an exclusive glyph interval.
pub fn generate_cfi_range(doc: &LaidOutDocument, doc_index: usize, start: u32, end: u32) -> Option<String> {
    if start >= end {
        return None;
    }
    let start_path = generate_document_position(doc, start, GlyphBoundary::Before)?;
    let end_path = generate_document_position(doc, end - 1, GlyphBoundary::After)?;
    let start_segments: Vec<&str> = start_path.split('/').filter(|segment| !segment.is_empty()).collect();
    let end_segments: Vec<&str> = end_path.split('/').filter(|segment| !segment.is_empty()).collect();
    let common_len = start_segments.iter().zip(&end_segments).take_while(|(left, right)| left == right).count();
    let common = start_segments[..common_len].join("/");
    let start_relative = start_segments[common_len..].join("/");
    let end_relative = end_segments[common_len..].join("/");
    Some(format!("epubcfi(/6/{}!/{common},/{start_relative},/{end_relative})", (doc_index + 1) * 2))
}

pub fn parse_cfi(doc: &LaidOutDocument, cfi: &str) -> Option<(usize, u32)> {
    let doc_index = parse_cfi_spine_only(cfi)?;
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (_, document_part) = inner.split_once('!')?;
    Some((doc_index, parse_document_position(doc, document_part, ParseBoundary::Glyph)?))
}

/// Resolves a standard range CFI to an exclusive glyph interval.
pub fn parse_cfi_range(doc: &LaidOutDocument, cfi: &str) -> Option<(usize, u32, u32)> {
    let doc_index = parse_cfi_spine_only(cfi)?;
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let (_, document_range) = inner.split_once('!')?;
    let parts = split_range_components(document_range);
    if parts.len() != 3 {
        return None;
    }
    let combine = |relative: &str| format!("{}/{}", parts[0].trim_end_matches('/'), relative.trim_start_matches('/'));
    let start = parse_document_position(doc, &combine(&parts[1]), ParseBoundary::GlyphOrDocumentEnd)?;
    let end = parse_document_position(doc, &combine(&parts[2]), ParseBoundary::GlyphOrDocumentEnd)?;
    (start < end).then_some((doc_index, start, end))
}

fn parse_document_position(doc: &LaidOutDocument, document_part: &str, boundary: ParseBoundary) -> Option<u32> {
    if document_part.is_empty() {
        return Some(0);
    }
    let segments: Vec<&str> = document_part.split('/').filter(|segment| !segment.is_empty()).collect();
    if segments.is_empty() {
        return Some(0);
    }
    let (path_segments, character_offset) = match segments.last()?.split_once(':') {
        Some((step, offset)) => {
            let mut path = segments.clone();
            *path.last_mut()? = step;
            (path, Some(leading_usize(offset)?))
        }
        None => (segments, None),
    };
    let steps: Vec<usize> = path_segments.iter().filter_map(|segment| leading_usize(segment)).collect();
    if steps.len() != path_segments.len() {
        return None;
    }
    let (elements, text_step) = match steps.last().copied() {
        Some(last) if !last.is_multiple_of(2) => (&steps[..steps.len() - 1], Some(last)),
        _ => (&steps[..], None),
    };
    doc.render_view().addressing().resolve_source_position(
        elements,
        text_step,
        character_offset,
        matches!(boundary, ParseBoundary::GlyphOrDocumentEnd),
    )
}

fn split_range_components(value: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut assertion_depth = 0usize;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            parts.last_mut().expect("one component always exists").push(character);
            escaped = false;
            continue;
        }
        if character == '^' {
            parts.last_mut().expect("one component always exists").push(character);
            escaped = true;
            continue;
        }
        match character {
            '[' => assertion_depth += 1,
            ']' => assertion_depth = assertion_depth.saturating_sub(1),
            ',' if assertion_depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut().expect("one component always exists").push(character);
    }
    parts
}

fn leading_usize(value: &str) -> Option<usize> {
    let digits = value.chars().take_while(char::is_ascii_digit).collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphResourceStore, GlyphShaper, LayoutConstraints, ShapeError};
    use html::pipeline::DocumentFactory;
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
        glyph_store: GlyphResourceStore,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphId>, GlyphResourceStore)>,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.glyph_store.clear();
        }

        fn glyph_resources(&mut self) -> &mut GlyphResourceStore {
            &mut self.glyph_store
        }

        fn begin_append_shaping(&mut self) -> Result<(), html::layout::ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.glyph_store.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.glyph_store = glyph_store;
            }
        }

        fn shape_glyph(&mut self, character: char, font_size: f32, _weight: u16, _slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, ShapeError> {
            let key = (character, font_size.to_bits());
            if let Some(glyph) = self.glyphs.get(&key) {
                return Ok(*glyph);
            }
            let metric = GlyphMetric::try_new(character, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(ShapeError::rejected_metric)?;
            let glyph = self.glyph_store.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout_html(source: &str) -> LaidOutDocument {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory.parse_with_new_pipeline(source, None).shape(&mut shaper).expect("fixture shapes").layout(LayoutConstraints::new(600.0, 20.0).expect("valid constraints"))
    }

    fn glyph_for_character(document: &LaidOutDocument, wanted: char) -> u32 {
        let text = document.render_view().text();
        (0..text.glyph_count() as u32).find(|index| text.glyph_at(*index as usize).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == wanted)).expect("fixture character exists")
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
        let document = layout_html("<html><body><p>A😀B</p></body></html>");
        let start = glyph_for_character(&document, '😀');
        let cfi = generate_cfi_range(&document, 0, start, start + 1).expect("range generated");

        assert!(cfi.contains(",/1:1,/1:3"), "emoji endpoints use UTF-16 offsets: {cfi}");
        assert_eq!(parse_cfi_range(&document, &cfi), Some((0, start, start + 1)));
    }

    #[test]
    fn adjacent_text_nodes_share_one_source_chunk() {
        let document = layout_html("<html><body><p>one<!--split-->two</p></body></html>");
        let start = glyph_for_character(&document, 't');
        let cfi = generate_cfi_range(&document, 0, start, start + 2).expect("range generated");

        assert!(cfi.contains(",/1:3,/1:5"), "adjacent text nodes use cumulative offsets: {cfi}");
        assert_eq!(parse_cfi_range(&document, &cfi), Some((0, start, start + 2)));
    }
}
