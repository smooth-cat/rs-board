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

## Modified Behavior

- While AppKit is interpreting a physical key event, ordinary keyboard characters continue to be
  ignored by `insertText:` so they are delivered through `KeyboardInput` exactly once.
- Marked-text IME commits keep the existing `Preedit("")` plus `Commit` behavior.
- Async system text insertion, when the view is focused and IME input is allowed, emits one
  `Ime::Commit` and returns the IME state to `Ground`.
- A focused, IME-enabled view reports a zero-length selected range so AppKit can recognize the
  current insertion point as a valid system dictation target.
- The text input client reports its actual AppKit window level (required for RS Board's elevated
  capture editor windows) and returns the corresponding zero-length range for caret positioning.
- AppKit's `startDictation:` and `stopDictation:` text commands continue through the responder
  chain. Repeated key-down events for those commands are consumed so one hardware-key press cannot
  restart an active Dictation capture session; all other commands retain winit's existing handling.
- Empty strings, unfocused views, disabled IME input, and control characters other than CR/LF are
  ignored. CR/LF remain valid so dictation can insert real line breaks.

## Modified Files

- `src/platform_impl/macos/view.rs`

## Removal Conditions

Remove this directory and the root `[patch.crates-io]` entry once an upstream `winit` release used
by eframe/egui-winit includes equivalent macOS async `insertText:` handling and RS Board's macOS
dictation acceptance tests pass without the local patch.
