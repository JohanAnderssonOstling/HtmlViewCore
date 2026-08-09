//! Read-only queries over a laid-out document.
//!
//! Every method reachable from [`DocQuery`] depends on the document alone: no
//! viewport, no frame, no navigation or selection state. Keeping them behind
//! this borrow makes that independence checkable by the compiler instead of by
//! convention, and lets the rest of the renderer pass document access around as
//! an ordinary parameter.
//!
//! The methods themselves live next to the code they serve — `selection.rs`,
//! `table_selection.rs`, `render.rs`, `nav.rs`, `semantic_keeps.rs` and
//! `highlight.rs` each contribute an `impl DocQuery<'_>` block.

use html::layout::LaidOutDocument;

use crate::RendererCore;

#[derive(Clone, Copy)]
pub(crate) struct DocQuery<'a> {
    document: &'a LaidOutDocument,
}

impl<'a> DocQuery<'a> {
    pub(crate) fn new(document: &'a LaidOutDocument) -> Self {
        Self { document }
    }

    pub(crate) fn view(self) -> html::layout::RenderView<'a> {
        self.document.render_view()
    }

    pub(crate) fn text(self) -> html::layout::RenderTextView<'a> {
        self.document.render_view().text()
    }

    pub(crate) fn boxes(self) -> html::layout::RenderBoxView<'a> {
        self.document.render_view().boxes()
    }
}

impl RendererCore {
    pub(crate) fn doc(&self) -> DocQuery<'_> {
        DocQuery::new(&self.document)
    }
}
