# Terminal UI Conventions

These conventions express reviewable interaction and presentation principles.
Specific key bindings, geometry constants, and current widget inventories
belong to command definitions, code, and tests.

## Interaction and discoverability

- Keep action and navigation affordances in the header or the relevant view;
  keep status, progress, and background telemetry in the footer. Report the
  operation actually underway, not an assumed future phase.
- Derive keyboard dispatch and Help from the same typed command declarations.
  Display configured modifiers, not hardcoded physical shortcuts. Reserve
  persistent view space for content; contextual shortcut hints belong in
  dialogs and Help rather than a permanent action bar.
- Structural navigation may use direct letter keys. When an editor captures
  text, every unmodified printable character inserts text instead of firing
  a navigation command. Typing, paste routing, and loss-of-work protection
  must agree on which surface owns the input.
- Every action must be reachable without function keys. Keep basic movement
  and dismissal consistent across views; do not make a destructive operation
  share an execution or copy shortcut.
- Make the scale of an action visible: a selected-item command must not
  silently become a batch command. Keep execution separate from status
  editing and from destructive clearing.

## Safe and legible controls

- Destructive actions and long-running autonomous launches require an explicit
  confirmation with a safe default. Check that the action has a valid target
  before offering to confirm it. Distinguish continuing, finishing the current
  batch member, and immediately cancelling when those choices are available.
- Give dialogs a recognizable title, readable content, explicit actions, and
  contextual guidance. Separate content from borders and controls; keep
  rendering and pointer hit-testing derived from the same geometry. Covered
  or hidden controls must not remain clickable.
- In hierarchical views, indent children more than parents and keep the
  parent's details visually attached to it. Support local and global folding
  without sacrificing a dense collapsed overview.
- Preserve the reader's position across folds, scrolling, stream resets,
  and bounded-window eviction. An interaction should not lose a keystroke or
  move the user away from the material they are reviewing without intent.

## Presentation and state

- Render through semantic theme roles rather than raw terminal colors.
  Opaque overlays must occlude underlying content, and focus, danger, and
  status must remain distinguishable across supported color depths.
- Resolve user-facing interface text through the localization catalog, with
  width budgets for constrained controls. Paths, hashes, code, and model
  output remain technical data rather than translated UI copy.
- Bound retained presentation caches and windows. Discarding an off-screen
  rendering must not discard the underlying durable record.
- Treat client projections and previews as presentation aids, not authority
  for persisted state. Reuse shared matching rules rather than implementing
  a subtly different UI-only version; the server and application validate
  any durable transition.
