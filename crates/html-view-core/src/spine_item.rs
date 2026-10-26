//! One item of the spine, with everything addressed by it.
//!
//! A document is not only what is on screen of it. It is also the pipeline
//! that produced it and the resources its content refers to -- and those
//! resources are addressed by indices that mean nothing anywhere else. An
//! image is `image_idx` into *this* document's list. Laying the document out
//! again, for a new column width or a note, needs the session that laid it out
//! the first time.
//!
//! Keeping those together with the display state is what lets the reader have
//! more than one document at once. A page and the spine items beside it are
//! the same kind of thing, cached or on screen, so they are one type.
//!
//! A note is not one of these. It is laid out from a document that already
//! exists and shares that document's resources, so it is a [`DocumentView`] --
//! the display half alone.

use std::ops::Deref;
use std::sync::Arc;

use html::engine::Engine;
use html::pipeline::PipelineInputs;
use html::resources::{ImagePipeline, ImageService};

use crate::document_view::DocumentView;
use crate::{ColumnLayout, LaidOutDocument, load};

pub(crate) struct SpineItem {
    /// Source addresses stay with their document through caching and reflow.
    pub(crate) inputs: PipelineInputs,
    /// Decoded images, addressed by an index into this document's resources.
    /// A pipeline belonging to another document answers the same index with a
    /// different picture.
    pub(crate) images: ImagePipeline,
    /// What is on screen of it.
    pub(crate) view: DocumentView<SessionDocument>,
}

/// Owns the pipeline, whose document is borrowed by the shared view code.
/// Keeping the session here prevents the view from retaining an obsolete
/// document after reflow or glyph rehydration.
pub(crate) struct SessionDocument {
    pub(crate) session: Engine,
}

impl Deref for SessionDocument {
    type Target = LaidOutDocument;

    fn deref(&self) -> &Self::Target {
        self.session.document().expect("spine session must own a laid-out document")
    }
}

impl SpineItem {
    pub(crate) fn new(loaded: load::LoadedRenderDocument, layout: ColumnLayout, image_service: ImageService) -> Self {
        let images = Self::image_pipeline_for(loaded.document(), image_service);
        Self {
            inputs: loaded.inputs,
            images,
            view: DocumentView::new(SessionDocument { session: loaded.session }, layout),
        }
    }

    pub(crate) fn image_pipeline_for(document: &LaidOutDocument, image_service: ImageService) -> ImagePipeline {
        ImagePipeline::with_service(Arc::new(document.render_view().images().to_vec()), image_service)
    }

    /// Records the inputs for the document now owned by the updated session.
    pub(crate) fn install_pipeline_result(&mut self, inputs: PipelineInputs) -> f32 {
        self.inputs = inputs;
        self.root_font_size()
    }

    pub(crate) fn root_font_size(&self) -> f32 {
        self.inputs.style_environment.root_font_size as f32
    }
}
