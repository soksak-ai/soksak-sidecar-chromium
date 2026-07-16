# 크로스플랫폼 렌더·프레젠테이션

Chromium 엔진 사이드카가 macOS·Windows·Linux에서 pane 콘텐츠를 렌더·present하는
방식. 이 크레이트의 플랫폼 작업 정본 규칙이다. 옛 노트에서 다른 규칙을 유도하지 않는다.

## 규칙

**사이드카가 네이티브 프레젠테이션을 소유한다.** 엔진은 CEF로 offscreen 렌더하고
(windowless + 공유 텍스처), 각 플랫폼이 그 공유 텍스처를 모듈 소유 네이티브 표면에
present한다. 프레젠테이션은 사이드카 안에 두며 코어로 옮기지 않는다 — 작동하는 macOS
경로를 불변으로 두는 한, 한 인터페이스 뒤의 per-OS 프레젠터가 유일하게 정합적인 형태다.

프레임 경로는 호스트 vtable·IPC·JS를 통과하지 않는다(SIDECARS.md §8): 픽셀은 CEF 공유
텍스처에서 네이티브 표면으로 직접 blit된다.

## 인터페이스와 per-OS 모듈

`presenter/mod.rs`는 플랫폼 무관 인터페이스(서피스 수명·bounds·hidden·popup·present).
`engine.rs`는 이 인터페이스만 부르고 플랫폼 모듈을 직접 부르지 않는다.

- `presenter/macos.rs` — 프로덕션. IOSurface → Metal blit → CALayer. 동결 레퍼런스
  경로이고 hand-rolled(raw Metal) 그대로 불변.
- `presenter/windows.rs`·`presenter/linux.rs` — CEF 공유 텍스처를 `cef` 크레이트의
  `osr_texture_import`(피처 `accelerated_osr`)로 `wgpu::Texture` 로 가져와, 네이티브
  child 창의 `wgpu::Surface` 에 렌더한다.

CEF `on_accelerated_paint`는 플랫폼별로 다른 핸들을 준다(macOS IOSurface·Windows
D3D11 `HANDLE`·Linux DMA-BUF planes). 세 네이티브 GPU 스택을 손으로 굴리는 대신 신규
플랫폼은 크레이트의 통합 임포터를 쓴다: `SharedTextureHandle::new(info).import_texture(&device)`
가 `wgpu::Texture` 를 반환한다(Linux DMA-BUF→Vulkan·Windows D3D11→Vulkan interop, CPU
폴백 포함). 그래서 Windows·Linux 는 present 메커니즘 하나(wgpu)를 공유하고, macOS 만
raw Metal(작동 경로=동결 레퍼런스)에 남는다. `present`는 `&AcceleratedPaintInfo` 를 받아
각자 소비한다(macOS 는 `shared_texture_io_surface`, wgpu 프레젠터는 info 전체를 임포터에).
wgpu 는 `cef` 크레이트가 쓰는 버전(29)에 핀하고 non-macOS 타깃에서만 켜, macOS dylib 은
wgpu 를 끌어오지 않는다.

## 오라클

`offscreen.rs`는 동결 레퍼런스(오라클)이고 `harness` 피처에서만 컴파일된다. 프로덕션
경로가 아니며 재활용하지 않는다 — 구현으로 재활용한 오라클은 아무것도 검증 못 한다.
`presenter/macos.rs`가 그 알고리즘을 재현하는 프로덕션 사본이고, 하니스가 프로덕션
출력 == 오라클 출력을 단언한다.

## 검증

- macOS·Linux는 로컬 컴파일: `cargo check --target <triple>`. exit 코드를 직접 잡는다 —
  `cargo check … | tail`은 파이프(tail)의 exit를 보고해 실패를 가린다.
- Windows는 컴파일·링크 모두 CI 전용. `cef-dll-sys`가 CEF C++ 래퍼를 빌드할 때 리소스
  컴파일러를 요구하는데 macOS 크로스컴파일 환경엔 없다. Linux가 비-macOS 코드 정합성의
  로컬 프록시이고, Windows 링크는 CI 빌드 매트릭스의 몫이다.
