//! Publication ownership of an independently laid-out engine scope.
use html::scoped::ScopedContent;

pub(crate) struct OpenNote {
    pub(crate) document_index: usize,
    pub(crate) view: ScopedContent,
}
