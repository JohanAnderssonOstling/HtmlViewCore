# Skrift Column Paginator

Framework-neutral reader pagination, navigation, interaction, and painting
contracts built on `html-engine`.

Skrift owns document interpretation: typed content roles, source addressing,
Unicode word selection and search, quoted-text location, intrinsic caret and
selection geometry, logical selection ranges, document image resources, CSS
content painting, and Markdown, table HTML, and formula export. Reader code
uses these public APIs without inspecting HTML tags or EPUB/ARIA attributes.
Source addresses resolve directly through the current document’s DOM.
Text, selection-export, and structural queries use the engine views directly;
reader adapters are reserved for publication identity and presentation policy.

Document text export, reading-line regions, word hit testing, structural selection
plans, table-cell enumeration, parse-only search, source statistics, image dependency
queries, and structural boundary/table queries also use the engine facade. The
paginator consumes document facts while retaining fitting, repetition, and book
composition preferences. Notes use intrinsic engine geometry without constructing
a page or a column frame.

Loading helpers return `Engine` directly (`LoadedRenderDocument` and
`PreparedRenderDocument` are compatibility aliases). Timing reports use the engine's
measured `LoadTimings` resource/pipeline phases. Document revision invalidation is
shared with the engine; reader display and overlay counters remain local. Each
spine item owns its engine and page state directly. Shaping and glyph resources
remain engine-owned; cached pages retain only placement geometry.

HtmlViewCore owns publication and interaction policy: EPUB CFI syntax and spine
identity, book-wide search scope, annotation ownership, pagination preferences,
screen/column projection, input gestures, selection appearance, and clipboard access. The engine's
optional `math-export` feature supplies MathML-to-LaTeX conversion when the reader
copies an equation.

Document configuration uses the engine’s `DocumentUpdate` API. Overlay ranges
use batched engine geometry with reader-owned colors and priority. Reading-unit
cursors and line/region image discovery also come from the engine; the reader
chooses playback position, visibility and prefetch extent. Font decoding and
authored fallback use the optional engine `font-data` feature through `html-fonts`.

The reader's page projection implements the engine's `PaintProjection` contract.
It selects visible lines and maps source rectangles into columns, including
repeated table-header regions. Selection and table hit testing reuse that rectangle
projection, and each overlay pass prepares its selection geometry once. Base
content and interaction overlays form two independently cached paint layers.
The engine applies the same CSS drawing stream
used by continuous fragments. Document geometry likewise accepts caller-placed
lines, leaving column selection and screen-coordinate conversion in the reader.

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
