// soksak-sidecar-browser-chromium — Chromium 엔진 사이드카의 C ABI 표면(soksak-sidecar-engine ABI@1).
// 코어(sidecar.rs)가 dlopen 으로 이 심볼들을 해소한다. 규범 정의 = docs/SIDECARS.md §3.
//
// 계약 요점: 모든 export 는 catch_unwind 로 감싼다(-2 = 패닉 트랩 — FFI 경계 unwinding 금지),
// message/notify 는 임의 스레드에서 호출될 수 있고(엔진 내부가 메인큐로 큐잉), init/shutdown 은
// 호스트가 메인스레드를 보장한다. reply 버퍼는 모듈이 할당하고 호스트가 soksak_sidecar_engine_free 로 돌려준다.
//
// 멀티플랫폼: 크레이트는 5타깃(darwin arm64/x64·linux arm64/x64·windows) 컴파일. 플랫폼별 조각은 engine.rs
// cfg(macos) 분기 + presenter/{macos,windows,linux}. macOS 전용 crate-level 게이트는 없다.

mod engine;
// 동결 오라클(검증 하니스 전용) — 프로덕션 프레젠터는 presenter/macos.rs. 오라클은 재활용하지 않는다.
// equivalence 비교(멱등 검증 백본)가 아직 배선되지 않아 하니스 빌드에서 미사용 = 의도된 dead_code 를
// 허용한다(offscreen.rs 는 byte 불변 유지 — allow 는 여기 mod 선언에만). 배송 dylib 엔 미포함.
#[cfg(all(target_os = "macos", feature = "harness"))]
#[allow(dead_code)]
mod offscreen;
mod presenter;

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

// ── 호스트 vtable 사본(코어 sidecar.rs 와 동일 레이아웃 — SIDECARS.md §3 이 규범) ──────────────

pub const HOST_ABI_VERSION: u32 = 1;

#[repr(C)]
pub struct SoksakSidecarEngineAbi {
    pub abi: u32,
    pub interface: *const c_char,
    pub version: *const c_char,
}
unsafe impl Sync for SoksakSidecarEngineAbi {}

#[repr(C)]
pub struct SoksakSidecarEngineHost {
    pub abi: u32,
    pub ctx: *mut c_void,
    pub emit: extern "C" fn(ctx: *mut c_void, json: *const u8, len: usize),
    pub log: extern "C" fn(ctx: *mut c_void, level: i32, msg: *const u8, len: usize),
}

#[repr(C)]
pub struct SoksakBuf {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

// 저장된 호스트 콜백(값 복사 — 호스트가 leak 으로 영구 보장). Send/Sync: fn 포인터 + 호스트가
// 임의 스레드 호출을 허용하는 계약이므로 안전.
struct HostFns {
    ctx: usize,
    emit: extern "C" fn(ctx: *mut c_void, json: *const u8, len: usize),
}
static HOST: OnceLock<HostFns> = OnceLock::new();

// 엔진(engine.rs)이 이벤트를 호스트로 내보내는 유일한 문 — 열린 플러그인 채널 전부에 relay 된다.
pub(crate) fn host_emit_json(value: &serde_json::Value) {
    if let Some(h) = HOST.get() {
        let bytes = value.to_string().into_bytes();
        (h.emit)(h.ctx as *mut c_void, bytes.as_ptr(), bytes.len());
    }
}

// ── 자기기술(무매니페스트 — 바이너리가 곧 진실) ──────────────────────────────────────────────

static ABI: SoksakSidecarEngineAbi = SoksakSidecarEngineAbi {
    abi: HOST_ABI_VERSION,
    interface: c"soksak-spec-sidecar-browser".as_ptr(),
    version: c"0.0.1".as_ptr(),
};

// 모델 선언 = 이 심볼 가족(soksak_sidecar_engine_*)의 존재 그 자체 — model 필드로 재진술하지 않는다
// (이중진실 금지: engine 심볼을 export 하며 다른 모델을 주장하는 모순이 성립 불가하도록).
#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_abi() -> *const SoksakSidecarEngineAbi {
    &ABI
}

