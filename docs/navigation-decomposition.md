# Navigation decomposition

Follow-on to the `RendererCore` decomposition (commits `45f970b`..`4011bc1`),
which took the struct from 30 fields to 24 by extracting `DocQuery`,
`DocumentCache`, `PreparedPageCache`, and by moving behavior onto
`TableSelectionState`, `HighlightState`, `AnnotationOverlayState` and
`ColumnLayout`.

## Why navigation is separable

Measured over `renderer/nav.rs`:

- 42 methods touch `self.nav`. Only **5** have a direct field footprint of 5 or
  more; **33** touch three fields or fewer.
- Only **12** can transitively reach a document swap
  (`replace_document_state` / `cache_current_document` / `load_document_at`).
  The other 30 cannot.

The apparent density is an artifact of everything funnelling through three wide
primitives:

| primitive | direct fields |
| --- | --- |
| `replace_document_state` | 10 |
| `cache_current_document` | 6 |
| `reset_view_state` | 5 |

So navigation is already layered: a thin cross-document orchestration layer over
a large within-document layer. The within-document layer needs only the
document, the viewport, the current frame and the host.

## Defects this fixes

1. **`emit_state_snapshot` bypasses the history dedup.** It emits
   `HistoryAvailability` inline with its own copy of the predicate instead of
   calling `update_history_availability_signals`, so `last_history_availability`
   is never written and the next history change re-emits an identical event.
   The predicates are logically equivalent, so no wrong value is emitted.
2. **Five copies of the dedup pattern.** Each `last_*` field is written by
   exactly one method, all in the shape *compare, assign, emit*.
3. **Progress arithmetic implemented twice, in opposite directions.**
   `update_nav_signal` maps glyph to location number; `set_progress_fraction`
   maps fraction to document and glyph. Both are pure over
   `document_text_lengths`.

## Phase 1 — split `NavigationState` (14 fields to 8)

| step | change | status |
| --- | --- | --- |
| 1a | `Debounced<T>` owning compare/assign/emit; bundle the four `update_nav_signal` markers into `NavSignals` | not started |
| 1b | `LocationHistory` owning the stack, cursor and availability marker; route `emit_state_snapshot` through it | not started |
| 1c | `TocAnchors` owning `toc_anchor_strings_by_doc` + `toc_anchor_filter`, rebuilt from `DocQuery` | not started |

Left bare on `NavigationState`: `current_doc_index`, `document_uris`,
`document_text_lengths`, `nav_anchor_glyph`, `pending_nav_anchor_update`. These
are read 50+ times across the renderer and are genuinely shared; wrapping them
buys nothing.

## Phase 2 — extract `Navigator`

| step | change | status |
| --- | --- | --- |
| 2a | `NavContext<'a>` bundling `DocQuery`, `&mut ViewportState`, `&VisibleFrame`, `&dyn RendererHost` | not started |
| 2b | Move within-document navigation onto `Navigator` taking `NavContext` | not started |

Stays on `RendererCore`: the 12 cross-document entry points, which ask the
navigator for a target and then perform the swap through the wide primitives.

Four methods need `provider` or `document_cache` rather than the context and
stay behind: `toc`, `resolve_doc_index_for_path`,
`footnote_preview_for_reference`, `load_document_for_index`.

### Expected friction

`NavContext` borrows four `RendererCore` fields at once, so orchestration that
builds a context and then touches another field will fight the borrow checker.
Mitigation: build the context at the point of use and keep it short-lived.

## Invariants

- All 117 tests pass after every step.
- Warning count stays at 4 (the pre-existing set).
- No public API change: `RendererCore` is `pub(crate)`, so its methods are not
  externally reachable.

## Outcome

Not yet implemented. This section records measured results once the steps
above are done.