- 현재 다섯 타깃 전부 CI 에서 `cargo build`(컴파일+링크) 통과. 네이티브 present 는 각 OS
  런타임 CI 검증(Linux xvfb)이 남았다. 컴파일만 되는 타깃은 아직 렌더 플랫폼이 아니다.

### 플랫폼 간 멱등

터미널 계약(정규형 투영 + 오라클)을 옮긴 두 평면:

- 제어면(canonical): 각 프레젠터의 프레임경로 결정(surface scale·present coded size·
  colorspace·popup rect)을 정규형으로 투영해 cross-OS byte-exact 대조 — 세 프레젠터가
  같은 결정을 해야 한다.
- 데이터면(fidelity): 각 OS에서 present된 표면 == CEF 원본 프레임(프레젠터는 픽셀 무손실
  도관). CEF가 원본을 cross-OS 동일하게 보장한다.

## 상태

다섯 타깃 전부 — darwin arm64/x64·linux arm64/x64·windows x64 — CI 에서 컴파일·링크
통과(`.github/workflows/ci.yml`, 발행 없는 빌드 매트릭스).

- **macOS**: 프로덕션 present(raw Metal, `presenter/macos.rs`)는 harness 런타임 검증됨
  (프레임 present + 입력, IME 포함).
- **Linux**: `presenter/linux.rs` — 부모 XID 아래 X11 child 창(`x11-dl`), 그 위 `wgpu::Surface`,
  `osr_texture_import` → textured-quad 렌더 → present — CI(`onscreen.yml`)에서 런타임 검증됨: harness 를
  xvfb + lavapipe 로 오프스크린 구동하면 프레젠터가 실제 프레임을 present 하고, cefQuery + 입력(IME 포함)
  라운드트립이 통과한다.
- **Windows**: `presenter/windows.rs`(같은 wgpu present + HWND child 창)는 컴파일·링크 통과하고,
  `onscreen.yml` 의 windows job 이 harness 를 실행(DX12 WARP 소프트웨어 어댑터)해 **Linux 와 동일 기준**으로
  검증한다: Windows CEF dist 스테이징·실제 프레임 present·전체 입력 라운드트립(IME_OK:a한글) 통과. Windows
  전용 수정 3개: 브라우저 생성 직후 `WasResized`(OSR 입력 hit-test 가 뷰 확정을 요구 — Linux 는 회귀시켜
  Windows 한정), CHAR 키코드(CEF 가 Windows 서 `windows_key_code` 를 문자로 해석), harness 입력 재전송
  (첫 클릭이 OSR 뷰의 입력-준비 시점과 race). macOS 에서 빌드 불가라 CI 가 유일 경로, 기본 target dir 이
  `MAX_PATH` 를 넘어 CI 는 `CARGO_TARGET_DIR` 을 짧은 루트로 둔다.
- Linux/Windows CEF 는 빌드타임 링크(`cef-dll-sys` 가 `cargo::rustc-link-lib` 방출)라 런타임
  로더가 없다 — 리소스 경로(`resources_dir_path`·`locales_dir_path`·`browser_subprocess_path`)만
  설정한다. macOS 만 `.framework` 런타임 로더를 쓴다.

Linux 는 가속(`on_accelerated_paint`→import→present)·CPU 폴백(`on_paint`→업로드→`present_cpu`)
두 경로가 배선됨. SW GL CI(lavapipe)엔 하드웨어 DMA-BUF 가 없어 CEF 가 CPU 경로를 타므로, 그 폴백이
CI 에서 프레임이 뜨게 하는 관건이다.

메시지 펌프는 per-OS 다. macOS 는 사이드카가 GCD 메인큐로 `do_work` 를 코어 런루프에 마셜한다.
Windows·Linux 엔 그 심볼이 없어 `schedule_pump` 이 가장 이른 만기를 기록하고 `drive_pump` 이
실행한다 — 코어 메인루프가 tick 하는 seam(Windows 메시지 전용 창, Linux glib idle 소스)으로,
macOS 런루프가 GCD 큐를 비우는 것과 동형이다. 그 코어측 tick 배선이 남은 런타임 단계다.

