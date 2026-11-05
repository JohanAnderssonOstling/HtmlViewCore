# Skrift Column Paginator

Framework-neutral reader viewport, navigation, interaction, and painting
contracts built on `html-engine`.

Skrift owns document interpretation: typed content roles, source addressing,
Unicode word selection and search, quoted-text location, intrinsic caret and
selection geometry, logical selection ranges, document image resources, CSS
content painting, and Markdown, table HTML, and formula export. Reader code
uses these public APIs without inspecting HTML tags or EPUB/ARIA attributes.
Source addresses resolve directly through the current document’s DOM.
Text, selection-export, and page composition use the engine views directly;
reader adapters are reserved for publication identity and presentation policy.

Document text export, reading-line regions, selection interaction/painting,
parse-only search, source statistics and page composition use the engine facade.
Selection plans, structural event assembly and image working sets remain private.
The reader chooses pagination preferences and viewport geometry. Notes use
intrinsic engine geometry without constructing a page or a column frame.

Loading helpers return `Document` directly. Timing reports use the engine's
`LoadTimings` resource/pipeline phases. The engine provides opaque revision
snapshots; reader layout, resource, display and overlay counters remain local. Each
spine item owns its engine and page state directly. The engine owns measured
geometry; the text backend owns native fonts, glyphs and shaping caches. Cached
pages retain only placement geometry.

HtmlViewCore owns publication and interaction policy: EPUB CFI syntax and spine
identity, book-wide search scope, annotation ownership, pagination preferences,
view origin/display scale, platform event routing, selection appearance, and
clipboard effects. The shell owns platform clipboard access. The engine's
optional `math-export` feature supplies MathML-to-LaTeX conversion when the reader
copies an equation.

Each displayed spine item owns one engine `SelectionController`. The reader
supplies pointer data, selection intent and drag distance through `PageView`;
the engine owns text/table gestures, touch anchors and highlight painting.
Reader adapters forward changed/finished/click outcomes into repaint requests,
CFI notifications and platform actions. `WORD_SELECTION_DRAG_DISTANCE` remains
four logical pixels and is exported for application gestures that must agree
with reader selection. Scoped notes use the same controller and reader policy;
standalone GPUI fragments choose their own adapter policy. The forwarding
`html-view-select` crate was removed; the reader calls the engine page view directly.

Document configuration uses the engine’s `DocumentUpdate` API. Overlay ranges
use the engine's styled-range painter with reader-owned colors and priority.
Highlight, underline, squiggle and strikethrough drawing live in the engine;
`AnnotationStyle` reexports its shared `OverlayStyle`. Reading-unit discovery
and page-based image loading also come from the engine; the reader
chooses playback position, following policy, prefetch timing and lookahead. The `html-fonts`
adapter uses the engine's optional `font-data` service for decoding and authored
source fallback, and owns publication font caching and native preparation.

Engine `PagePositions` retains the page's geometry, paint origin, line
placements, offset breaks, repeated headers and paint dependencies. Reader frame
state stores that result intact. `PageView` supplies content painting, hit tests,
selection interaction/geometry/painting, overlay painting and reading-line bands
from the same mapping. The reader removes view origin and display scale before
passing a pointer, and scales returned bounds for its UI. Base content and
interaction overlays remain independently cached paint layers. Width-only
reprojection updates the engine page through `PagePositions::reproject_columns`;
the reader only decides whether the cache remains valid. The old `html-view-doc`
crate and its `PageProjection` implementation were removed.

Pagination uses the engine's `Paginator::compose` with `PageRequest::StartingAt`,
`StartingAtOffset` or `EndingAt`. The request's anchor determines traversal.
`ColumnLayout` retains viewport/width/scale preferences and supplies neutral
`PageGeometry`; the engine handles fitting, CSS breaks, widows/orphans, table
fragmentation/repeated headers, book keeps and optional vertical rhythm. Shared
page-result types are reexported from `html::pagination`, and `PaginationScratch`
retains transient buffers. Backward offset fallback and first-page normalization
are engine-owned. Viewport state stores one `PageRequest` rather than separate
direction/cursor/backward bookkeeping, while retaining pending offsets separately
from the installed page. Installed-page cursors come directly from `PagePositions`.
The former `html-view-paginate` crate was removed.
Page turns use the engine's `next_page_anchor` and `previous_page_anchor`;
the reader applies their cursor/offset results and handles chapter boundaries,
pending navigation and notifications. Line stepping, offset restoration,
document-end anchors and fractional jumps use `DocumentTarget` operations.
`request_images` receives the composed page and lookahead count; the engine
selects its pagination groups and image lines. Page text/matching and narration
visibility/reading order use `PageView`. The reader applies display scaling in
place to the engine's reading-band vector.
Narration delegates page traversal and word-safe fragment extraction to
`Paginator::append_reading_fragments`. The reader supplies the source interval
and reading origin, retains the active fragment index, and owns playback and
publication navigation.

