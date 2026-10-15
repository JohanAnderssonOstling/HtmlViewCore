//! Source addresses for content without glyphs. Built alongside document loading
//! and retained across reflow; layout box ids are only used to recover geometry.

use std::collections::HashMap;

use html::document::{MarkupSyntax, parse_document};

#[derive(Default)]
pub struct SourceIndex {
    elements: HashMap<u32, Vec<usize>>,
    images: HashMap<u32, u32>,
}

impl SourceIndex {
    pub fn new(source: &str, syntax: MarkupSyntax) -> Self {
        let Ok(parsed) = parse_document(source, syntax) else { return Self::default() };
        let Ok(document) = parsed.build_dom_checked() else { return Self::default() };
        let mut index = Self::default();
        for node in document.node_ids() {
            let Some(element) = document.element_ref(node) else { continue };
            let Some(path) = document.get_dom_path_to_node(node).into_iter().skip(1).map(|node| document.get_element_step(node)).collect::<Option<Vec<_>>>() else { continue };
            index.elements.insert(node.raw(), path);
            if let Some(image) = element.image_idx() {
                index.images.insert(image, node.raw());
            }
        }
        index
    }

    pub fn image_path(&self, image: u32) -> Option<&[usize]> {
        self.element_path(*self.images.get(&image)?)
    }

    pub fn element_path(&self, node: u32) -> Option<&[usize]> {
        self.elements.get(&node).map(Vec::as_slice)
    }

    pub fn element_node(&self, path: &[usize]) -> Option<u32> {
        self.elements.iter().find_map(|(node, candidate)| (candidate == path).then_some(*node))
    }
}