// ── init / shutdown (메인스레드 — 호스트 보장) ───────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_init(
    host: *const SoksakSidecarEngineHost,
    cfg_json: *const u8,
    cfg_len: usize,
) -> i32 {
    std::panic::catch_unwind(|| {
        if host.is_null() || cfg_json.is_null() {
            return 1;
        }
        let h = unsafe { &*host };
        if h.abi != HOST_ABI_VERSION {
            return 2;
        }
        let _ = HOST.set(HostFns { ctx: h.ctx as usize, emit: h.emit });
        let cfg_bytes = unsafe { std::slice::from_raw_parts(cfg_json, cfg_len) };
        let Ok(cfg) = serde_json::from_slice::<serde_json::Value>(cfg_bytes) else {
            return 3;
        };
        let Some(dist) = cfg.get("distDir").and_then(|d| d.as_str()) else {
            return 3;
        };
        if engine::initialize(std::path::Path::new(dist)) {
            0
        } else {
            4
        }
    })
    .unwrap_or(-2)
}

#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_shutdown() {
    let _ = std::panic::catch_unwind(engine::shutdown_engine);
}

// tick: 비-macOS 펌프 구동 — 코어 메인 스레드(=CEF UI 스레드)가 자기 런루프에서 프레임마다 부른다.
// macOS 는 GCD 가 do_work 를 코어 런루프에 자동 전달하므로 이 심볼이 없다. windows/linux 코어는
// 메시지 전용 창/glib idle 로 이 tick 을 배선한다(Phase F). 어느 스레드서 부르면 안 된다 — CEF UI 스레드만.
#[cfg(not(target_os = "macos"))]
#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_tick() {
    let _ = std::panic::catch_unwind(engine::drive_pump);
}

// ── message: 불투명 요청 디스패치(soksak-spec-sidecar-browser 프로토콜) ─────────────────────────

fn reply_into(buf: *mut SoksakBuf, value: serde_json::Value) {
    if buf.is_null() {
        return;
    }
    let mut bytes = value.to_string().into_bytes();
    let out = SoksakBuf { ptr: bytes.as_mut_ptr(), len: bytes.len(), cap: bytes.capacity() };
    std::mem::forget(bytes);
    unsafe { *buf = out };
}