Position reporting uses one `ReadingPositionChanged` snapshot after composition
settles. The engine's `document_position` query supplies the cursor, glyph,
document fraction and a source address resolved on demand. The reader selects
the settled/pending origin and preferred glyph, attaches publication identity,
and calculates publication progress. Relayout uses the engine's `reflow_anchor`
to preserve leading content or capture a source address and glyph fallback.
The snapshot carries the resulting source
address and reading progress; hosts derive CFI,
table-of-contents selection and persistence from that snapshot. Deliberate jumps
emit `NavigationJump` before their settled position. The host owns back/forward
history and restores its saved source positions through CFI navigation. The
renderer retains no separate anchor-based history or TOC filter. An explicit
state snapshot replays the last settled position without calculating a position
from pending geometry.

Renderer operations produce `RendererEffects`: coalesced repaint/restyle flags,
events, owned clipboard payloads and publication font-load requests. Construct a
session with `RendererServices` for job workers and resource wakeups, then call
`drain_effects(&mut effects)` after construction and each operation, including
frame preparation. Process and clear the caller-owned buffer for reuse. Supply
popup note width through `set_note_popup_width`; no shared host callback object
is needed. Platform clipboard failures are handled by the shell; document export
failures remain renderer events.

The root API explicitly exports session, configuration, command, event, search,
address, and paint-cache contracts. Navigation, viewport and overlay state
remain in their component crates; page composition lives in the engine. Use `session.apply(RendererCommand::...)` for
font size, preferred column width, scale, document jumps, and search activation
or stepping. Relative adjustments derive the new value from
`session.render_view().root_font_size()`, `session.column_width()`, or
`session.scale()`. Document queries use `render_view()`; operations that return
meaningful results keep their direct APIs. `SetSearchQuery` transfers the query
string into search state without an extra clone.
Use `set_cfi_position` to restore a saved CFI without adding navigation history.
Page and line stepping use `RendererCommand::NavigatePage(step)` and
`NavigateLine(step)`: positive moves forward once, negative moves backward once,
and zero leaves navigation unchanged. Hosts translate wheel direction into a
line-step command. Narration follows targets without recording manual navigation.

Table copying uses `copy_table_at(position, format)`, with the engine's
`html::export::TableExportFormat::{Markdown, Html, AuthoredHtml}`. Copying the
selected cell range remains a separate operation. Reading queries share the
visible source position; full-document fragments append the prefix and the
onward page fragments into one buffer without generating a discarded suffix.

Document interaction methods accept painted positions relative to the document's
top-left; returned selection and reading bounds use the same coordinates. The
session handles scale conversion. Hosts subtract their window origin; note
positions stay relative to the note, and `render_view()` retains intrinsic engine
coordinates. There are no public origin or layout-conversion setters/getters.

`content_at(position)` returns one `ContentHit` containing table, image, and
equation action flags. Image copying includes images under text, while equation
LaTeX actions require the equation to win paint order. `pointer_down` returns a
`PointerDownOutcome` with `handled` and the image-hit flag so hosts can retain tap
policy without repeating the image query. Selection and table copying return
whether a payload was queued; image and formula export failures remain events.
`DocumentGlyph` and `DocumentImage` are internal reader-root imports.

Consumers should depend on the `skrift-column-paginator` package:

```toml
[dependencies]
skrift-column-paginator = { git = "https://github.com/JohanAnderssonOstling/HtmlViewCore" }
```

The ignored corpus-profiling tests accept a local HTML corpus through
`HTML_VIEW_PROFILE_CORPUS`; application test data is deliberately not part of
this repository.

The optional [`adapters` workspace](adapters/README.md) owns `html-view-gpui`.
It shares the engine’s generic GPUI backend; core reader crates have no GPUI
dependency. Loading helpers call the engine directly, with separate ordinary,
timed and worker-preparation operations.
