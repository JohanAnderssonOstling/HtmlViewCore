//! Publication ownership of an independently laid-out engine scope.
use crate::FootnotePreview;
use html::scoped::ScopedContent;
use html::Document;

pub(crate) struct OpenNote {
    pub(crate) view: ScopedContent,
    pub(crate) preview: FootnotePreview,
    pub(crate) owner: Option<Document>,
}