fn dispatch(req: &serde_json::Value, surface: usize) -> Result<serde_json::Value, String> {
    let ty = req.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let id = || req.get("id").and_then(|v| v.as_u64()).map(|v| v as u32).ok_or("id 필요");
    let int = |k: &str| req.get(k).and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(0);
    match ty {
        // 능력 자기보고(스펙 §8) — 소비자가 create 전에 feature-detect 한다. version 은 상주 모듈의
        // 정체성 프로브 — 디스크 dylib 교체는 상주 모듈에 무효(never-unload)라, E2E 는 로드된 모듈이
        // 기대 빌드인지 caps 로 먼저 확인한다(스테일 상주 모듈 오검증 사고의 재발 방지 규칙).
        "caps" => Ok(serde_json::json!({
            "ok": true,
            "modes": ["windowed", "offscreen"],
            "version": env!("CARGO_PKG_VERSION"),
        })),
        "create" => {
            if surface == 0 {
                return Err("surface(부모 뷰) 없음".into());
            }
            let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("about:blank");
            // mode: additive 필드(생략 = windowed). offscreen 이면 scale(devicePixelRatio)로
            // 백킹 스토어 크기를 결정한다(bounds 는 양 모드 동일하게 논리 px — 스펙 §8).
            let offscreen_scale = match req.get("mode").and_then(|m| m.as_str()) {
                Some("offscreen") => {
                    Some(req.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0) as f32)
                }
                Some("windowed") | None => None,
                Some(other) => return Err(format!("미지 mode: {other}")),
            };
            // owner: additive 필드 — 생성 주체 태그(플러그인 id). 소유 기반 회수(reconcile)의 근거.
            let owner = req.get("owner").and_then(|o| o.as_str()).unwrap_or("").to_string();
            let id = engine::request_create(
                surface,
                int("x"),
                int("y"),
                int("w").max(1),
                int("h").max(1),
                url.to_string(),
                offscreen_scale,
                owner,
            );
            Ok(serde_json::json!({ "ok": true, "id": id }))
        }
        // ── offscreen 입력 포워딩(스펙 §8) — DOM 셀이 받은 입력을 플러그인이 되보낸 것 ─────────
        "mouse" => {
            let kind = match req.get("kind").and_then(|k| k.as_str()) {
                Some("move") => 0u8,
                Some("down") => 1,
                Some("up") => 2,
                other => return Err(format!("미지 mouse kind: {other:?}")),
            };
            engine::mouse(
                id()?,
                kind,
                int("x"),
                int("y"),
                int("button").clamp(0, 2) as u8,
                int("clicks").max(1),
                int("mods").max(0) as u32,
            );
            Ok(serde_json::json!({ "ok": true }))
        }
        "wheel" => {
            engine::wheel(id()?, int("x"), int("y"), int("dx"), int("dy"));
            Ok(serde_json::json!({ "ok": true }))
        }
        "key" => {
            let kind = match req.get("kind").and_then(|k| k.as_str()) {
                Some("down") => 0u8,
                Some("up") => 1,
                Some("char") => 2,
                other => return Err(format!("미지 key kind: {other:?}")),
            };
            // char: 단일 코드포인트 문자열(생략 가능). BMP 밖은 KEYEVENT_CHAR 로 표현 불가 — IME 경로 사용.
            let ch = req
                .get("char")
                .and_then(|c| c.as_str())
                .and_then(|s| s.encode_utf16().next())
                .unwrap_or(0);
            engine::key(id()?, kind, int("code"), ch, int("mods").max(0) as u32);
            Ok(serde_json::json!({ "ok": true }))
        }
        "ime" => {
            let kind = match req.get("kind").and_then(|k| k.as_str()) {
                Some("set") => 0u8,
                Some("commit") => 1,
                Some("finish") => 2,
                Some("cancel") => 3,
                other => return Err(format!("미지 ime kind: {other:?}")),
            };
            let text = req.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let caret = req.get("caret").and_then(|c| c.as_u64()).unwrap_or(u64::MAX) as u32;
            engine::ime(id()?, kind, text, caret);
            Ok(serde_json::json!({ "ok": true }))
        }
        "devtools-open" => {
            // DevTools 를 inspected 브라우저의 임베드 child 로 연다(새 탭의 surface+rect 사용).
            // 결과 id 는 일반 브라우저 id 와 동급 — 어댑터가 그 label 에 매핑해 bounds/hidden/close 를 건다.
            if surface == 0 {
                return Err("surface(부모 뷰) 없음".into());
            }
            let inspected = req
                .get("inspectedId")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .ok_or("inspectedId 필요")?;
            // screencast: 프론트엔드의 페이지 미리보기 패널 — 플러그인 설정이 정본(생략=끔).
            let screencast = req.get("screencast").and_then(|v| v.as_bool()).unwrap_or(false);
            let id = engine::request_create_devtools(
                surface,
                int("x"),
                int("y"),
                int("w").max(1),
                int("h").max(1),
                inspected,
                screencast,
            
                req.get("owner").and_then(|o| o.as_str()).unwrap_or("").to_string(),
            );
            Ok(serde_json::json!({ "ok": true, "id": id }))
        }
        "bounds" => {
            engine::set_bounds(id()?, int("x"), int("y"), int("w").max(1), int("h").max(1));
            Ok(serde_json::json!({ "ok": true }))
        }
        "load" => {
            let url = req.get("url").and_then(|u| u.as_str()).ok_or("url 필요")?;
            engine::load(id()?, url.to_string());
            Ok(serde_json::json!({ "ok": true }))
        }
        "query-reply" => {
            // 페이지 cefQuery 응답(플러그인 JS → 브라우저 라우터 콜백). queryId 로 보관 콜백을 완료한다.
            // success=false 면 onFailure(errorCode, response). keep=true(persistent 스냅샷 push)면 콜백 유지.
            let qid = req.get("queryId").and_then(|v| v.as_i64()).ok_or("queryId 필요")?;
            let success = req.get("success").and_then(|v| v.as_bool()).unwrap_or(true);
            let response = req.get("response").and_then(|v| v.as_str()).unwrap_or("");
            let error_code = req.get("errorCode").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            let keep = req.get("keep").and_then(|v| v.as_bool()).unwrap_or(false);
            engine::query_reply(qid, success, response, error_code, keep);
            Ok(serde_json::json!({ "ok": true }))
        }
        "reload" => {
            let ignore = req.get("ignoreCache").and_then(|v| v.as_bool()).unwrap_or(false);
            engine::reload(id()?, ignore);
            Ok(serde_json::json!({ "ok": true }))
        }
        "eval" => {
            // 페이지 JS 실행 — 응답 {ok, evalId}, 결과는 eval-result 이벤트(비동기). 스펙 §8.
            let js = req.get("js").and_then(|j| j.as_str()).ok_or("js 필요")?;
            let eval_id = engine::request_eval(id()?, js.to_string());
            Ok(serde_json::json!({ "ok": true, "evalId": eval_id }))
        }
        "stop" => {
            engine::stop_load(id()?);
            Ok(serde_json::json!({ "ok": true }))
        }
        "back" => {
            engine::go_back(id()?);
            Ok(serde_json::json!({ "ok": true }))
        }
        "forward" => {
            engine::go_forward(id()?);
            Ok(serde_json::json!({ "ok": true }))
        }
        "hidden" => {
            let hidden = req.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
            engine::set_hidden(id()?, hidden);
            Ok(serde_json::json!({ "ok": true }))
        }
        "focus" => {
            engine::set_focus(id()?);
            Ok(serde_json::json!({ "ok": true }))
        }
        "close" => {
            engine::close(id()?);
            Ok(serde_json::json!({ "ok": true }))
        }
        "stats" => {
            // 살아있는 엔진 child id 목록 — E2E/진단(close 실파괴 검증).
            Ok(serde_json::json!({
                "ok": true,
                "ids": engine::browser_ids(),
                // 소유 정보 — 소비자는 자기 owner 의 id 만 회수 대상으로 삼는다(타인 서피스 회수 금지).
                "surfaces": engine::surfaces_info()
                    .into_iter()
                    .map(|(id, owner, offscreen)| serde_json::json!({ "id": id, "owner": owner, "offscreen": offscreen }))
                    .collect::<Vec<_>>(),
                "dbg": {
                    "closeEnter": engine::DBG_CLOSE_ENTER.load(std::sync::atomic::Ordering::Relaxed),
                    "closeApplied": engine::DBG_CLOSE_APPLIED.load(std::sync::atomic::Ordering::Relaxed),
                    "closeBrowser": engine::DBG_CLOSE_BROWSER.load(std::sync::atomic::Ordering::Relaxed),
                    "closeNotFound": engine::DBG_CLOSE_NOTFOUND.load(std::sync::atomic::Ordering::Relaxed),
                    "reaped": engine::DBG_REAPED.load(std::sync::atomic::Ordering::Relaxed),
                    // offscreen 픽셀 경로 생존 수치 — E2E 가 frames>0 을 단언한다.
                    "framesPresented": crate::presenter::FRAMES_PRESENTED.load(std::sync::atomic::Ordering::Relaxed),
                }
            }))
        }
        "popup-mode" => {
            let as_window = req.get("asWindow").and_then(|v| v.as_bool()).unwrap_or(false);
            engine::set_popup_window(as_window);
            Ok(serde_json::json!({ "ok": true }))
        }
        other => Err(format!("미지 요청 type: {other}")),
    }
}

