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

use std::sync::Arc;

use html::engine::Engine;
use html::pipeline::PipelineInputs;
use html::resources::{ImagePipeline, ImageService};

use crate::document_view::DocumentView;
use crate::{ColumnLayout, LaidOutDocument, load};

pub(crate) struct SpineItem {
    /// The pipeline that produced the document, kept so it can be asked for
    /// the document again under different inputs.
    pub(crate) session: Engine,
    pub(crate) inputs: PipelineInputs,
    /// Decoded images, addressed by an index into this document's resources.
    /// A pipeline belonging to another document answers the same index with a
    /// different picture.
    pub(crate) images: ImagePipeline,
    /// What is on screen of it.
    pub(crate) view: DocumentView,
}

impl SpineItem {
    pub(crate) fn new(
        loaded: load::LoadedRenderDocument,
        layout: ColumnLayout,
        image_service: ImageService,
    ) -> Self {
        let images = Self::image_pipeline_for(&loaded.document, image_service);
        Self {
            session: loaded.session,
            inputs: loaded.inputs,
            images,
            view: DocumentView::new(loaded.document, layout),
        }
    }

    pub(crate) fn image_pipeline_for(
        document: &LaidOutDocument,
        image_service: ImageService,
    ) -> ImagePipeline {
        ImagePipeline::with_service(
            Arc::new(document.render_view().images().to_vec()),
            image_service,
        )
    }

    /// Replaces the document while keeping the item's identity, for a relayout
    /// that produced a new one from the same source.
    pub(crate) fn replace_document(&mut self, document: LaidOutDocument) {
        self.view.document = document;
    }

    /// Installs the inseparable products of one successful pipeline update.
    pub(crate) fn install_pipeline_result(
        &mut self,
        document: LaidOutDocument,
        inputs: PipelineInputs,
    ) -> f32 {
        self.view.document = document;
        self.inputs = inputs;
        self.root_font_size()
    }

    pub(crate) fn root_font_size(&self) -> f32 {
        self.inputs.style_environment.root_font_size as f32
    }
}
