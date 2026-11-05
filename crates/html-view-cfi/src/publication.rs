//! Maps renderer document indices to the original EPUB package addresses.

use html_view_types::SourcePosition;
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
        let Some(bases) = &self.bases else {
            return Some(internal.to_owned());
        };
        let (package, content) = split(internal)?;
        let document = synthetic_index(package)?;
        let base = bases.get(document)?.as_deref()?;
        Some(join(base, content))
    }

    pub fn to_internal(&self, publication: &str) -> Option<String> {
        if self.bases.is_none() {
            return Some(publication.to_owned());
        }
        let (package, content) = split(publication)?;
        let steps = package_steps(package)?;
        let document = *self.documents.get(&steps)?;
        Some(join(&format!("/6/{}", (document + 1) * 2), content))
    }

    pub fn document_index(&self, cfi: &str) -> Option<usize> {
        let internal = self.to_internal(cfi)?;
        synthetic_index(split(&internal)?.0)
    }

    pub fn source_position_cfi(&self, position: &SourcePosition) -> Option<String> {
        self.to_publication(&super::source_position_cfi(position))
    }

    pub fn parse_source_position(&self, cfi: &str) -> Option<SourcePosition> {
        let internal = self.to_internal(cfi)?;
        if split(&internal)?.1.is_none() {
            return Some(SourcePosition {
                document: self.document_index(cfi)?,
                path: Vec::new(),
                utf16_offset: None,
            });
        }
        super::parse_source_position(&internal)
    }
}

fn join(package: &str, content: Option<&str>) -> String {
    match content.filter(|content| !content.is_empty()) {
        Some(content) => format!("epubcfi({package}!{content})"),
        None => format!("epubcfi({package})"),
    }
}

fn synthetic_index(package: &str) -> Option<usize> {
    let steps = package_steps(package)?;
    let [6, step] = steps.as_slice() else {
        return None;
    };
    (*step >= 2 && step.is_multiple_of(2)).then(|| step / 2 - 1)
}

fn split(cfi: &str) -> Option<(&str, Option<&str>)> {
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let mut assertion = false;
    let mut escaped = false;
    for (index, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '^' => escaped = true,
            '[' if !assertion => assertion = true,
            ']' if assertion => assertion = false,
            '!' if !assertion => return Some((&inner[..index], Some(&inner[index + 1..]))),
            _ => {}
        }
    }
    (!assertion && !escaped).then_some((inner, None))
}

/// Ignore optional assertions when matching structurally equivalent addresses.
fn package_steps(package: &str) -> Option<Vec<usize>> {
    let mut plain = String::new();
    let mut assertion = false;
    let mut escaped = false;
    for ch in package.chars() {
        if escaped {
            if !assertion {
                return None;
            }
            escaped = false;
            continue;
        }
        match ch {
            '^' if assertion => escaped = true,
            '[' if !assertion => assertion = true,
            ']' if assertion => assertion = false,
            _ if assertion => {}
            '/' | '0'..='9' => plain.push(ch),
            _ => return None,
        }
    }
    if assertion || escaped {
        return None;
    }
    let steps = plain
        .strip_prefix('/')?
        .split('/')
        .map(|step| {
            if step.starts_with('0') {
                return None;
            }
            let value = step.parse::<usize>().ok()?;
            (value > 0 && value.is_multiple_of(2)).then_some(value)
        })
        .collect::<Option<Vec<_>>>()?;
    (!steps.is_empty()).then_some(steps)
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