#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_message(
    req: *const u8,
    len: usize,
    surface: usize,
    reply: *mut SoksakBuf,
) -> i32 {
    std::panic::catch_unwind(|| {
        if req.is_null() {
            reply_into(reply, serde_json::json!({ "error": "빈 요청" }));
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(req, len) };
        let parsed: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                reply_into(reply, serde_json::json!({ "error": format!("요청 JSON 파싱 실패: {e}") }));
                return -1;
            }
        };
        match dispatch(&parsed, surface) {
            Ok(v) => {
                reply_into(reply, v);
                0
            }
            Err(msg) => {
                reply_into(reply, serde_json::json!({ "error": msg }));
                -1
            }
        }
    })
    .unwrap_or(-2)
}

// ── notify: 호스트 사실 통지(fire-and-forget) ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_notify(evt: *const u8, len: usize) {
    let _ = std::panic::catch_unwind(|| {
        if evt.is_null() {
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(evt, len) };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("surface-occluded") => {
                let occluded = v.get("occluded").and_then(|o| o.as_bool()).unwrap_or(false);
                engine::set_overlay(occluded);
            }
            // 파괴 순서 계약(SIDECARS) — 창이 닫히기 전에 그 surface 의 child 를 먼저 닫는다.
            Some("surface-closing") => {
                if let Some(view) = v.get("view").and_then(|x| x.as_u64()) {
                    engine::close_surface(view as usize);
                }
            }
            _ => {}
        }
    });
}

// ── free: message reply 버퍼 반환 ────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn soksak_sidecar_engine_free(buf: SoksakBuf) {
    let _ = std::panic::catch_unwind(|| {
        if !buf.ptr.is_null() {
            unsafe { drop(Vec::from_raw_parts(buf.ptr, buf.len, buf.cap)) };
        }
    });
}
