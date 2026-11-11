//! Maps renderer document indices to the original EPUB package addresses.

use html_view_types::SourcePosition;
use crate::syntax::{Location, ParsedPath, package_steps, parse_fragment, synthetic_index};
use std::collections::HashMap;

/// `None` uses the synthetic document addresses of non-EPUB publications.
/// Missing entries in an EPUB map are deliberately unaddressable: inventing a
/// package address could silently point into a different spine item.
#[derive(Clone, Debug, Default)]
pub struct PublicationCfi {
    bases: Option<Vec<Option<String>>>,
    documents: HashMap<Vec<usize>, usize>,
}

impl PublicationCfi {
    pub fn new(bases: Option<Vec<Option<String>>>) -> Self {
        let mut documents = HashMap::new();
        if let Some(bases) = &bases {
            for (index, base) in bases.iter().enumerate() {
                if let Some(steps) = base.as_deref().and_then(package_steps) {
                    documents.entry(steps).or_insert(index);
                }
            }
        }
        Self { bases, documents }
    }

    pub fn is_epub(&self) -> bool {
        self.bases.is_some()
    }

    pub fn to_publication(&self, internal: &str) -> Option<String> {
        let fragment = parse_fragment(internal)?;
        let document = synthetic_index(&fragment.package)?;
        let Some(bases) = &self.bases else {
            return Some(internal.to_owned());
        };
        let base = bases.get(document)?.as_deref()?;
        package_steps(base)?;
        Some(join(base, fragment.content))
    }

    pub fn to_internal(&self, publication: &str) -> Option<String> {
        let fragment = parse_fragment(publication)?;
        let document = self.index_for_package(&fragment.package)?;
        if self.bases.is_none() {
            return Some(publication.to_owned());
        }
        Some(join(
            &format!("/6/{}", (document + 1) * 2),
            fragment.content,
        ))
    }

    pub fn document_index(&self, cfi: &str) -> Option<usize> {
        self.index_for_package(&parse_fragment(cfi)?.package)
    }

    fn index_for_package(&self, package: &[usize]) -> Option<usize> {
        if self.bases.is_none() {
            synthetic_index(package)
        } else {
            self.documents.get(package).copied()
        }
    }

    pub fn source_position_cfi(&self, position: &SourcePosition) -> Option<String> {
        self.to_publication(&super::source_position_cfi(position))
    }

    pub fn parse_source_position(&self, cfi: &str) -> Option<SourcePosition> {
        let fragment = parse_fragment(cfi)?;
        let document = self.index_for_package(&fragment.package)?;
        let path = match fragment.location {
            Some(Location::Point(path) | Location::Range(path, _)) => path,
            None => ParsedPath::default(),
        };
        Some(SourcePosition {
            document,
            path: path.steps,
            utf16_offset: path.offset,
        })
    }
}

fn join(package: &str, content: Option<&str>) -> String {
    match content.filter(|content| !content.is_empty()) {
        Some(content) => format!("epubcfi({package}!{content})"),
        None => format!("epubcfi({package})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_spine_items_and_nonstandard_package_paths_round_trip() {
        let map = PublicationCfi::new(Some(vec![
            Some("/8[sp!ine]/2[first]".into()),
            Some("/8[sp!ine]/8[last^]id]".into()),
        ]));
        let old = "epubcfi(/6/4!/4[p]/2,/1:2,/1:5)";
        let full = "epubcfi(/8[sp!ine]/8[last^]id]!/4[p]/2,/1:2,/1:5)";
        assert_eq!(map.to_publication(old).as_deref(), Some(full));
        assert_eq!(map.to_internal(full).as_deref(), Some(old));
        assert_eq!(map.document_index("epubcfi(/8/8!/4/2/1:2)"), Some(1));
        assert_eq!(map.document_index("epubcfi(/8/4!/4/2/1:2)"), None);
        assert_eq!(map.document_index("epubcfi(/8/8)"), Some(1));
    }

    #[test]
    fn unknown_epub_documents_do_not_acquire_fabricated_addresses() {
        let map = PublicationCfi::new(Some(vec![None]));
        assert_eq!(map.to_publication("epubcfi(/6/2!/4/1:0)"), None);
        assert_eq!(map.to_internal("epubcfi(/6/2!/4/1:0)"), None);
        assert_eq!(
            PublicationCfi::default()
                .to_publication("epubcfi(/6/2!/4/1:0)")
                .as_deref(),
            Some("epubcfi(/6/2!/4/1:0)")
        );
    }
}
