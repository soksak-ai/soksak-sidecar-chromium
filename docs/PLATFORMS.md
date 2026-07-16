# Cross-platform rendering and presentation

How the Chromium engine sidecar renders and presents pane content on macOS,
Windows, and Linux. This is the authoritative rule for platform work in this
crate; do not derive a different one from older notes.

## The rule

The sidecar owns native presentation. The engine renders offscreen through CEF
(windowless + shared texture); each platform presents the shared texture onto a
module-owned native surface. Presentation stays inside the sidecar and is not
moved to the core — a per-OS presenter behind one interface is the only shape
consistent with keeping the working macOS path unchanged.

The frame path never crosses the host vtable, IPC, or JS (SIDECARS.md §8): the
pixels are blitted from CEF's shared texture straight onto the native surface.

## Interface and per-OS modules

`presenter/mod.rs` is a platform-neutral interface (surface lifecycle, bounds,
hidden, popup, present). `engine.rs` calls only this interface and never a
platform module directly.

- `presenter/macos.rs` — production. IOSurface → Metal blit → CALayer.
- `presenter/windows.rs` — D3D11 shared HANDLE → DirectComposition.
- `presenter/linux.rs` — DMA-BUF plane fds → EGLImage → GL on an X11 child.

The per-OS split is inherent, not a design choice: CEF's `on_accelerated_paint`
hands a different handle per platform (macOS IOSurface pointer, Windows D3D11
`HANDLE`, Linux DMA-BUF planes), and the `cef` crate ships a separate importer
for each (`osr_texture_import/{iosurface,d3d11,dmabuf}.rs`). Metal, D3D11, and
EGL share no code. `present` takes `&AcceleratedPaintInfo` so each presenter
extracts its own field.

## Oracle

`offscreen.rs` is the frozen reference (oracle), compiled only under the
`harness` feature. It is not the production path and must not be recycled as
one — an oracle reused as the implementation cannot verify anything.
`presenter/macos.rs` is the production copy that reproduces its algorithm; the
harness asserts the production output matches the oracle.

## Verification

- macOS and Linux compile locally: `cargo check --target <triple>`. Capture the
  exit code directly — `cargo check … | tail` reports the pipe's exit, not
  cargo's, and hides failures.
- Windows compiles in CI only. `cef-dll-sys` builds CEF's C++ wrapper with a
  resource compiler that is absent when cross-compiling from macOS. Linux is the
  local proxy for non-macOS code correctness.
- Native present is verified per-OS at runtime in CI (Linux under xvfb). A stub
  that only compiles is not a passing platform.

### Equivalence across platforms

Two planes, mirroring the terminal contract (canonical projection + oracle):

- Control plane, canonical: each presenter's frame-path decisions (surface
  scale, present coded size, colorspace, popup rect) project to a canonical form
  compared byte-exact across OS — the three presenters must make the same
  decisions.
- Data plane, fidelity: per OS, the presented surface equals the CEF source
  frame (the presenter is a pixel-preserving conduit). CEF guarantees the source
  is equivalent across OS.

## Status

Compiles GREEN on macOS and Linux; Windows builds in CI. Native present on
Windows and Linux is stubbed with a logged marker (not silent success) pending
the D3D11/DirectComposition and DMA-BUF/EGL implementations. CEF loading on
Linux/Windows (`libcef.{so,dll}`) is stubbed pending implementation; only the
macOS `.framework` path is wired.
