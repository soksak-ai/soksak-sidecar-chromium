#!/bin/bash
# dist 스테이징의 단일 진실 — dev(make sidecar-chromium)와 CI(release.yml)가 같은 스크립트를 쓴다.
# 사용: stage.sh <dist-dir>   (cargo build --release 선행 전제; 이 크레이트 디렉토리에서 실행)
set -euo pipefail
dist="${1:?사용: stage.sh <dist-dir>}"
# windows CI 는 MAX_PATH 회피로 CARGO_TARGET_DIR 을 짧은 루트로 옮긴다 — 빌드 산출물 위치를 그에 맞춘다.
src="${CARGO_TARGET_DIR:-target}/release"

mkdir -p "$dist"

# ── Linux dist ────────────────────────────────────────────────────────────────
# libcef.so 는 빌드타임 링크되지만 런타임에도 dist 형제로 있어야 로드된다(LD_LIBRARY_PATH=dist).
# CEF 리소스(icudtl.dat·*.pak·locales/)와 서브프로세스 helper 를 CEF 배포판(cef-dll-sys OUT_DIR)에서
# 스테이징한다. 배포판 내부 구조(Release/·Resources/ 여부)는 find 로 탐색해 레이아웃에 견고하게.
if [ "$(uname -s)" = "Linux" ]; then
  cefdir=$(ls -dt "$src/build/"cef-dll-sys-*/out/cef_linux_* 2>/dev/null | head -1)
  if [ -z "$cefdir" ]; then
    echo "linux CEF 추출 미발견" >&2
    ls -la "$src/build/"cef-dll-sys-*/out/ 2>/dev/null >&2 || true
    exit 1
  fi
  echo "== CEF linux 추출 구조 ($cefdir) =="
  find "$cefdir" -maxdepth 2 -type d | sed "s#$cefdir#.#"
  mkdir -p "$dist/locales"
  # libcef.so + 런타임 라이브러리 + V8 스냅샷 + sandbox
  find "$cefdir" -maxdepth 2 \( -name 'libcef.so' -o -name 'libEGL.so' -o -name 'libGLESv2.so' \
    -o -name 'libvulkan.so*' -o -name '*.bin' -o -name 'chrome-sandbox' \) \
    -exec cp -n {} "$dist/" \; 2>/dev/null || true
  find "$cefdir" -maxdepth 3 \( -name 'libvk_swiftshader.so' -o -name 'vk_swiftshader_icd.json' \) \
    -exec cp -n {} "$dist/" \; 2>/dev/null || true
  # CEF 리소스
  find "$cefdir" -maxdepth 2 -name 'icudtl.dat' -exec cp {} "$dist/" \;
  find "$cefdir" -maxdepth 2 -name '*.pak' -exec cp {} "$dist/" \;
  find "$cefdir" -path '*/locales/*.pak' -exec cp {} "$dist/locales/" \;
  # 서브프로세스 helper — 엔진의 browser_subprocess_path 와 이름이 일치해야 한다(engine.rs:
  # dist/soksak-sidecar-browser-chromium-helper). 다른 이름이면 CEF execvp 실패 → GPU/렌더러 서브프로세스
  # 전멸 → "GPU process isn't usable" FATAL(실측). + 사이드카 .so(배포 완결성; 하니스는 rlib 링크라 불요).
  cp "$src/soksak-sidecar-browser-chromium-helper" "$dist/soksak-sidecar-browser-chromium-helper"
  cp -n "$src/libsoksak_sidecar_browser_chromium.so" "$dist/soksak-sidecar-browser-chromium.so" 2>/dev/null || true
  if [ ! -e "$dist/libcef.so" ]; then echo "libcef.so 미스테이징 — 위 구조 확인" >&2; exit 1; fi
  echo "스테이지 완료(linux): $dist"
  ls -la "$dist"
  exit 0
fi

