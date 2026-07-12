# Fixed pane layout — rough plan

## Goal

Replace today's free-form, arbitrarily-splittable pane layout with a fixed set
of layouts: **Two-column** and **Three-column**. Files target the "editor"
role; new terminals target the "terminal" role. Users can drag panes to
reorder columns; the system tracks which pane is "last active" per role so
that opening a file/terminal lands in the right place even after reordering.

Constraint: this is a fork that needs to keep pulling from upstream Zed, so
changes to high-churn upstream files (`pane.rs`, `pane_group.rs`, the `Pane`
struct itself) should be minimized. Prefer additive new modules/fields over
restructuring shared code.

## Current state (for reference)

- `PaneGroup { root: Member }`, `Member::{Pane(Entity<Pane>), Axis(PaneAxis)}`,
  `PaneAxis { axis, members: Vec<Member>, flexes }` — a fully recursive
  n-ary tree (`crates/workspace/src/pane_group.rs`). Any split action or
  drag-to-edge mutates this tree arbitrarily via `PaneAxis::split`.
- `Workspace.active_pane` / `Workspace.last_active_center_pane` — single
  "last active" pointer, no concept of pane role/kind.
- The terminal panel (`TerminalPanel`, `crates/terminal_view/src/terminal_panel.rs`)
  is a bottom **Dock** with its own *independent* `PaneGroup` — not part of
  the main center tree.
- "Center terminal" is not a distinct type — it's the same `TerminalView`
  (`impl Item`/`SerializableItem`) used by the dock, just inserted into a
  center `Pane` instead. Triggered by the `NewCenterTerminal` action →
  `TerminalPanel::add_center_terminal` (terminal_panel.rs:752), which
  currently hardcodes `workspace.active_pane()` as the destination.
- No item-kind-based routing exists today: opening a path either takes an
  explicit target pane or falls back to "last active center pane."

## Target model

### Four logical open-targets

Not a binary Editor/Terminal split — a column can be the default target for
more than one kind:

| | 2-column | 3-column |
|---|---|---|
| **Terminal** | col 0 (left) | col 0 (left) |
| **Alt-Terminal** | col 1 (right) | col 1 (middle) |
| **Editor** | col 1 (right) | col 2 (right) |
| **Alt-Editor** | col 1 (right) | col 1 (middle) |

New columns default to an empty text buffer regardless of which kinds will
later target them.

```rust
enum PaneKind { Terminal, AltTerminal, Editor, AltEditor }
enum LayoutKind { TwoColumn, ThreeColumn }

impl LayoutKind {
    fn column_count(self) -> usize { match self { Self::TwoColumn => 2, Self::ThreeColumn => 3 } }

    /// Static routing table — where a freshly-opened item of this kind lands
    /// when there's no recorded "last active" pane for it yet.
    fn default_column_for(self, kind: PaneKind) -> usize {
        use PaneKind::*;
        match (self, kind) {
            (Self::TwoColumn, Terminal) => 0,
            (Self::TwoColumn, AltTerminal | Editor | AltEditor) => 1,
            (Self::ThreeColumn, Terminal) => 0,
            (Self::ThreeColumn, AltTerminal | AltEditor) => 1,
            (Self::ThreeColumn, Editor) => 2,
        }
    }
}
```

### Layout representation — reuse existing tree shape, don't rewrite it

`PaneGroup`/`Member`/`PaneAxis` stay exactly as they are upstream. A fixed
layout is just a root `Member::Axis(PaneAxis { axis: Horizontal, members: [Pane, Pane(, Pane)] })`
with no nested `Member::Axis` children — already expressible with existing
types, so building it at workspace-creation time needs no changes to
`pane_group.rs`. "Fixed" is a *policy* we enforce elsewhere (never let a
further split introduce a nested axis), not a new data type.

### Role/kind tracking — not a `Pane` field

Adding a `role` field to the `Pane` struct would touch every construction
site of `Pane` upstream and be a merge-conflict magnet. Keep it entirely in
our own code.

