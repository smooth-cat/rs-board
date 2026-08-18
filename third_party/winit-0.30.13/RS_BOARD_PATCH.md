# RS Board winit 0.30.13 Patch

## Source

- Crate: `winit 0.30.13`
- Crate SHA-256: `a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`
- Upstream Git SHA: `e9809ef54b18499bb4f2cac945719ecc2a61061b`
- License: Apache-2.0, unchanged from upstream. See `LICENSE`.

This directory is a complete copy of the crates.io source package, kept outside the root workspace
and applied through the root `[patch.crates-io]` entry. The patch path is repository-relative so
`Cargo.lock` remains usable from different checkout paths and on other Apple Silicon Macs.

## Reason

macOS system dictation, the character viewer, and similar system text services can call
`NSTextInputClient::insertText:replacementRange:` outside the normal `keyDown:` /
`interpretKeyEvents:` path. Upstream `winit 0.30.13` ignores those async insertions in RS Board's
IME-enabled text fields, so focused egui text editors never receive dictated text.

The upstream macOS backend also flattens tablet-point mouse events into ordinary cursor and mouse
button events. That discards the normalized pressure reported by AppKit before it can reach egui's
existing pressure-aware touch event path.

## Modified Behavior

- While AppKit is interpreting a physical key event, ordinary keyboard characters continue to be
  ignored by `insertText:` so they are delivered through `KeyboardInput` exactly once.
- Marked-text IME commits keep the existing `Preedit("")` plus `Commit` behavior.
- Async system text insertion, when the view is focused and IME input is allowed, clears any stale
  marked-text preedit, emits one `Ime::Commit`, and returns the IME state to `Ground`. This keeps the
  first following control key, such as Tab or Escape, from being consumed as part of the commit.
- Escape is forwarded directly to the application when the IME is in `Ground` or `Disabled` with no
  active marked-text preedit. This lets the editor close while macOS Dictation remains active, while
  preserving Escape's normal role of cancelling an active IME composition.
- A focused, IME-enabled view reports a zero-length selected range so AppKit can recognize the
  current insertion point as a valid system dictation target.
- The text input client reports its actual AppKit window level (required for RS Board's elevated
  capture editor windows) and returns the corresponding zero-length range for caret positioning.
- AppKit's `startDictation:` and `stopDictation:` text commands continue through the responder
  chain. Repeated key-down events for those commands are consumed so one hardware-key press cannot
  restart an active Dictation capture session. When AppKit maps Escape to `stopDictation:`, that one
  command is instead routed to the application without stopping the active Dictation session; all
  other commands retain winit's existing handling.
- Empty strings, unfocused views, disabled IME input, and control characters other than CR/LF are
  ignored. CR/LF remain valid so dictation can insert real line breaks.
- Primary-button tablet-point down, drag, and up events are exposed through winit's existing
  `Touch` event with normalized AppKit pressure. Standalone `tabletPoint:` events are recognized by
  event type and synthesize a start phase on the first positive-pressure sample. Ordinary mouse and
  trackpad events are unchanged.
- Losing window focus clears any retained tablet contact so a later ordinary mouse-up is not
  mistaken for the end of an interrupted pen stroke, and emits a cancelled touch so egui releases
  its matching pointer emulation state.

## Modified Files

- `src/platform_impl/macos/view.rs`
- `src/platform_impl/macos/window_delegate.rs`
- `src/event.rs` (platform-support documentation only)

## Removal Conditions

Remove this directory and the root `[patch.crates-io]` entry once an upstream `winit` release used
by eframe/egui-winit includes equivalent macOS async `insertText:` handling and RS Board's macOS
dictation acceptance tests pass without the local patch.
