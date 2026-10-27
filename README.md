# HTML View Core

Framework-neutral reader pagination, navigation, interaction, and painting
contracts built on `html-engine`.

Skrift owns document interpretation: typed content roles, source addressing,
Unicode word selection and search, quoted-text location, intrinsic caret and
selection geometry, and Markdown, table HTML, and formula export. Reader code
uses these public APIs without inspecting HTML tags or EPUB/ARIA attributes.
Source addresses resolve directly through the current document’s DOM.
Text, selection-export, and structural queries use the engine views directly;
reader adapters are reserved for publication identity and presentation policy.

HtmlViewCore owns publication and interaction policy: EPUB CFI syntax and spine
identity, book-wide search scope, annotation ownership, pagination preferences,
screen/column projection, selection state, and clipboard access. The engine's
optional `math-export` feature supplies MathML-to-LaTeX conversion when the reader
copies an equation.

Consumers should depend on the `html-view-core` package:

```toml
[dependencies]
html-view-core = { git = "https://github.com/JohanAnderssonOstling/HtmlViewCore" }
```

The ignored corpus-profiling tests accept a local HTML corpus through
`HTML_VIEW_PROFILE_CORPUS`; application test data is deliberately not part of
this repository.
