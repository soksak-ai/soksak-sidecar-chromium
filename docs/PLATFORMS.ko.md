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

- `presenter/macos.rs` — 프로덕션. IOSurface → Metal blit → CALayer.
- `presenter/windows.rs` — D3D11 shared HANDLE → DirectComposition.
- `presenter/linux.rs` — DMA-BUF plane fd → EGLImage → X11 child의 GL.

per-OS 분기는 설계 선택이 아니라 도메인의 성질이다: CEF `on_accelerated_paint`가
플랫폼별로 다른 핸들을 준다(macOS IOSurface 포인터·Windows D3D11 `HANDLE`·Linux
DMA-BUF planes). `cef` 크레이트도 임포터를 셋 따로 제공한다
(`osr_texture_import/{iosurface,d3d11,dmabuf}.rs`). Metal·D3D11·EGL은 공유 코드가 0.
`present`는 `&AcceleratedPaintInfo`를 받아 각 프레젠터가 자기 필드를 추출한다.

## 오라클

`offscreen.rs`는 동결 레퍼런스(오라클)이고 `harness` 피처에서만 컴파일된다. 프로덕션
경로가 아니며 재활용하지 않는다 — 구현으로 재활용한 오라클은 아무것도 검증 못 한다.
`presenter/macos.rs`가 그 알고리즘을 재현하는 프로덕션 사본이고, 하니스가 프로덕션
출력 == 오라클 출력을 단언한다.

## 검증

- macOS·Linux는 로컬 컴파일: `cargo check --target <triple>`. exit 코드를 직접 잡는다 —
  `cargo check … | tail`은 파이프(tail)의 exit를 보고해 실패를 가린다.
- Windows는 CI 전용. `cef-dll-sys`가 CEF C++ 래퍼를 빌드할 때 리소스 컴파일러를
  요구하는데 macOS 크로스컴파일 환경엔 없다. Linux가 비-macOS 코드 정합성의 로컬 프록시.
- 네이티브 present는 각 OS 런타임에서 CI 검증(Linux는 xvfb). 컴파일만 되는 스텁은
  합격한 플랫폼이 아니다.

### 플랫폼 간 멱등

터미널 계약(정규형 투영 + 오라클)을 옮긴 두 평면:

- 제어면(canonical): 각 프레젠터의 프레임경로 결정(surface scale·present coded size·
  colorspace·popup rect)을 정규형으로 투영해 cross-OS byte-exact 대조 — 세 프레젠터가
  같은 결정을 해야 한다.
- 데이터면(fidelity): 각 OS에서 present된 표면 == CEF 원본 프레임(프레젠터는 픽셀 무손실
  도관). CEF가 원본을 cross-OS 동일하게 보장한다.

## 상태

macOS·Linux 컴파일 GREEN, Windows는 CI 빌드. Windows·Linux의 네이티브 present는 로그
스텁(조용한 성공 아님)이며 D3D11/DirectComposition·DMA-BUF/EGL 구현 대기.
Linux/Windows의 CEF 적재(`libcef.{so,dll}`)도 스텁이며 구현 대기 — macOS `.framework`
경로만 배선됨.
