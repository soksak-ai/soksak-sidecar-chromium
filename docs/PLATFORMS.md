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

- `presenter/macos.rs` — production. IOSurface → Metal blit → CALayer. This is
  the frozen reference path and stays hand-rolled (raw Metal), untouched.
- `presenter/windows.rs`, `presenter/linux.rs` — import CEF's shared texture with
  the `cef` crate's `osr_texture_import` (feature `accelerated_osr`) into a
  `wgpu::Texture`, then render it to a `wgpu::Surface` on the native child window.

CEF's `on_accelerated_paint` hands a different handle per platform (macOS
IOSurface pointer, Windows D3D11 `HANDLE`, Linux DMA-BUF planes). Rather than
hand-roll three native GPU stacks, the new platforms use the crate's unified
importer: `SharedTextureHandle::new(info).import_texture(&device)` returns a
`wgpu::Texture` (Linux DMA-BUF→Vulkan, Windows D3D11→Vulkan interop), with a CPU
fallback. So Windows and Linux share ONE present mechanism (wgpu); only macOS
stays on raw Metal because its working path is the frozen reference. `present`
takes `&AcceleratedPaintInfo` and each presenter consumes it its own way (macOS
reads `shared_texture_io_surface`; wgpu presenters pass the whole info to the
importer). wgpu is pinned to the version the `cef` crate uses (29) and enabled
per-target for non-macOS only, so the macOS dylib does not pull it in.

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
- Windows compiles and links in CI only. `cef-dll-sys` builds CEF's C++ wrapper
  with a resource compiler that is absent when cross-compiling from macOS. Linux
  is the local proxy for non-macOS code correctness; the Windows link is the CI
  build matrix's job.
- All five targets currently pass `cargo build` (compile + link) in CI. Native
  present is still to be verified per-OS at runtime in CI (Linux under xvfb). A
  target that only compiles is not yet a rendering platform.

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

All five targets — darwin arm64/x64, linux arm64/x64, windows x64 — compile and
link in CI (`.github/workflows/ci.yml`, a build-only matrix, no publish).

- **macOS**: production present (raw Metal, `presenter/macos.rs`) is
  runtime-verified via the harness (frames presented + input, including IME).
- **Linux**: `presenter/linux.rs` — X11 child window under the parent XID
  (`x11-dl`), a `wgpu::Surface` on it, `osr_texture_import` → textured-quad
  render → present — is runtime-verified in CI (`onscreen.yml`): the harness
  runs offscreen under xvfb + lavapipe, the presenter presents real frames, and
  the full cefQuery + input (incl. IME) round-trip passes.
- **Windows**: `presenter/windows.rs` — the same wgpu present with an HWND child
  window (`windows` crate) — compiles and links in CI, and `onscreen.yml`'s
  windows job runs the harness (DX12 WARP software adapter) to the same standard
  as Linux: it stages a Windows CEF dist, presents real frames, and passes the
  full input round-trip (IME_OK:a한글). Three Windows-specific fixes were needed:
  `WasResized` right after browser creation (OSR input hit-testing needs the
  view established — gated to Windows, since it regresses Linux), the CHAR key
  code (CEF reads `windows_key_code` as the character on Windows), and harness
  input resend (the first click races the OSR view becoming input-ready). It
  cannot be built from macOS (the CEF C++ wrapper needs the Windows resource
  compiler), so CI is the only path, and the default target dir crosses
  `MAX_PATH` there, so CI points `CARGO_TARGET_DIR` at a short root.
- CEF is linked at build time on Linux/Windows (`cef-dll-sys` emits
  `cargo::rustc-link-lib`), so there is no runtime loader to wire — only the
  resource paths (`resources_dir_path`, `locales_dir_path`,
  `browser_subprocess_path`). Only macOS uses the `.framework` runtime loader.

Both the accelerated (`on_accelerated_paint` → import → present) and the CPU
fallback (`on_paint` → upload → `present_cpu`) paths are wired for Linux; on
software-GL CI (lavapipe) CEF has no hardware DMA-BUF and takes the CPU path, so
that fallback is what makes frames appear in CI.

The message pump is per-OS. On macOS the sidecar marshals `do_work` onto the
core's run loop through the GCD main queue. Windows and Linux have no such
symbol, so `schedule_pump` records the earliest due time and `drive_pump` runs
it — the seam the core's main loop ticks (a message-only window on Windows, a
glib idle source on Linux), mirroring how the macOS run loop drains the GCD
queue. Wiring that core-side tick is the remaining runtime step.

## Verification gates

| Scope | How it is verified | Where |
|---|---|---|
| macOS production present | harness renders a page; frames + input (incl. IME) | local, run the harness |
| All five targets compile + link | `cargo build --target <triple>` matrix (build links; check does not) | CI (`ci.yml`) |
| macOS/Linux code correctness | `cargo check --target <triple>` (capture exit directly) | local |
| On-screen render (Linux) | run the harness under xvfb + lavapipe, assert frames + input | CI (`onscreen.yml`) |
| On-screen render (Windows) | run the harness (DX12/WARP), assert frames + full input | CI (`onscreen.yml`) |
| Cross-OS equivalence | canonical control-plane record + per-OS data-plane fidelity | to build |

## Debugging

Run the engine without the app:

    cargo run --release --features harness --bin harness -- <dist-dir> offscreen

`make sidecar-chromium` (or `stage.sh <dist-dir>`) stages the CEF framework and
helper bundles. PASS (exit 0) means the cefQuery round-trip and, in offscreen
mode, the present path and input all worked. Frames are exposed as
`stats.dbg.framesPresented`; each presenter logs its first present once. Common
failures: on macOS, blank content usually means a missing `Helper (Renderer).app`
variant; on Linux, no frames usually means the child window was not mapped under
the parent, or the CPU fallback is not being exercised.

## Roadmap

- **A/B — done**: presenter interface + oracle split; crate un-gated; macOS and
  Linux compile clean.
- **C/D — done**: `presenter/linux.rs` and `presenter/windows.rs` both implement
  the accelerated and CPU-fallback paths (X11/HWND child, same wgpu pipeline).
- **E — done**: CEF is linked at build time on Linux/Windows; the non-macOS
  init sets the resource paths instead of loading a framework.
- **Phase 0 — done**: all five targets (darwin arm64/x64, linux arm64/x64,
  windows x64) compile and link in CI (`ci.yml` build matrix). The message pump
  is gated per-OS (macOS GCD, non-macOS `drive_pump` seam).
- **On-screen (Linux) — done**: `onscreen.yml` builds the Linux harness, stages
  a Linux CEF dist, and runs it under xvfb + software Vulkan (lavapipe). The
  presenter presents real frames (`framesPresented` climbs) and the full input
  round-trip passes — same assertion as the macOS harness.
- **On-screen (Windows) — done**: the same harness on windows-2025 (DX12 WARP)
  stages a Windows dist, presents real frames, and passes the full input
  round-trip (IME_OK:a한글) — the same standard as Linux, a blocking gate.
- **F — to do**: the core hands the presenter a per-OS parent handle (X11 XID /
  HWND) next to the macOS NSView path, and ticks `soksak_sidecar_engine_tick`
  (which runs `drive_pump`) from its main loop (glib idle / message-only
  window); 5-target release-matrix CI. The sidecar side of this tick already
  exists and the on-screen harness drives it.
- **Equivalence**: control-plane canonical projection compared across OS + a
  per-OS data-plane fidelity check.
