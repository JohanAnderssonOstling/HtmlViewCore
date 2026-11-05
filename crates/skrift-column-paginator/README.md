# skrift-column-paginator

The public reader crate, previously named `html-view-core`. Import it in Rust as
`skrift_column_paginator`.

This crate owns framework-independent reader state, selection, search
highlighting, pagination, painting, platform services, and command/effect boundary.
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
engine `ScopedContent` and, for a cross-document popup, its owning document.

Navigation, pagination and projection consume the engine's immutable `RenderView`
directly. Logical selection uses the engine's `SelectionState`; link hover belongs
to the reader's page state. Pagination transfers the frame's existing line-position
buffer into the next composition. `ColumnLayout::configure_viewport` composes columns
without negotiating document reflow; explicit width commands own that update.

Construct `RendererSession` with `RendererServices` for worker spawners and the
resource waker. After construction and each operation, drain `RendererEffects`
into a reusable caller-owned buffer. The shell processes repaint/restyle flags,
events, clipboard payloads and font-load requests, clearing the buffer as it
goes. Copy operations queue owned payloads; the shell handles platform clipboard
errors. `set_note_popup_width` supplies the width for subsequent popup layouts.

Synchronous document-loading helpers and `RendererSession::from_provider_with_nav`
return `Result`. Navigation reports preparation or shaping failures through
`OperationFailed` and retains the active page. Reader note and viewport settings
commit after their document updates succeed.

`SearchResults` carries `BookSearchResults`, a collection of shared immutable
batches with `get`, `iter`, and length queries. `SearchResultsAppended` carries
only the new batch. Event consumers replace their collection for a full snapshot
and append shared batches for incremental updates, without cloning match strings.
Moving the result cursor and appending a batch for the same query reuse local
matches. Query/options changes, chapter replacement and text/style reshaping
refresh those matches; ordinary reflow preserves them.

Open popup notes participate in image demand and resource polling. Image-only
updates refresh their scoped geometry and emit `FootnoteOpened` with the existing
placement so the shell can replace its recorded popup scene. While a note from
another document owns the native backend, page image relayout is deferred until
it closes. Overlay painting retains caller-owned scratch buffers across frames.