## 검증 게이트

| 범위 | 검증 방법 | 위치 |
|---|---|---|
| macOS 프로덕션 present | harness 가 페이지 렌더 — 프레임+입력(IME 포함) | 로컬(harness 실행) |
| 다섯 타깃 컴파일+링크 | `cargo build --target <triple>` 매트릭스(build 는 링크, check 는 아님) | CI(`ci.yml`) |
| macOS/Linux 코드 정합 | `cargo check --target <triple>`(exit 직접 잡기) | 로컬 |
| 온스크린 렌더(Linux) | xvfb + lavapipe 에서 harness 실행, 프레임+입력 단언 | CI(`onscreen.yml`) |
| 온스크린 렌더(Windows) | harness 실행(DX12/WARP), 프레임+전체 입력 단언 | CI(`onscreen.yml`) |
| cross-OS 멱등 | 제어면 canonical 레코드 + per-OS 데이터면 fidelity | 구축 예정 |

## 디버깅

앱 없이 엔진 구동:

    cargo run --release --features harness --bin harness -- <dist-dir> offscreen

`make sidecar-chromium`(또는 `stage.sh <dist-dir>`)가 CEF 프레임워크·helper 번들을 스테이징한다.
PASS(exit 0)=cefQuery 왕복 + offscreen 이면 present 경로·입력까지 성공. 프레임은
`stats.dbg.framesPresented`, 각 프레젠터는 첫 present 를 1회 로그. 흔한 실패: macOS 는 blank=
`Helper (Renderer).app` 변형 누락, Linux 는 프레임 0=child 창이 부모 아래 미맵 또는 CPU 폴백 미가동.

## 로드맵

- **A/B — 완료**: presenter 인터페이스+오라클 분리, 크레이트 게이트 해제, macOS·Linux 클린 컴파일.
- **C/D — 완료**: `presenter/linux.rs`·`presenter/windows.rs` 둘 다 가속·CPU 폴백 두 경로 구현
  (X11/HWND child, 같은 wgpu 파이프라인).
- **E — 완료**: Linux/Windows CEF 는 빌드타임 링크. 비-macOS init 은 프레임워크 적재 대신
  리소스 경로만 설정한다.
- **Phase 0 — 완료**: 다섯 타깃(darwin arm64/x64·linux arm64/x64·windows x64) 전부 CI 에서
  컴파일·링크(`ci.yml` 빌드 매트릭스). 메시지 펌프는 per-OS 게이트(macOS GCD, 비-macOS `drive_pump` seam).
- **온스크린(Linux) — 완료**: `onscreen.yml` 이 Linux harness 를 빌드·Linux CEF dist 스테이징 후
  xvfb + 소프트웨어 Vulkan(lavapipe)로 오프스크린 구동한다. 프레젠터가 실제 프레임을 present 하고
  (`framesPresented` 증가) 전 입력 라운드트립이 통과 — macOS harness 와 동일 단언.
- **온스크린(Windows) — 완료**: 같은 harness 를 windows-2025(DX12 WARP)에서 돌려 Windows dist 스테이징·
  실제 프레임 present·전체 입력 라운드트립(IME_OK:a한글) 통과 — Linux 와 동일 기준의 blocking 게이트.
- **F — 예정**: 코어가 프레젠터에 per-OS 부모 핸들(X11 XID / HWND)을 macOS NSView 경로 옆에 넘기고,
  메인루프에서 `soksak_sidecar_engine_tick`(→`drive_pump`)를 tick(glib idle / 메시지 전용 창)한다;
  5타깃 릴리스 매트릭스 CI. 이 tick 의 사이드카 쪽은 이미 있고 온스크린 harness 가 구동한다.
- **멱등**: 제어면 canonical 투영 cross-OS 대조 + per-OS 데이터면 fidelity.
