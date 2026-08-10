# `scraper` usage

> **Resolved.** `scraper` is gone from this workspace. Note bodies are now held
> out of the reading flow by the engine (`NoteFlow`) rather than by injected
> CSS, and previews come from `Engine::layout_note` rather than a second parse.
> What follows is the survey that led there, kept for the reasoning.
>
> `html-parse` still depends on `scraper` internally, so it remains in
> `Cargo.lock` transitively. What was removed is the second, divergent parse --
> not the crate from the build graph.

Survey of everywhere the workspace depended on `scraper`, and what each use was
for. The goal was to depend on `html-engine` alone and stop doing ad hoc HTML
parsing.

## Summary

`scraper` is declared once and used by one crate for one feature.

| declaration | |
| --- | --- |
| `Cargo.toml:20` | `scraper = "0.27"` (workspace dependency) |
| `crates/html-view-core/Cargo.toml:21` | `scraper.workspace = true` |

No other crate depends on it. `html-view-doc`, `-paginate`, `-paint`,
`-select`, `-navigate`, `-types`, `-cfi` and `-load` are already clean.

## Call sites

Five, all serving **footnote preview popups**.

### 1. Parsing the target document — `document_cache.rs:22,27`

```rust
pub(crate) struct FootnoteDocumentIndex {
    pub(crate) document: scraper::Html,
}

impl FootnoteDocumentIndex {
    pub(crate) fn parse(source: &str) -> Self {
        Self { document: scraper::Html::parse_document(source) }
    }
}
```

A second parsed representation of a spine document, cached separately from the
engine's own pipeline output and evicted on its own LRU.

### 2. Locating the note element — `renderer/nav.rs:263`

```rust
let selector = scraper::Selector::parse("[id]").expect("static id selector must parse");
let element = indexed.document.select(&selector).find(|element| element.value().attr("id") == Some(fragment))?;
```

Finds the element carrying the fragment id, then reads `epub:type` and `role`
off it to decide whether the target is really a note.

### 3. Extracting preview text — `renderer/nav.rs:270`

```rust
let block_selector = scraper::Selector::parse("h1, h2, h3, h4, h5, h6, p, li, blockquote")
    .expect("static footnote block selector must parse");
```

Walks block elements inside the note and collects their text, prefixing `li`
with a bullet and wrapping `blockquote` in quotes.

### 4. Test selector — `document_cache.rs:163`

`scraper::Selector::parse("#doc0")` in a unit test, asserting the cache stores
the parsed source rather than an empty document. Follows whatever the
production code does.

## Why it is there

Footnote targets can live in a **different spine document from the one on
screen**. The engine's `render_view()` describes only the document currently
laid out, so there is no engine-side way to ask "does id X exist in chapter 7,
is it a note, and what does it say" without loading chapter 7.

The code is explicit that this is a fallback. `renderer/nav.rs:256`:

```rust
let authoritative_target_semantics = (doc == self.nav.current_doc_index)
    .then(|| self.document.render_view().addressing().is_note_target(fragment))
    .unwrap_or(false);
```

When the target **is** the current document, the engine already answers the
question through `addressing().is_note_target()` — and the variable name says
that answer is the authoritative one. `scraper` exists only to approximate it
for documents that are not laid out.

So this is not a capability the engine lacks in principle. It is a lifecycle
gap: the answer is available for one document and not the others.

## Options for removing it

Roughly in order of cost.

1. **Load the target document through the existing pipeline.**
   `html-view-load` already loads any spine document, and `DocumentCache`
   already caches laid-out documents. A footnote preview could load the target
   the same way navigation does and query `addressing()` on it. Removes
   `scraper`, removes `FootnoteDocumentIndex` and its LRU, and makes the
   preview path agree with the authoritative path by construction. Cost: laying
   out a whole document to preview one note is heavier than parsing it, though
   the result is already cached for the navigation that usually follows.

2. **Ask the engine for a parse-only DOM query API.** `html::pipeline` already
   exposes `parse_html_document`/`ParsedHtml` and `plain_text_from_fragment`.
   If `ParsedHtml` gained id lookup, attribute reads and block text extraction,
   the preview could use it without shaping or layout. Cheapest at runtime and
   the correct long-term shape, but needs a change in `html-engine`.

3. **Keep a second parser but make it the engine's.** Least attractive: it
   preserves the duplication that makes preview semantics diverge from
   `is_note_target`.

## Risk of leaving it

The two paths can disagree. `is_note_target` is the engine's judgement,
implemented against the laid-out document; the `scraper` path re-derives it from
`epub:type` and `role` tokens by hand at `renderer/nav.rs:265`. A note the
engine recognises and the hand-rolled check does not — or the reverse — shows a
preview in one chapter and not in another for the same markup.