# ── Windows dist ──────────────────────────────────────────────────────────────
# libcef.dll + 런타임 DLL(chrome_elf·libEGL·libGLESv2·vk_swiftshader·vulkan-1 등) + CEF 리소스 +
# helper.exe 를 CEF 배포판에서 스테이징한다. helper.exe 이름은 엔진 browser_subprocess_path 와 일치해야
# 한다(engine.rs: dist/soksak-sidecar-browser-chromium-helper.exe). git-bash 는 uname -s 가 MINGW*.
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    cefdir=$(ls -dt "$src/build/"cef-dll-sys-*/out/cef_windows_* 2>/dev/null | head -1)
    if [ -z "$cefdir" ]; then
      echo "windows CEF 추출 미발견" >&2
      ls -la "$src/build/"cef-dll-sys-*/out/ 2>/dev/null >&2 || true
      exit 1
    fi
    echo "== CEF windows 추출 구조 ($cefdir) =="
    find "$cefdir" -maxdepth 2 -type d | sed "s#$cefdir#.#"
    mkdir -p "$dist/locales"
    find "$cefdir" -maxdepth 2 -name '*.dll' -exec cp -n {} "$dist/" \;
    find "$cefdir" -maxdepth 2 -name '*.bin' -exec cp -n {} "$dist/" \; 2>/dev/null || true
    find "$cefdir" -maxdepth 2 -name 'icudtl.dat' -exec cp {} "$dist/" \;
    find "$cefdir" -maxdepth 2 -name '*.pak' -exec cp {} "$dist/" \;
    find "$cefdir" -path '*/locales/*.pak' -exec cp {} "$dist/locales/" \;
    cp "$src/soksak-sidecar-browser-chromium-helper.exe" "$dist/soksak-sidecar-browser-chromium-helper.exe"
    cp -n "$src/soksak_sidecar_browser_chromium.dll" "$dist/soksak-sidecar-browser-chromium.dll" 2>/dev/null || true
    if [ ! -e "$dist/libcef.dll" ]; then echo "libcef.dll 미스테이징 — 위 구조 확인" >&2; exit 1; fi
    echo "스테이지 완료(windows): $dist"
    ls -la "$dist"
    exit 0
    ;;
esac

# ── macOS dist ────────────────────────────────────────────────────────────────
# dylib 은 원자적 교체(temp + mv). in-place cp 로 같은 경로를 덮어쓰면, 옛 dylib 을 이미 mmap 한
# 프로세스가 있을 때 서명된 페이지 캐시와 새 내용이 불일치해 다른(신선) 프로세스의 dlopen 이
# "Code Signature Invalid"(SIGKILL)로 죽는다(실측). rename 은 새 inode 를 주어 옛 매핑과 분리 →
# 신선 프로세스는 항상 유효 서명을 본다. reach fetch(프로덕션 설치)의 원자적 install 과 동일 원칙.
dylib_tmp="$dist/.soksak-sidecar-browser-chromium.dylib.tmp.$$"
cp "$src/libsoksak_sidecar_browser_chromium.dylib" "$dylib_tmp"
mv -f "$dylib_tmp" "$dist/soksak-sidecar-browser-chromium.dylib"

# helper .app 변형 5종 — Chromium 은 렌더러를 " Helper (Renderer).app" 형제 번들에서 띄운다
# (변형 부재 시 렌더러 spawn 이 조용히 실패 = 콘텐츠 blank, 실측).
for v in "" " (Renderer)" " (GPU)" " (Plugin)" " (Alerts)"; do
  app="$dist/soksak-sidecar-browser-chromium Helper$v.app"
  exe="soksak-sidecar-browser-chromium Helper$v"
  bid=$(printf '%s' "helper$v" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9' '.' | tr -s '.' | sed 's/\.$//')
  mkdir -p "$app/Contents/MacOS"
  cp "$src/soksak-sidecar-browser-chromium-helper" "$app/Contents/MacOS/$exe"
  sed -e "s/__EXECUTABLE__/$exe/g" -e "s/__BUNDLE_ID_SUFFIX__/$bid/g" \
    resources/HelperInfo.plist > "$app/Contents/Info.plist"
  # cp 로 .app 안에 들어온 실행파일은 cargo 의 linker-signed adhoc 서명이 번들 문맥에서 무효가 된다
  # ("code has no resources but signature indicates they must be present"). macOS 는 서명 무효 서브
  # 프로세스를 SIGKILL(exit_code=9) → GPU/renderer 프로세스가 즉사해 콘텐츠 blank·앱 크래시(실측).
  # adhoc 재서명으로 번들 문맥에 맞는 유효 서명을 부여한다(dev 스테이징 — 배포는 실 인증서로 재서명).
  codesign --force --sign - "$app/Contents/MacOS/$exe"
done

# Chromium framework — cef 빌드 산출물(OUT_DIR)에서 심링크(아카이브는 tar -L 로 해소).
fw=$(ls -dt "$src/build/"cef-dll-sys-*/out/cef_macos_*/"Chromium Embedded Framework.framework" 2>/dev/null | head -1)
if [ -z "$fw" ]; then echo "framework 미발견(cef 빌드 산출물 없음)" >&2; exit 1; fi
ln -sfn "$(cd "$(dirname "$fw")" && pwd)/Chromium Embedded Framework.framework" "$dist/Chromium Embedded Framework.framework"
echo "스테이지 완료: $dist (helper 변형 5종)"
