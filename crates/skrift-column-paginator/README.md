# skrift-column-paginator

The public reader crate, previously named `html-view-core`. Import it in Rust as
`skrift_column_paginator`.

This crate owns framework-independent reader state, selection, search
highlighting, pagination, painting, host interface, and command/event boundary.
The core owns document loading, cross-document navigation, selection, search, and
reader-specific paint orchestration. Generic fragment layout, retained render
scenes, CSS content painting, and the projection contract belong to `html-render-core`.
Document geometry, logical selection ranges, source/quote anchoring, and image
resource updates are exposed by `html-engine`. EPUB and loose-file
source discovery belongs to the consuming application. Framework adapters,
concrete glyph-layout resolution, and window-system painters live in separate
integration crates such as `html-view-gpui`.

Scoped content sessions, typed content hit testing, annotation anchor indexes,
document search sessions and portable export payloads are owned by the engine.
The reader selects a publication document, supplies line/cell projection, decodes
CFIs, styles resolved ranges and routes payloads to its host. Open notes retain an
engine `ScopedContent` plus the owning publication document index.
