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
| 1a | `Debounced<T>` owning compare/assign/emit; bundle the four `update_nav_signal` markers into `NavSignals` | done |
| 1b | `LocationHistory` owning the stack, cursor and availability marker; route `emit_state_snapshot` through it | done |
| 1c | `TocAnchors` owning `toc_anchor_strings_by_doc` + `toc_anchor_filter`, rebuilt from `DocQuery` | done |

Left bare on `NavigationState`: `current_doc_index`, `document_uris`,
`document_text_lengths`, `nav_anchor_glyph`, `pending_nav_anchor_update`. These
are read 50+ times across the renderer and are genuinely shared; wrapping them
buys nothing.

## Phase 2 — extract `Navigator`

| step | change | status |
| --- | --- | --- |
| 2a | `NavView` (read-only) and `NavContext` (read-write) over `DocQuery`, viewport, frame, host | done |
| 2b | Move within-document navigation onto `NavigationState` taking those contexts | done |

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

### Phase 1 (done)

`NavigationState`: 14 fields to 8. Defects 1 and 2 fixed; defect 3 outstanding.

`emit_state_snapshot` now calls `LocationHistory::emit_availability` instead of
emitting `HistoryAvailability` from its own inline predicate, so the dedup
marker is always written. `Debounced<T>` makes the old failure mode
unrepresentable: emitting requires going through the type that stores.

One test assertion changed shape. It reached into `location_history` and
`location_history_index` to assert the cursor sat on the last entry; it now
asserts `history.is_at_newest()`, which states the same property without
depending on the representation.

117 tests pass, 4 warnings.

### Phase 2 (done)

23 methods moved to `impl NavigationState` in `renderer/navigate.rs` (302
lines). The file references no `RendererCore` field outside the two contexts --
no pipeline, provider, cache, revisions or column layout -- so the claim that
within-document navigation needs only document, viewport, frame and host is now
checkable by the compiler rather than asserted.

Two deviations from the plan as written:

- **No separate `Navigator` type.** The methods went onto `NavigationState`
  itself, matching how `TableSelectionState`, `HighlightState` and
  `ColumnLayout` were handled in the earlier phase. A wrapper would have added a
  name without adding a boundary.
- **Two contexts, not one.** Queries only read the viewport while movement
  writes it. Forcing both through `&mut` would have made every read-only
  forwarder on `RendererCore` take `&mut self`, which several callers cannot
  provide. `NavContext::view()` produces the read-only form.

Defect 3 is fixed: `set_progress_fraction` now calls
`NavigationState::document_at_fraction`, the named inverse of
`reading_progress`, instead of repeating the walk over `document_text_lengths`.
Both are now pure functions of that field and testable directly.

`RendererCore` keeps thin forwarders, so no call site outside `nav.rs` changed.
The predicted borrow-checker friction appeared exactly once, in the two
forwarders that mutate `nav` while reading the document; `nav_cx` returning
`(&mut NavigationState, NavContext)` as disjoint field borrows resolves it.

117 tests pass; warning count unchanged at 4.
