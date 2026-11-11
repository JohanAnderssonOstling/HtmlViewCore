//! Reader loading policy and the settings snapshot used by chapter preparation.

use html::layout::{ImageSizingPolicy, NoteFlow, TextCompositionPolicy};
use html::resources::ResourceProvider;
use html::style::ReaderStyleOverrides;
use html::{Document, DocumentOptions, Error, PreparedDocument};
use html_view_types::NoteDisplay;
use std::sync::Arc;

#[derive(Clone, PartialEq)]
pub(crate) struct DocumentLoadSettings {
    pub(crate) root_font_size: f32,
    pub(crate) column_width: f64,
    pub(crate) reader_overrides: ReaderStyleOverrides,
    pub(crate) image_sizing_policy: ImageSizingPolicy,
    pub(crate) text_composition_policy: TextCompositionPolicy,
    pub(crate) note_display: NoteDisplay,
}

impl DocumentLoadSettings {
    pub(crate) fn into_options(self) -> Result<DocumentOptions, Error> {
        let mut options =
            DocumentOptions::new(self.column_width)?.with_root_font_size(self.root_font_size)?;
        options.reader_overrides = self.reader_overrides;
        options.image_sizing_policy = self.image_sizing_policy;
        options.text_composition_policy = self.text_composition_policy;
        options.note_flow = note_flow_for(self.note_display);
        Ok(options)
    }
}

/// How a presentation choice reaches the pipeline. Popup mode holds notes back
/// from the flow so the reader can show them itself; as-authored leaves them
/// where they were written.
pub(crate) fn note_flow_for(note_display: NoteDisplay) -> NoteFlow {
    match note_display {
        NoteDisplay::Popup => NoteFlow::Excluded,
        NoteDisplay::AsAuthored => NoteFlow::InFlow,
    }
}

pub(crate) fn prepare_document_with_settings(
    provider: Arc<dyn ResourceProvider>,
    uri: &str,
    settings: DocumentLoadSettings,
) -> Result<PreparedDocument, html::Error> {
    let options = settings.into_options()?;
    Document::prepare(provider, uri, options)
}

#[cfg(test)]
mod tests;