The actual implementation (see "Implemented so far" below) resolves a role
straight from `Workspace.layout` with a static column-index match — no
per-pane side table exists. That's sufficient as long as columns aren't
reorderable. If/when dragging to reorder columns is added, a role → pane
mapping keyed by `Entity<Pane>` identity (rather than column index) would be
needed so a dragged pane keeps its role; that's deferred, unbuilt, and
should be designed fresh against whatever the reordering UI turns out to
need rather than against this now-stale sketch.

### Implemented so far

`Layout`/`LayoutRole` are a plain enum + match, not a trait-object
hierarchy — the set of layouts is closed and `Layout` already needs
`Deserialize` for settings, so a `dyn LayoutStrategy` split would just add
indirection without buying extensibility we need.

- `crates/workspace/src/layout.rs` — `Layout { FreeForm, TwoColumn,
  ThreeColumn }` and `LayoutRole { Terminal, AltTerminal, Editor,
  AltEditor }`.
- `Workspace.layout: Layout` (workspace.rs:1371) — new additive field,
  defaults to `Layout::TwoColumn` at construction (workspace.rs:1819).
  `Workspace::set_layout` (workspace.rs:5877) updates it.
- `Workspace::pane_for_layout_role` (workspace.rs:5881) — resolves a role to
  a pane by matching on `self.layout`:
  - `Layout::TwoColumn`: static column mapping (`Terminal` → column 0,
    `AltTerminal`/`Editor`/`AltEditor` → column 1), read off
    `self.center.panes()` by index.
  - All other layouts (`FreeForm`, and `ThreeColumn` until it's built out):
    fall back to `self.active_pane.clone()`.

Not yet done: `ThreeColumn`'s own column mapping, the `last_active_by_kind`
side table (so reordered panes keep their role), and the two touch points
below (blocking further splits, and threading the resolved pane into
`TerminalPanel::add_center_terminal`) that would make `pane_for_layout_role`
actually get called from anywhere.

### Minimal-footprint touch points in existing files

1. **Blocking further splits / drag-to-edge-splits.** `SplitLeft/Right/Up/Down`
   handlers and `Pane::handle_tab_drop`'s use of `drag_split_direction`
   (pane.rs:3790) are the only two call sites that invoke `PaneGroup::split`.
   Gate both with a cheap check ("is a fixed layout active? if so, no-op or
   reinterpret as a column reorder") rather than touching `PaneAxis::split`
   itself.

2. **Center terminal destination.** `TerminalPanel::add_center_terminal`
   (terminal_panel.rs:752) hardcodes `workspace.active_pane()`. Thread an
   optional target-pane parameter through that one function (defaulting to
   today's behavior when `None`); our routing logic passes the resolved pane
   when a fixed layout is active. One signature change, not a restructure.

3. **Role tracking for reorderable columns** (only needed once dragging to
   reorder columns exists — not required for the static column-index match
   used today). Would likely hook into the existing focus-change
   observation on `Workspace` (around workspace.rs:4198, where `active_pane`
   is updated today), but the actual shape is unbuilt and TBD — see "Role/kind
   tracking" above.

Net diff surface: a couple of guard checks in `pane.rs`, one new optional
parameter in `terminal_panel.rs`, and one new module of ours holding the new
types/logic. `pane_group.rs` is untouched; `Pane`'s struct/fields are
untouched.

## Open questions

1. What are the actual actions/keybindings for the two "alt" opens (e.g.
   "Open File in Alt Pane", "New Alt Terminal")? Do they already exist under
   different names, or are they new?
2. Layout switching UI: is Two-column ↔ Three-column a per-workspace toggle?
   What happens to existing column contents when column count changes (e.g.
   3→2 — does the middle column's content merge into col 1, or close)?
3. Serialization/migration: existing saved workspaces have arbitrary
   `PaneGroup` trees — one-time migration path needed, or is starting fresh
   acceptable?
4. Does dragging ever need to *swap* kinds (e.g. drag the terminal pane into
   the alt-editor slot), or only reorder position while each pane keeps its
   own kind?
5. Does the bottom terminal dock go away entirely, or stay as an escape
   hatch for terminals beyond the two fixed slots?
