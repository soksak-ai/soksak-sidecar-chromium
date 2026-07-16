// Chromium 엔진 — 앱 프로세스 안에서 windowed 로 구동해 pane 의 네이티브 child 뷰로 임베드한다
// (set_as_child). 프레임이 JS 를 안 거치므로 네이티브 Chromium 속도. 임베딩 수단은 서드파티 CEF
// (Chromium Embedded Framework) — 그 이름은 이 크레이트 안에만 산다(docs/NAMING.md §2).
//
// 왜 in-process 인가: macOS 는 부모 뷰(NSView)가 프로세스-로컬이라 별도 프로세스의 Chromium 창을 앱
// 창에 붙일 수 없다 → 엔진이 앱 프로세스에서 렌더해야 한다. 그래서 이 사이드카는 engine 모델
// (in-process dylib, docs/SIDECARS.md)이고 코어가 런타임 dlopen 한다(코어 링크 0). 게이트도 env 가
// 아니라 "플러그인이 선언하고 열 때"다 — 로드됨 = 활성.
//
// 메시지펌프(핵심): CEF 는 자기 스레드루프를 안 돈다(external_message_pump=1). 대신 "지금 work 필요"를
// OnScheduleMessagePumpWork(delay) 로 push 한다. 그걸 GCD 로 메인큐 "최상위"에 비재진입 디스패치해서
// do_message_loop_work 를 편다. tao 이벤트 콜백 안에서 직접 do_message_loop_work 를 부르면 NSApp
// 이벤트펌프가 재진입되어 didFinishLaunching 도중 CATransaction display 에서 데드락한다(실측). 이 방식은
// cefclient 의 MainMessageLoopExternalPumpMac(NSTimer) 와 동치 — 폴링 아님, CEF push 기반.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use cef::args::Args;
use cef::rc::*;
use cef::wrapper::message_router::{
    BrowserSideCallback, BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSide,
    MessageRouterBrowserSideHandlerCallbacks, MessageRouterConfig,
};
use cef::*;

use crate::host_emit_json;

// DisplayHandler::on_cursor_change 의 cursor 핸들은 OS별 concrete 타입이고, cef 핸들러 매크로가 파라미터
// attribute(#[cfg])를 못 받으므로 param 에 cfg 를 못 붙인다 — 대신 이 별칭을 OS별로 해소해 매크로엔 단일
// ident 로 넘긴다. mac=*mut u8(바인딩에 CursorHandle 별칭 없음), linux=c_ulong·win=HCURSOR(cef::CursorHandle).
#[cfg(target_os = "macos")]
type CefCursorArg = *mut u8;
#[cfg(not(target_os = "macos"))]
type CefCursorArg = cef::CursorHandle;

// 임베드 대기 요청(플러그인 → 커맨드 → 여기). CEF 조작은 UI(메인) 스레드에서만 하므로 큐잉 후 pump 에서 적용.
// 좌표(x,y,w,h)는 플랫폼 중립 top-left 원점 DIP(points) — 부모 뷰 안에서. macOS 는 apply 시 y-flip.
struct CreateReq {
    id: u32,
    nsview: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    url: String,
    // Some(inspected id) = 이 요청은 새 브라우저가 아니라 inspected 브라우저의 DevTools 를 임베드 child 로
    // 여는 것(URL 무시). show_dev_tools 로 붙이고 핸들은 on_after_created 가 잡는다.
    devtools_of: Option<u32>,
    // DevTools 프론트엔드의 screencast(페이지 미리보기 패널) 켜기 — devtools_of 요청에서만 의미.
    // 프로필이 in-memory 라 DevTools 자체 토글은 세션마다 증발 → 이 플래그(플러그인 설정)가 정본.
    screencast: bool,
    // Some(scale) = offscreen 호스팅 모드(SIDECARS.md §8) — windowless + 공유 텍스처로 렌더하고
    // 모듈 소유 layer 로 present(offscreen.rs). None = windowed(기존 경로, 기본).
    offscreen_scale: Option<f32>,
}
// 기존 브라우저 대상 제어 오퍼레이션(id 로 지정). CEF 조작은 메인 스레드 전용 → 큐잉 후 pump 에서 적용.
enum Op {
    Load { id: u32, url: String },
    Reload { id: u32, ignore_cache: bool },
    Stop { id: u32 },
    Back { id: u32 },
    Forward { id: u32 },
    Bounds { id: u32, x: i32, y: i32, w: i32, h: i32 },
    Hidden { id: u32, hidden: bool },
    Focus { id: u32 },
    Close { id: u32 },
    Overlay { active: bool }, // DOM 오버레이/모달이 브라우저 pane 위에 뜸 → 모든 CEF child 숨김
    // offscreen 입력 포워딩(SIDECARS.md §8) — DOM 셀이 받은 입력을 플러그인이 메시지로 보낸 것.
    // 좌표는 표면-로컬 논리 px(DOM CSS px == CEF DIP).
    Mouse { id: u32, kind: u8, x: i32, y: i32, button: u8, clicks: i32, mods: u32 }, // kind 0=move 1=down 2=up
    Wheel { id: u32, x: i32, y: i32, dx: i32, dy: i32 },
    Key { id: u32, kind: u8, code: i32, ch: u16, mods: u32 }, // kind 0=down 1=up 2=char
    Ime { id: u32, kind: u8, text: String, caret: u32 }, // kind 0=set 1=commit 2=finish 3=cancel
    // 호스트→페이지 JS 실행(스펙 §8 eval) — 결과는 query 브리지로 회수해 eval-result 이벤트로 배달.
    Eval { id: u32, eval_id: u64, js: String },
}
static PENDING: LazyLock<Mutex<Vec<CreateReq>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static OPS: LazyLock<Mutex<Vec<Op>>> = LazyLock::new(|| Mutex::new(Vec::new()));
// on_before_close 가 넣는 "제거 대기" CEF identifier. Browser drop(refcount 해제)은 CEF 콜백
// 안이 아니라 다음 pump(do_work)에서 한다 — 콜백 안에서 drop 하면 파괴 재진입으로 크래시.
static CLOSING: LazyLock<Mutex<Vec<i32>>> = LazyLock::new(|| Mutex::new(Vec::new()));
// close 가 요청된 엔진 id — close_browser 호출 뒤 on_before_close(reap)까지 child 의 native view 가
// 파괴 진행 상태라, 이 구간의 후속 op(hidden/bounds/focus/중복 close)가 죽은 NSView 포인터를 만지면
// SIGSEGV(실측 — set_native_hidden 크래시). 요청 즉시 기록하고 find 기반 op 를 전부 차단한다.
static CLOSE_REQUESTED: LazyLock<Mutex<std::collections::HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

// close 경로 단계 카운터 — stats 응답의 dbg 로 노출(진단: close 유실/미완결을 즉시 관찰).
pub static DBG_CLOSE_ENTER: AtomicU64 = AtomicU64::new(0);
pub static DBG_CLOSE_APPLIED: AtomicU64 = AtomicU64::new(0);
pub static DBG_CLOSE_BROWSER: AtomicU64 = AtomicU64::new(0);
pub static DBG_CLOSE_NOTFOUND: AtomicU64 = AtomicU64::new(0);
pub static DBG_REAPED: AtomicU64 = AtomicU64::new(0);

fn close_requested(id: u32) -> bool {
    CLOSE_REQUESTED.lock().map(|s| s.contains(&id)).unwrap_or(false)
}
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
// 생성된 브라우저: id → Browser. bounds/navigate/close 는 여기서 찾아 적용.
// DevTools child 도 여기 등록된다(일반 탭과 동일 — bounds/hidden/close 경로 재사용).
static BROWSERS: LazyLock<Mutex<Vec<(u32, Browser)>>> = LazyLock::new(|| Mutex::new(Vec::new()));
// id → owner(생성 주체 태그, create.owner). 소유는 엔진(진실의 원천)이 기록한다 — 소비자 로컬
// 장부는 유실·불일치가 가능해 회수(reconcile)의 근거가 될 수 없다(실측: 언데드 서피스 잔존).
// stats.surfaces 로 노출, reap 시 제거. 빈 문자열 = 미태깅(구 소비자).
static OWNERS: LazyLock<Mutex<HashMap<u32, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
// id → 부모 surface(NSView) — 파괴 순서 계약(SIDECARS: 창 dealloc 전 surface-closing 통지)의
// 대상 조회용. 생성 시 기록, reap(제거) 시 정리.
static PARENTS: LazyLock<Mutex<Vec<(u32, usize)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

// DevTools-as-tab 흐름(Chromium 공식 원격 디버깅 표면 — chrome://inspect·IDE 가 쓰는 그 경로):
//   CEF 149 는 DevTools 브라우저의 네이티브-부모 임베드를 양쪽에서 막는다(실측 + 원소스
//   browser_host_create.cc: "Alloy style is not supported for this browser"(DevTools) ×
//   "Chrome style is not supported with native parent on MacOS"). 그래서 창을 만들지 않는다 —
//   initialize 에서 remote_debugging_port 를 열고, DevTools "프론트엔드"(웹앱)를 우리 일반 탭
//   (검증된 alloy child)에 URL 로 띄운다. 분할·이동·닫기 = 일반 탭 그대로.
//   타깃 해소: inspected 브라우저에 CDP Target.getTargetInfo(execute_dev_tools_method) → targetId
//   → http://127.0.0.1:{port}/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/{targetId}
//   → 그 URL 로 일반 CreateReq 재큐잉(엔진 id 는 요청 시 선배정 — 어댑터 label 매핑 유지).
static DEVTOOLS_PORT: AtomicU32 = AtomicU32::new(0);
static DEVTOOLS_MSG_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
struct DevtoolsWait {
    msg_id: i32,  // execute_dev_tools_method 상관 id(응답 매칭 키)
    engine_id: u32,
    nsview: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    screencast: bool, // 프론트엔드 로드 후 강제할 screencast 설정값
    _reg: Option<Registration>, // 관찰자 등록 유지(drop=해지) — 응답 수신까지 살아있어야 한다
}
static DEVTOOLS_WAIT: LazyLock<Mutex<Vec<DevtoolsWait>>> = LazyLock::new(|| Mutex::new(Vec::new()));

// DevTools 프론트엔드 child(engine id) → 원하는 screencast 값. 프론트엔드는 자기 설정을
// localStorage('screencastEnabled')에 두는데 우리 프로필은 in-memory 라 세션마다 증발 —
// LoadHandler(load 완료)가 이 값을 localStorage 에 강제한다(다르면 1회 reload, 같으면 no-op).
static DEVTOOLS_SCREENCAST: LazyLock<Mutex<std::collections::HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

// DevTools 프론트엔드의 screencast 설정을 선주입(로드 "시작" 시 — 앱 모듈이 설정을 읽기 전).
// 키는 'screencast-enabled'(프론트엔드 실소스 실측: createSetting("screencast-enabled",!0) —
// kebab-case, 기본 true). Common.Settings 는 값을 JSON 직렬화로 저장 — boolean 은 'true'/'false'.
// reload 방식은 금지 — 프론트엔드 reload 는 ws 재접속을 깨서 "DevTools is undocked" 백지가 된다(실측).
// 켤 때는 접힌 split(size 0 — 비활성 로드가 기록)도 지워 기본 크기로 계산되게 한다.
fn enforce_devtools_screencast(b: &mut Browser) {
    let Some(engine_id) = engine_id_of(b.identifier()) else { return };
    let want = match DEVTOOLS_SCREENCAST.lock().ok().and_then(|m| m.get(&engine_id).copied()) {
        Some(w) => w,
        None => return,
    };
    let Some(f) = b.main_frame() else { return };
    let w = if want { "true" } else { "false" };
    let js = format!(
        "(function(){{try{{localStorage.setItem('screencast-enabled','{w}');if({want}){{var s=localStorage.getItem('inspector-view.screencast-split-view-state');if(s){{var o=JSON.parse(s);if(o&&o.vertical&&!o.vertical.size)localStorage.removeItem('inspector-view.screencast-split-view-state');}}}}}}catch(e){{}}}})();"
    );
    f.execute_java_script(Some(&CefString::from(js.as_str())), None, 0);
}

// 부모 뷰 좌표계는 macOS 가 하단-좌 원점(비-flip) → top-left DIP 를 NSView y 로 뒤집는다.
// parent_h(부모 뷰 높이, points) - (top + h). 비-macos 는 그대로(top-left).
#[cfg(target_os = "macos")]
pub(crate) fn flip_y(parent_view: *mut c_void, top: i32, h: i32) -> i32 {
    if parent_view.is_null() {
        return top;
    }
    unsafe {
        let v = &*(parent_view as *const objc2::runtime::AnyObject);
        let b: objc2_foundation::NSRect = objc2::msg_send![v, bounds];
        (b.size.height as i32) - (top + h)
    }
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn flip_y(_parent_view: *mut c_void, top: i32, _h: i32) -> i32 {
    top
}

// NSView setFrame(부모 좌표계, 하단-좌). 메인 스레드에서만.
#[cfg(target_os = "macos")]
pub(crate) fn set_view_frame(view: *mut c_void, x: i32, y: i32, w: i32, h: i32) {
    if view.is_null() {
        return;
    }
    unsafe {
        let v = &*(view as *const objc2::runtime::AnyObject);
        let frame = objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(x as f64, y as f64),
            objc2_foundation::NSSize::new(w.max(1) as f64, h.max(1) as f64),
        );
        let _: () = objc2::msg_send![v, setFrame: frame];
    }
}

// id 로 브라우저 조회(clone — refcount 증가, 호출자가 소유).
fn find_browser(id: u32) -> Option<Browser> {
    BROWSERS
        .lock()
        .ok()
        .and_then(|list| list.iter().find(|(bid, _)| *bid == id).map(|(_, b)| b.clone()))
}

// ── 메시지펌프 스케줄링(GCD 메인큐, 비재진입) ──────────────────────────────────────────────
// PUMP_SCHEDULED: GCD 블록이 하나 예약돼 있음(중복 예약 억제). IN_WORK: do_message_loop_work 실행 중
// (런루프 spin 으로 재진입 시 감지). REDO: 실행 중 새 요청이 왔음 → 끝나고 즉시 한 번 더.
static PUMP_SCHEDULED: AtomicBool = AtomicBool::new(false);
static IN_WORK: AtomicBool = AtomicBool::new(false);
static REDO: AtomicBool = AtomicBool::new(false);

// 렌더 틱: external_message_pump 에선 CEF present 가 "활동 중 메시지루프가 계속 도는 것"을 전제한다
// (안 돌면 렌더러 프레임이 합성/present 안 됨 → 흰 화면). 그래서 "보이는 브라우저 && 활동 중"일 때만
// ~60fps 로 do_message_loop_work 를 돌려 present 를 몰아준다. 유휴(정적·로드 완료 후)엔 멈춘다 → CPU 0.
// cefclient 의 external pump 타이머와 동치(꼼수 아님). schedule_pump 는 활동으로 안 침(펌프→CEF 재요청
// →bump 무한 피드백 방지). 활동 = LoadHandler 로딩 + 사용자 op(navigate/bounds/…).
static VISIBLE: LazyLock<Mutex<std::collections::HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
static LOADING: LazyLock<Mutex<std::collections::HashSet<i32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
static TICK_ON: AtomicBool = AtomicBool::new(false);
static OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false); // DOM 모달/오버레이가 pane 위에 떠 있음
static ACTIVE_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
// per-id 활동 창(id → until_ms) — 픽셀을 바꾸는 사건이 있었던 서피스만 invalidate 대상이 된다.
// 전역 창(ACTIVE_UNTIL_MS)은 id 없는 사건(Overlay) 전용. 전역만 쓰면 한 서피스의 활동(예: 로딩이
// 안 끝나는 탭)이 모든 가시 서피스를 60fps 전면 재페인트시킨다(실측 — 정적 활성 탭 상시 60fps).
static ACTIVE_IDS: LazyLock<Mutex<HashMap<u32, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_EVAL_ID: AtomicU64 = AtomicU64::new(1);
static CLOCK: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
const RENDER_TICK_MS: i64 = 16; // ~60fps(활동 중에만)
const ACTIVE_GRACE_MS: u64 = 1500; // 활동 신호 후 이만큼 더 틱(로드 후 지연 페인트 커버)

fn now_ms() -> u64 {
    CLOCK.elapsed().as_millis() as u64
}
fn visible_nonempty() -> bool {
    VISIBLE.lock().map(|s| !s.is_empty()).unwrap_or(false)
}
fn loading_nonempty() -> bool {
    LOADING.lock().map(|s| !s.is_empty()).unwrap_or(false)
}
fn is_active() -> bool {
    loading_nonempty()
        || now_ms() < ACTIVE_UNTIL_MS.load(Ordering::Relaxed)
        || active_ids_nonempty()
}
// 전역 활동(마지막 수단 — id 없는 사건 전용). id 를 아는 곳은 bump_id 를 쓴다.
fn bump_active() {
    ACTIVE_UNTIL_MS.store(now_ms() + ACTIVE_GRACE_MS, Ordering::Relaxed);
    start_render_tick();
}
// 서피스별 활동 — 이 id 만 invalidate 대상에 든다.
fn bump_id(id: u32) {
    if let Ok(mut m) = ACTIVE_IDS.lock() {
        m.insert(id, now_ms() + ACTIVE_GRACE_MS);
    }
    start_render_tick();
}
fn active_ids_nonempty() -> bool {
    let now = now_ms();
    ACTIVE_IDS
        .lock()
        .map(|mut m| {
            m.retain(|_, until| *until > now);
            !m.is_empty()
        })
        .unwrap_or(false)
}
fn note_visible(id: u32, visible: bool) {
    if let Ok(mut s) = VISIBLE.lock() {
        if visible {
            s.insert(id);
        } else {
            s.remove(&id);
        }
    }
}
fn note_gone(id: u32) {
    if let Ok(mut s) = VISIBLE.lock() {
        s.remove(&id);
    }
}
// SOKSAK_SIDECAR_BROWSER_CHROMIUM_NO_TICK=1 → 렌더 틱(busy-pump) 비활성. present 를 GPU vsync + 이벤트 펌프
// (OnScheduleMessagePumpWork)만으로 모는지 실측하기 위한 진단 게이트(사이드카 소유 env — 코어 env 아님).
static NO_TICK: LazyLock<bool> =
    LazyLock::new(|| std::env::var("SOKSAK_SIDECAR_BROWSER_CHROMIUM_NO_TICK").as_deref() == Ok("1"));
fn start_render_tick() {
    if *NO_TICK {
        return;
    }
    if TICK_ON.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe { dispatch_async_f(main_queue(), std::ptr::null_mut(), render_tick) };
}
extern "C" fn render_tick(_ctx: *mut c_void) {
    if !visible_nonempty() || !is_active() {
        TICK_ON.store(false, Ordering::SeqCst);
        return;
    }
    do_work();
    unsafe {
        let when = dispatch_time(DISPATCH_TIME_NOW, RENDER_TICK_MS.saturating_mul(1_000_000));
        dispatch_after_f(when, main_queue(), std::ptr::null_mut(), render_tick);
    }
}

// libdispatch(GCD) 원시 FFI — libSystem 자동 링크. _dispatch_main_q 는 메인 시리얼 큐 심볼.
#[allow(non_upper_case_globals)]
extern "C" {
    static _dispatch_main_q: [u8; 0];
    fn dispatch_async_f(queue: *const c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(when: u64, queue: *const c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta: i64) -> u64;
}
const DISPATCH_TIME_NOW: u64 = 0;

fn main_queue() -> *const c_void {
    core::ptr::addr_of!(_dispatch_main_q) as *const c_void
}

// CEF push(OnScheduleMessagePumpWork) 또는 request_create 가 부른다 — 어느 스레드든 안전. 메인런루프
// 최상위에서 do_work 가 돌도록 GCD 로 디스패치. 이미 예약된 블록이 있으면 합쳐 버린다(스택 방지).
fn schedule_pump(delay_ms: i64) {
    if PUMP_SCHEDULED.swap(true, Ordering::SeqCst) {
        return;
    }
    let q = main_queue();
    unsafe {
        if delay_ms <= 0 {
            dispatch_async_f(q, std::ptr::null_mut(), pump_entry);
        } else {
            let when = dispatch_time(DISPATCH_TIME_NOW, delay_ms.saturating_mul(1_000_000));
            dispatch_after_f(when, q, std::ptr::null_mut(), pump_entry);
        }
    }
}

// GCD 블록 진입(메인 스레드) — 예약 플래그 해제 후 실제 work.
extern "C" fn pump_entry(_ctx: *mut c_void) {
    PUMP_SCHEDULED.store(false, Ordering::SeqCst);
    do_work();
}

// 실제 pump — 대기 임베드 요청 적용 + do_message_loop_work. do_message_loop_work 가 런루프를 spin 하며
// 메인큐 블록을 다시 dequeue 하면 재진입될 수 있다 → IN_WORK 로 감지, 재진입이면 REDO 만 세우고 즉시 반환.
// 바깥 work 가 끝난 뒤 REDO 면 한 번 더 예약(누락된 work 회수).
fn do_work() {
    if IN_WORK.load(Ordering::SeqCst) {
        REDO.store(true, Ordering::SeqCst);
        return;
    }
    IN_WORK.store(true, Ordering::SeqCst);
    apply_pending();
    apply_ops();
    do_message_loop_work();
    // offscreen 프레임 구동 — windowed 는 CEF 가 자기 NSView 로 합성하지만 offscreen 은 on_accelerated_paint
    // 이 "손상 + 펌프"에 걸려야 프레임을 낸다. external_message_pump 만으론 내부 프레임 타이머가 안 도는
    // 실측(로드 완료 후 정지 프레임이 blank 로 남음). active 인 동안만 offscreen 뷰를 invalidate 해 로드/
    // 애니메이션 중 프레임을 확보한다 — active 가 끝나면(정적) 멈춰 idle 0(마지막 프레임은 레이어에 잔존).
    if is_active() {
        invalidate_offscreen();
    }
    reap_closing();
    IN_WORK.store(false, Ordering::SeqCst);
    if REDO.swap(false, Ordering::SeqCst) {
        schedule_pump(0);
    }
}

// active 인 offscreen 브라우저를 invalidate — CEF 가 손상 영역을 다시 그려 on_accelerated_paint 를
// 낸다(변화 없으면 CEF 가 스스로 스킵 — 과잉 렌더 아님). 메인 스레드 전용(do_work 안).
fn invalidate_offscreen() {
    // 활동 중인 서피스만 — (per-id 활동 창) ∪ (자기 cid 가 로딩 중) ∪ (전역 창: Overlay 등).
    // 전면 invalidate 는 CEF 에 강제 손상이라 정적 페이지도 매 틱 재페인트된다(스킵 없음, 실측).
    let now = now_ms();
    let global = now < ACTIVE_UNTIL_MS.load(Ordering::Relaxed);
    let active: std::collections::HashSet<u32> = ACTIVE_IDS
        .lock()
        .map(|m| m.iter().filter(|(_, u)| **u > now).map(|(i, _)| *i).collect())
        .unwrap_or_default();
    let loading: std::collections::HashSet<i32> =
        LOADING.lock().map(|s| s.clone()).unwrap_or_default();
    let pairs: Vec<(u32, i32)> = BROWSERS
        .lock()
        .map(|l| l.iter().map(|(i, b)| (*i, b.identifier())).collect())
        .unwrap_or_default();
    for (id, cid) in pairs {
        if !crate::presenter::is_offscreen(id) {
            continue;
        }
        if !(global || active.contains(&id) || loading.contains(&cid)) {
            continue;
        }
        if let Some(host) = find_browser(id).and_then(|b| b.host()) {
            host.invalidate(PaintElementType::default()); // PET_VIEW
        }
    }
}

// 대기 CreateReq → set_as_child 로 CEF child 브라우저 생성(메인 스레드에서만). y 는 top-left → NSView 로 flip.
fn apply_pending() {
    let reqs: Vec<CreateReq> =
        PENDING.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
    for r in reqs {
        let parent = r.nsview as *mut c_void;

        // DevTools 탭 — 창 생성이 아니라 CDP 로 inspected 의 targetId 를 조회한 뒤, DevTools 프론트엔드
        // URL 을 가진 "일반 CreateReq" 로 재큐잉한다(관찰자 on_dev_tools_method_result 가 수행). 그래서
        // DevTools 도 완전한 일반 탭 — CEF 의 DevTools 창 제약(위 주석)과 무관해진다.
        if let Some(inspected) = r.devtools_of {
            match find_browser(inspected).and_then(|b| b.host()) {
                Some(host) => {
                    let msg_id = DEVTOOLS_MSG_ID.fetch_add(1, Ordering::Relaxed);
                    let mut obs = CefDevToolsObserver::new();
                    let reg = host.add_dev_tools_message_observer(Some(&mut obs));
                    if let Ok(mut q) = DEVTOOLS_WAIT.lock() {
                        q.push(DevtoolsWait {
                            msg_id,
                            engine_id: r.id,
                            nsview: r.nsview,
                            x: r.x,
                            y: r.y,
                            w: r.w,
                            h: r.h,
                            screencast: r.screencast,
                            _reg: reg,
                        });
                    }
                    let method = CefString::from("Target.getTargetInfo");
                    let sent = host.execute_dev_tools_method(msg_id, Some(&method), None);
                    if sent == 0 {
                        eprintln!("[chromium] devtools CDP 전송 실패 (inspected={inspected})");
                        if let Ok(mut q) = DEVTOOLS_WAIT.lock() {
                            q.retain(|e| e.msg_id != msg_id);
                        }
                    } else {
                        eprintln!(
                            "[chromium] devtools 타깃 조회 (id={}, inspected={inspected}, msg={msg_id})",
                            r.id
                        );
                    }
                }
                None => eprintln!("[chromium] devtools 대상 브라우저 없음 (inspected={inspected})"),
            }
            continue;
        }

        // offscreen 모드(SIDECARS.md §8) — windowless + 공유 텍스처. 뷰/레이어는 offscreen.rs 소유,
        // 코어 hitTest 미등록(surface-created 미방출 — DOM 셀이 입력 소유), 입력은 프로토콜 포워딩.
        if let Some(scale) = r.offscreen_scale {
            create_offscreen(&r, scale);
            continue;
        }

        let ns_y = flip_y(parent, r.y, r.h.max(1));
        // set_as_child 는 cef_window_handle_t(OS별: mac NSView*·win HWND·linux XID)를 받는다.
        let wi = WindowInfo::default().set_as_child(
            r.nsview as cef::sys::cef_window_handle_t,
            &Rect { x: r.x, y: ns_y, width: r.w.max(1), height: r.h.max(1) },
        );
        let mut client = CefClient::new();
        let bs = BrowserSettings::default();
        let url = CefString::from(r.url.as_str());
        let browser = browser_host_create_browser_sync(
            Some(&wi),
            Some(&mut client),
            Some(&url),
            Some(&bs),
            None,
            None,
        );
        if let Some(b) = browser {
            // 레이어 정공법 편입: CEF 는 child 를 부모 최상단에 얹는데, 그 자리는 DOM(사이드바/
            // 모달/탭바) 전부를 가린다(실측 — devtools 가 우측 사이드바 침범). 코어의 설계는
            // "child 는 메인(투명) webview 아래 + 셀-hole 로 비침"(플러그인 뷰 transparent 선언) —
            // 형제 스택 맨 아래로 재배치하고, 코어 SURFACES(마우스 hitTest 위임)에 등록을 알린다.
            #[cfg(target_os = "macos")]
            if let Some(host) = b.host() {
                let child = host.window_handle() as *mut c_void;
                if !child.is_null() {
                    unsafe {
                        use objc2::runtime::AnyObject;
                        let v = child as *mut AnyObject;
                        let sup: *mut AnyObject = objc2::msg_send![&*v, superview];
                        if !sup.is_null() {
                            // NSWindowBelow(-1) + relativeTo nil = 형제 맨 아래.
                            let _: () = objc2::msg_send![&*v, removeFromSuperview];
                            let _: () = objc2::msg_send![&*sup, addSubview: &*v, positioned: -1isize, relativeTo: std::ptr::null::<AnyObject>()];
                        }
                    }
                    host_emit_json(&serde_json::json!({
                        "event": "surface-created", "view": child as usize
                    }));
                }
            }
            if let Ok(mut list) = BROWSERS.lock() {
                list.push((r.id, b));
            }
            note_visible(r.id, true); // 생성 시 보임
            bump_id(r.id); // 초기 로드 present 위해 렌더 틱 가동
            if let Ok(mut p) = PARENTS.lock() {
                p.push((r.id, r.nsview));
            }
            eprintln!("[chromium] child browser 생성 OK (id={}, nsview={:#x})", r.id, r.nsview);
        } else {
            eprintln!("[chromium] child browser 생성 실패 (id={})", r.id);
        }
    }
}

// 생성 동기 구간의 view_rect/screen_info 폴백 — browser_host_create_browser_sync 가 도는 동안
// CEF 가 뷰포트/DPI 를 물어오는데 identifier→엔진 id 매핑이 아직 없다. apply_pending 은 메인 스레드
// 직렬이라 스태시 하나로 충분하다(생성 완료 즉시 해제).
static OSR_CREATING: Mutex<Option<(i32, i32, f32)>> = Mutex::new(None);

// offscreen 생성(메인 스레드) — 프레젠터 뷰를 먼저 만들고 windowless+공유 텍스처 브라우저를 붙인다.
// 실패 시 프레젠터를 되감는다(고아 뷰 금지).
fn create_offscreen(r: &CreateReq, scale: f32) {
    let w = r.w.max(1);
    let h = r.h.max(1);
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    crate::presenter::create_surface(r.id, r.nsview, r.x, r.y, w, h, scale);
    if let Ok(mut c) = OSR_CREATING.lock() {
        *c = Some((w, h, scale));
    }
    let mut wi = WindowInfo::default();
    wi.windowless_rendering_enabled = 1;
    // 공유 텍스처(OnAcceleratedPaint · IOSurface) 경로 — CPU on_paint 는 계약 밖(1회 로그 후 드랍).
    wi.shared_texture_enabled = 1;
    // parent_view 는 절대 주지 않는다(null). windowless 에 부모 뷰를 주면 CEF GetWindowHandle 이
    // 그 뷰를 반환하고, do_close/was_hidden 등 window_handle 경로가 "창의 contentView" 를 제거/숨김
    // 처리해 메인 웹뷰가 창에서 분리된다(실측: 캡처 with_webview 가 detached 웹뷰를 만나 앱 사망).
    // DPI/모니터 정보는 RenderHandler::screen_info 가 정본으로 공급한다.
    // parent_view 는 macOS 전용 필드. non-macOS 는 parent_window 가 default 0(부모 없음)로 동일 의도.
    #[cfg(target_os = "macos")]
    {
        wi.parent_view = std::ptr::null_mut();
    }
    wi.bounds = Rect { x: 0, y: 0, width: w, height: h };
    let mut client = CefOsrClient::new();
    let mut bs = BrowserSettings::default();
    bs.windowless_frame_rate = 60;
    let url = CefString::from(r.url.as_str());
    let browser = browser_host_create_browser_sync(
        Some(&wi),
        Some(&mut client),
        Some(&url),
        Some(&bs),
        None,
        None,
    );
    if let Ok(mut c) = OSR_CREATING.lock() {
        *c = None;
    }
    match browser {
        Some(b) => {
            if let Ok(mut list) = BROWSERS.lock() {
                list.push((r.id, b));
            }
            if let Ok(mut p) = PARENTS.lock() {
                p.push((r.id, r.nsview)); // 파괴 순서 계약(surface-closing) 대상에 동일 편입
            }
            note_visible(r.id, true);
            bump_id(r.id);
            eprintln!("[chromium] offscreen browser 생성 OK (id={}, {w}x{h}@{scale})", r.id);
        }
        None => {
            crate::presenter::destroy(r.id);
            eprintln!("[chromium] offscreen browser 생성 실패 (id={})", r.id);
        }
    }
}

// 대기 제어 오퍼레이션 적용(메인 스레드). 대상 브라우저가 없으면 조용히 건너뜀(닫힌 뒤 늦은 op).
fn apply_ops() {
    let ops: Vec<Op> = OPS.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
    for op in ops {
        match op {
            Op::Load { id, url } => {
                if let Some(f) = find_browser(id).and_then(|b| b.main_frame()) {
                    f.load_url(Some(&CefString::from(url.as_str())));
                }
            }
            Op::Reload { id, ignore_cache } => {
                if let Some(b) = find_browser(id) {
                    if ignore_cache {
                        b.reload_ignore_cache();
                    } else {
                        b.reload();
                    }
                }
            }
            Op::Stop { id } => {
                if let Some(b) = find_browser(id) {
                    b.stop_load();
                }
            }
            Op::Back { id } => {
                if let Some(b) = find_browser(id) {
                    if b.can_go_back() == 1 {
                        b.go_back();
                    }
                }
            }
            Op::Forward { id } => {
                if let Some(b) = find_browser(id) {
                    if b.can_go_forward() == 1 {
                        b.go_forward();
                    }
                }
            }
            Op::Bounds { id, x, y, w, h } => {
                if close_requested(id) {
                    // 파괴 진행 중 — native view 접근 금지(use-after-free 방지).
                } else if crate::presenter::is_offscreen(id) {
                    // offscreen — 프레젠터 뷰 프레임 갱신 후 뷰포트 재보고(view_rect 가 새 크기를 답한다).
                    crate::presenter::set_bounds(id, x, y, w, h.max(1));
                    if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                        host.was_resized();
                    }
                } else if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                    let view = host.window_handle() as *mut c_void;
                    #[cfg(target_os = "macos")]
                    {
                        // 자식 뷰의 superview(부모) 높이로 flip.
                        let parent = unsafe {
                            if view.is_null() {
                                std::ptr::null_mut()
                            } else {
                                let v = &*(view as *const objc2::runtime::AnyObject);
                                let sv: *mut objc2::runtime::AnyObject = objc2::msg_send![v, superview];
                                sv as *mut c_void
                            }
                        };
                        let ns_y = flip_y(parent, y, h.max(1));
                        set_view_frame(view, x, ns_y, w, h);
                        host.was_resized();
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = (view, x, y, w, h);
                        host.was_resized();
                    }
                }
            }
            Op::Eval { id, eval_id, js } => {
                let Some(frame) = find_browser(id).and_then(|b| b.main_frame()) else {
                    host_emit_json(&serde_json::json!({
                        "event": "eval-result", "id": id, "evalId": eval_id,
                        "ok": false, "value": "브라우저/프레임 없음",
                    }));
                    continue;
                };
                // 사용자 JS 를 async 함수 본문으로 인라인(eval() 미사용 — 페이지 CSP unsafe-eval 회피).
                // 결과는 JSON 직렬화 가능해야 한다(native browser.eval 과 동일 계약). 회수는 페이지의
                // window.cefQuery(__soksakEval 마커) → on_query_str 인터셉트 → eval-result 이벤트.
                let wrapper = format!(
                    "(async()=>{{let __ok=true,__v;try{{__v=await(async()=>{{ {js} }})();}}catch(e){{__ok=false;__v=String((e&&e.stack)||e);}}let __s;try{{__s=JSON.stringify(__v===undefined?null:__v);}}catch(_){{__s=JSON.stringify(String(__v));}}try{{window.cefQuery({{request:JSON.stringify({{__soksakEval:{eval_id},ok:__ok,value:JSON.parse(__s)}}),onSuccess:function(){{}},onFailure:function(){{}}}});}}catch(_){{}}}})();"
                );
                frame.execute_java_script(Some(&CefString::from(wrapper.as_str())), None, 0);
            }
            Op::Hidden { id, hidden } => {
                if close_requested(id) {
                    continue; // 파괴 진행 중 — native view 접근 금지(use-after-free 방지).
                }
                note_visible(id, !hidden);
                if !hidden {
                    bump_id(id); // 다시 보일 때 present 위해 틱 가동
                }
                // child 는 메인 webview 아래 — 오버레이는 자연히 위에 그려지므로 탭 숨김만 반영.
                if crate::presenter::is_offscreen(id) {
                    crate::presenter::set_hidden(id, hidden);
                    if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                        host.was_hidden(if hidden { 1 } else { 0 });
                        if !hidden {
                            // 숨김 중 프레임을 버렸으므로 전면 재페인트 강제.
                            host.invalidate(PaintElementType::default());
                        }
                    }
                } else {
                    set_native_hidden(id, hidden);
                }
            }
            Op::Focus { id } => {
                if !close_requested(id) {
                    if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                        host.set_focus(1);
                    }
                }
            }
            Op::Close { id } => {
                // 이미 파괴 진행 중이면 전부 건너뛴다 — 중복 close 의 set_native_hidden 이 파괴 중
                // child 의 view 를 만져 SIGSEGV(실측). 요청 기록이 후속 op 도 차단한다(위 가드들).
                DBG_CLOSE_APPLIED.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut req) = CLOSE_REQUESTED.lock() {
                    if !req.insert(id) {
                        continue;
                    }
                }
                note_gone(id);
                if let Ok(mut m) = DEVTOOLS_SCREENCAST.lock() {
                    m.remove(&id);
                }
                // 즉시 숨김 — close_browser 는 비동기(렌더러 왕복, DevTools 프론트엔드는 수 초)라
                // do_close 의 removeFromSuperview 까지 child 가 화면에 잔존한다(실측 ~2s). 탭은 이미
                // 닫힌 확정 상태이므로 서피스는 지금 사라져야 한다(파괴 완결은 그대로 do_close).
                set_native_hidden(id, true); // windowless 는 window_handle=null 가드로 무해
                crate::presenter::set_hidden(id, true); // offscreen 프레젠터 뷰 즉시 숨김(windowed 는 no-op)
                // force(1) — 수명은 호스트(워크스페이스) 소유: 뷰 닫기는 이미 확정된 결정이다.
                // non-force(0)는 unload/beforeunload 에 막혀 조용히 중단될 수 있고(DevTools 프론트엔드
                // 실측 — 탭은 사라지는데 child 가 유령으로 잔존), 우리는 JS dialog UI 도 없어 그 중단을
                // 풀 수단이 없다. BROWSERS 제거·drop 은 on_before_close→reap_closing(다음 pump)이 한다.
                match find_browser(id).and_then(|b| b.host()) {
                    Some(host) => {
                        host.close_browser(1);
                        DBG_CLOSE_BROWSER.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        DBG_CLOSE_NOTFOUND.fetch_add(1, Ordering::Relaxed);
                    }
                }
                eprintln!("[chromium] close 요청 (id={id}, force)");
            }
            Op::Overlay { active } => {
                // child 는 메인(투명) webview "아래"(생성 시 재배치) — DOM 오버레이/모달은 자연히 child
                // 위에 그려지고, 마우스는 코어 hitTest 오버레이 게이트가 DOM 에 준다. 그래서 숨김 불요
                // (구현 초기엔 child 가 맨 위라 전체 숨김으로 회피했었다 — 부분 오버레이에도 브라우저가
                // 전부 꺼지는 과잉). 상태만 기록.
                OVERLAY_ACTIVE.store(active, Ordering::Relaxed);
                if !active {
                    bump_active(); // 오버레이 닫힘 → present 재개
                }
            }
            // ── offscreen 입력 포워딩 적용(SIDECARS.md §8) — CEF 입력 API 는 UI(메인) 스레드 전용 ──
            Op::Mouse { id, kind, x, y, button, clicks, mods } => {
                if close_requested(id) {
                    continue;
                }
                if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                    if kind == 1 {
                        pressed_set(id, button, true);
                    }
                    let ev = MouseEvent { x, y, modifiers: event_flags(mods) | pressed_flags(id) };
                    match kind {
                        0 => host.send_mouse_move_event(Some(&ev), 0),
                        1 | 2 => {
                            let bt: MouseButtonType = match button {
                                1 => sys::cef_mouse_button_type_t::MBT_MIDDLE,
                                2 => sys::cef_mouse_button_type_t::MBT_RIGHT,
                                _ => sys::cef_mouse_button_type_t::MBT_LEFT,
                            }
                            .into();
                            host.send_mouse_click_event(
                                Some(&ev),
                                bt,
                                if kind == 2 { 1 } else { 0 },
                                clicks.max(1),
                            );
                            if kind == 2 {
                                pressed_set(id, button, false);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Op::Wheel { id, x, y, dx, dy } => {
                if close_requested(id) {
                    continue;
                }
                if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                    let ev = MouseEvent { x, y, modifiers: pressed_flags(id) };
                    // DOM wheel 델타(+아래/+오른쪽) → CEF 스크롤 델타는 반전 — 폐기본에서 검증된 부호.
                    host.send_mouse_wheel_event(Some(&ev), -dx, -dy);
                }
            }
            Op::Key { id, kind, code, ch, mods } => {
                if close_requested(id) {
                    continue;
                }
                if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                    let type_: KeyEventType = match kind {
                        1 => sys::cef_key_event_type_t::KEYEVENT_KEYUP,
                        2 => sys::cef_key_event_type_t::KEYEVENT_CHAR,
                        _ => sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN,
                    }
                    .into();
                    let ev = KeyEvent {
                        type_,
                        modifiers: event_flags(mods),
                        windows_key_code: code,
                        native_key_code: 0,
                        is_system_key: 0,
                        character: ch,
                        unmodified_character: ch,
                        focus_on_editable_field: 0,
                        ..Default::default()
                    };
                    host.send_key_event(Some(&ev));
                }
            }
            Op::Ime { id, kind, text, caret } => {
                if close_requested(id) {
                    continue;
                }
                if let Some(host) = find_browser(id).and_then(|b| b.host()) {
                    // 범위 단위 = UTF-16 코드 유닛(JS 문자열 인덱스와 동일 — 플러그인 caret 그대로 사용).
                    let invalid = Range { from: u32::MAX, to: u32::MAX };
                    match kind {
                        0 => {
                            let len = text.encode_utf16().count() as u32;
                            let t = CefString::from(text.as_str());
                            let underline = CompositionUnderline {
                                range: Range { from: 0, to: len },
                                color: 0xFF00_0000, // 불투명 검정 — Chromium 기본 조합 밑줄 관행
                                background_color: 0,
                                thick: 0,
                                ..Default::default()
                            };
                            let caret = caret.min(len);
                            let sel = Range { from: caret, to: caret };
                            host.ime_set_composition(
                                Some(&t),
                                Some(&[underline]),
                                Some(&invalid),
                                Some(&sel),
                            );
                        }
                        1 => {
                            let t = CefString::from(text.as_str());
                            host.ime_commit_text(Some(&t), Some(&invalid), 0);
                        }
                        2 => host.ime_finish_composing_text(0),
                        _ => host.ime_cancel_composition(),
                    }
                }
            }
        }
    }
}

// 프로토콜 mods(1=shift 2=ctrl 4=alt 8=meta) → CEF EventFlags 비트.
fn event_flags(mods: u32) -> u32 {
    let mut f = 0u32;
    if mods & 1 != 0 {
        f |= 1 << 1; // SHIFT
    }
    if mods & 2 != 0 {
        f |= 1 << 2; // CONTROL
    }
    if mods & 4 != 0 {
        f |= 1 << 3; // ALT
    }
    if mods & 8 != 0 {
        f |= 1 << 7; // COMMAND
    }
    f
}

// offscreen 드래그 추적 — 프로토콜 mods 는 키보드 전용이라, 페이지 내 드래그가 성립하려면 move 이벤트에
// 눌린 마우스 버튼 플래그를 엔진이 실어야 한다. down/up 으로 기록하고 reap 시 정리.
static PRESSED: LazyLock<Mutex<HashMap<u32, u8>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
fn pressed_set(id: u32, button: u8, down: bool) {
    if let Ok(mut m) = PRESSED.lock() {
        let bits = m.entry(id).or_insert(0);
        let bit = 1u8 << button.min(2);
        if down {
            *bits |= bit;
        } else {
            *bits &= !bit;
        }
    }
}
fn pressed_flags(id: u32) -> u32 {
    let bits = PRESSED.lock().ok().and_then(|m| m.get(&id).copied()).unwrap_or(0);
    let mut f = 0u32;
    if bits & 1 != 0 {
        f |= 1 << 4; // LEFT_MOUSE_BUTTON
    }
    if bits & 2 != 0 {
        f |= 1 << 5; // MIDDLE_MOUSE_BUTTON
    }
    if bits & 4 != 0 {
        f |= 1 << 6; // RIGHT_MOUSE_BUTTON
    }
    f
}

// CEF child 네이티브 뷰 숨김/표시(was_hidden + NSView setHidden). 메인 스레드에서만.
fn set_native_hidden(id: u32, hidden: bool) {
    if let Some(host) = find_browser(id).and_then(|b| b.host()) {
        host.was_hidden(if hidden { 1 } else { 0 });
        #[cfg(target_os = "macos")]
        unsafe {
            let view = host.window_handle() as *mut c_void;
            if !view.is_null() {
                let v = &*(view as *const objc2::runtime::AnyObject);
                let _: () = objc2::msg_send![v, setHidden: hidden];
            }
        }
    }
}

// 살아있는 서피스의 소유 정보 — stats.surfaces. (id, owner, offscreen).
pub fn surfaces_info() -> Vec<(u32, String, bool)> {
    let ids: Vec<u32> = BROWSERS
        .lock()
        .map(|l| l.iter().map(|(i, _)| *i).collect())
        .unwrap_or_default();
    let owners = OWNERS.lock().map(|o| o.clone()).unwrap_or_default();
    ids.into_iter()
        .map(|id| {
            (
                id,
                owners.get(&id).cloned().unwrap_or_default(),
                crate::presenter::is_offscreen(id),
            )
        })
        .collect()
}

// on_before_close 가 기록한 닫힌 브라우저를 BROWSERS 에서 제거(=Rust Browser drop). CEF 콜백
// 밖(do_message_loop_work 이후)에서 실행돼 파괴 재진입을 피한다.
fn reap_closing() {
    let ids: Vec<i32> = CLOSING.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
    if ids.is_empty() {
        return;
    }
    if let Ok(mut list) = BROWSERS.lock() {
        // PARENTS 도 함께 정리 — 제거되는 엔진 id 를 먼저 수집(대장 동기).
        let gone: Vec<u32> = list
            .iter()
            .filter(|(_, br)| ids.contains(&br.identifier()))
            .map(|(i, _)| *i)
            .collect();
        list.retain(|(_, br)| !ids.contains(&br.identifier()));
        if let Ok(mut p) = PARENTS.lock() {
            p.retain(|(i, _)| !gone.contains(i));
        }
        // 파괴 완결 — close 요청 기록도 회수(집합 무한 성장 방지).
        DBG_REAPED.fetch_add(gone.len() as u64, Ordering::Relaxed);
        if let Ok(mut req) = CLOSE_REQUESTED.lock() {
            for i in &gone {
                req.remove(i);
            }
        }
        if let Ok(mut o) = OWNERS.lock() {
            for i in &gone {
                o.remove(i);
            }
        }
        for i in &gone {
            crate::presenter::destroy(*i); // offscreen 프레젠터 회수(windowed 는 no-op)
            if let Ok(mut m) = PRESSED.lock() {
                m.remove(i);
            }
        }
    }
}

// 제어 op 큐잉 + 즉시 pump 예약(메인 스레드에서 apply_ops 가 실제 적용). 어느 스레드든 안전.
fn request_op(op: Op) {
    // 활동은 픽셀을 바꿀 그 서피스에만 귀속(per-id) — 전역 bump 는 모든 가시 서피스를 재페인트시킨다.
    match &op {
        Op::Load { id, .. }
        | Op::Reload { id, .. }
        | Op::Stop { id }
        | Op::Back { id }
        | Op::Forward { id }
        | Op::Bounds { id, .. }
        | Op::Hidden { id, .. }
        | Op::Focus { id }
        | Op::Mouse { id, .. }
        | Op::Wheel { id, .. }
        | Op::Key { id, .. }
        | Op::Ime { id, .. }
        | Op::Eval { id, .. } => bump_id(*id),
        Op::Close { .. } => {}
        Op::Overlay { .. } => bump_active(), // 전체 숨김/복귀 — id 없음
    }
    if let Ok(mut q) = OPS.lock() {
        q.push(op);
    }
    schedule_pump(0);
}

pub fn load(id: u32, url: String) {
    request_op(Op::Load { id, url });
}
pub fn reload(id: u32, ignore_cache: bool) {
    request_op(Op::Reload { id, ignore_cache });
}
pub fn stop_load(id: u32) {
    request_op(Op::Stop { id });
}
pub fn go_back(id: u32) {
    request_op(Op::Back { id });
}
pub fn go_forward(id: u32) {
    request_op(Op::Forward { id });
}
pub fn set_bounds(id: u32, x: i32, y: i32, w: i32, h: i32) {
    request_op(Op::Bounds { id, x, y, w, h });
}
pub fn set_hidden(id: u32, hidden: bool) {
    request_op(Op::Hidden { id, hidden });
}
pub fn set_focus(id: u32) {
    request_op(Op::Focus { id });
}
// DOM 오버레이/모달 게이트 — 코어 webview_overlay_active 가 창 전이 시 호출. 활성이면 모든 CEF child 숨김.
pub fn set_overlay(active: bool) {
    request_op(Op::Overlay { active });
}
pub fn close(id: u32) {
    DBG_CLOSE_ENTER.fetch_add(1, Ordering::Relaxed);
    request_op(Op::Close { id });
}

// ── offscreen 입력 포워딩 진입(lib.rs dispatch → 큐잉, 메인 스레드에서 apply_ops 가 적용) ─────
pub fn mouse(id: u32, kind: u8, x: i32, y: i32, button: u8, clicks: i32, mods: u32) {
    request_op(Op::Mouse { id, kind, x, y, button, clicks, mods });
}
pub fn wheel(id: u32, x: i32, y: i32, dx: i32, dy: i32) {
    request_op(Op::Wheel { id, x, y, dx, dy });
}
pub fn key(id: u32, kind: u8, code: i32, ch: u16, mods: u32) {
    request_op(Op::Key { id, kind, code, ch, mods });
}
pub fn ime(id: u32, kind: u8, text: String, caret: u32) {
    request_op(Op::Ime { id, kind, text, caret });
}

// 파괴 순서 계약(SIDECARS 호스트 통지 "surface-closing") — 그 surface(NSView)에 부모 지정된
// child 전부를 닫는다. 창 dealloc 이 살아있는 엔진 뷰 위에서 진행되지 않게 하는 수명주기 규칙.
pub fn close_surface(nsview: usize) {
    let ids: Vec<u32> = PARENTS
        .lock()
        .map(|p| p.iter().filter(|(_, v)| *v == nsview).map(|(i, _)| *i).collect())
        .unwrap_or_default();
    if !ids.is_empty() {
        eprintln!("[chromium] surface-closing({nsview:#x}) → child {ids:?} 닫기");
    }
    for id in ids {
        request_op(Op::Close { id });
    }
}

// disable-gpu 로 GPU 프로세스 서명 이슈 회피(ad-hoc 서명 dev). 정식 서명 시 재검토.
// browser_process_handler 를 노출해 CEF 의 메시지펌프 스케줄 콜백을 받는다(external_message_pump 핵심).
wrap_app! {
    struct CefApp {}
    impl App {
        fn on_before_command_line_processing(
            &self,
            _pt: Option<&CefString>,
            cmd: Option<&mut CommandLine>,
        ) {
            if let Some(c) = cmd {
                // 풀 GPU — disable-gpu 를 켜지 않는다(브라우저는 GPU 가속이 정상, CPU 렌더는 타협).
                // GPU 프로세스 서명은 ad-hoc(arm64 링커 자동) 로 통과. 키체인은 in-memory 로 회피.
                c.append_switch(Some(&CefString::from("use-mock-keychain")));
                // 팝업 차단 해제 — target=_blank/window.open 을 막지 않고 on_before_popup 으로 설정대로
                // 라우팅(새 탭/새 창). 차단이 아니라 라우팅이 브라우저의 올바른 동작.
                c.append_switch(Some(&CefString::from("disable-popup-blocking")));
                // DevTools 프론트엔드 탭(http://127.0.0.1:{port}/devtools/…)의 WebSocket 연결 허용 —
                // Chromium 은 디버깅 WS 의 Origin 을 검사한다. 그 정확한 origin 만 허용(와일드카드 금지).
                let port = DEVTOOLS_PORT.load(Ordering::Relaxed);
                if port != 0 {
                    c.append_switch_with_value(
                        Some(&CefString::from("remote-allow-origins")),
                        Some(&CefString::from(format!("http://127.0.0.1:{port}").as_str())),
                    );
                }
            }
        }
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(CefBrowserProcessHandler::new())
        }
    }
}

// CEF 가 "지금(또는 delay 후) do_message_loop_work 필요" 를 push 하는 콜백. 어느 스레드든 호출될 수 있다.
wrap_browser_process_handler! {
    struct CefBrowserProcessHandler {}
    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            schedule_pump(delay_ms);
        }
    }
}

// LifeSpanHandler — close 시퀀스의 정석. close_browser 후 CEF 가 do_close → on_before_close 를
// 부른다. on_before_close 에서야 Browser 를 놓는 게 안전하다(그 전에 동기 drop 하면 파괴 중
// use-after-free 로 크래시 — 실측). BROWSERS 제거를 여기서만 한다.
// 새 링크(target=_blank/window.open) 열기 정책 — 플러그인 browserNewWindow 설정 반영. true=새 창(엔진
// 네이티브 팝업), false=새 탭(팝업 취소 + URL 을 host.emit 으로 채널에 배달 → 플러그인이 인앱 새 탭).
// 전역(플러그인 설정이 전역이라 브라우저별 아님).
static POPUP_AS_WINDOW: AtomicBool = AtomicBool::new(false);

pub fn set_popup_window(as_window: bool) {
    POPUP_AS_WINDOW.store(as_window, Ordering::Relaxed);
}

// CEF identifier → 엔진-로컬 id (popup-url 이벤트에 소스 브라우저를 실어 멀티창 어댑터가 자기 것만
// 소비하게 — 구 전역 emit 의 중복 수신 구조 교정).
fn engine_id_of(cef_identifier: i32) -> Option<u32> {
    BROWSERS.lock().ok().and_then(|list| {
        list.iter()
            .find(|(_, b)| b.identifier() == cef_identifier)
            .map(|(id, _)| *id)
    })
}

// ── 페이지↔호스트 메시지 라우터(CefMessageRouter) ────────────────────────────────
// 임베드 앱(디자인 캔버스 등)이 window.cefQuery({request}) 로 구조적 데이터(JSON, 코드 아님)를 보내면
// on_query_str 이 그걸 host_emit_json(event:"query") 로 플러그인 JS 에 넘기고, 콜백을 query_id 로 보관한다.
// 플러그인 JS 는 명령 실행/구독 처리 후 {type:"query-reply", queryId, ...} 를 사이드카로 되보내고 —
// engine::query_reply 가 보관된 콜백을 success_str/failure 로 완료한다. persistent 쿼리는 콜백을 남겨
// 반복 push(호스트→페이지 스냅샷 채널) — eval 없는 양방향. 사이드카는 request/response 를 불투명하게
// 릴레이만 하고 execute/subscribe 의미는 플러그인 JS 가 해석한다(코어/사이드카 락인 없음).
static BROWSER_ROUTER: OnceLock<Arc<BrowserSideRouter>> = OnceLock::new();
static QUERY_CALLBACKS: LazyLock<Mutex<HashMap<i64, Arc<Mutex<dyn BrowserSideCallback>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct HostQueryHandler;

impl BrowserSideHandler for HostQueryHandler {
    fn on_query_str(
        &self,
        browser: Option<Browser>,
        _frame: Option<Frame>,
        query_id: i64,
        request: &str,
        persistent: bool,
        callback: Arc<Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        let id = browser.map(|b| b.identifier()).and_then(engine_id_of);
        // eval 회수 채널 — 페이지 코드가 아니라 우리 eval 래퍼가 보낸 것(__soksakEval 마커).
        // 일반 query 로 흘리지 않고 eval-result 이벤트로 완결한다(plugin JS 의 query 의미와 분리).
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(request) {
            if let Some(eval_id) = v.get("__soksakEval").and_then(|x| x.as_u64()) {
                host_emit_json(&serde_json::json!({
                    "event": "eval-result", "id": id, "evalId": eval_id,
                    "ok": v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
                    "value": v.get("value").cloned().unwrap_or(serde_json::Value::Null),
                }));
                if let Ok(cb) = callback.lock() {
                    cb.success_str("");
                }
                return true;
            }
        }
        if let Ok(mut m) = QUERY_CALLBACKS.lock() {
            m.insert(query_id, callback);
        }
        // 플러그인 JS 로 전달 — 응답은 query_reply(비동기)로 완료된다. request 는 불투명 JSON 문자열.
        host_emit_json(&serde_json::json!({
            "event": "query",
            "id": id,
            "queryId": query_id,
            "request": request,
            "persistent": persistent,
        }));
        true // 쿼리 claim — 콜백이 비동기로 완료됨을 약속
    }
    fn on_query_canceled(&self, _browser: Option<Browser>, _frame: Option<Frame>, query_id: i64) {
        // 브라우저 파괴·네비게이션·JS cancel — 보관 콜백을 제거하고(이후 호출 금지) 플러그인에 통지.
        if let Ok(mut m) = QUERY_CALLBACKS.lock() {
            m.remove(&query_id);
        }
        host_emit_json(&serde_json::json!({ "event": "query-canceled", "queryId": query_id }));
    }
}

// initialize() 성공 후 UI 스레드에서 1회 설치(add_handler 는 UI 스레드 assert). 브라우저-사이드 config 는
// helper 의 렌더-사이드와 동일(기본 cefQuery/cefQueryCancel).
fn install_message_router() {
    let router = <BrowserSideRouter as MessageRouterBrowserSide>::new(MessageRouterConfig::default());
    router.add_handler(Arc::new(HostQueryHandler), /*first=*/ false);
    let _ = BROWSER_ROUTER.set(router);
}

// 플러그인 JS 의 {type:"query-reply"} 를 받아 보관 콜백을 완료한다(lib.rs dispatch 경유). success 는
// onSuccess(response), 실패는 onFailure(error_code, response). keep=true(persistent 스냅샷 push)면 콜백을
// 남겨 다음 push 를 허용, 아니면 제거한다. 콜백 메서드는 임의 브라우저 스레드에서 호출 가능(라우터가
// 내부적으로 UI 스레드로 post_task) — 사이드카 메시지 스레드에서 불려도 안전.
pub fn query_reply(query_id: i64, success: bool, response: &str, error_code: i32, keep: bool) {
    let cb = {
        let Ok(mut m) = QUERY_CALLBACKS.lock() else { return };
        if keep {
            m.get(&query_id).cloned()
        } else {
            m.remove(&query_id)
        }
    };
    if let Some(cb) = cb {
        if let Ok(cb) = cb.lock() {
            if success {
                cb.success_str(response);
            } else {
                cb.failure(error_code, response);
            }
        }
    }
}

wrap_life_span_handler! {
    struct CefLifeSpanHandler {}
    impl LifeSpanHandler {
        // 새 링크 열기 — 설정 반영(꼼수 아님, 코어가 CEF 팝업을 설정대로 라우팅).
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_js_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(|u| u.to_string()).unwrap_or_default();
            // DevTools 는 CEF 내부 팝업(target_url=devtools://…)이다. 이걸 우리 탭/창 라우팅으로
            // 가로채면(취소=1) show_dev_tools 가 이미 착수한 창 생성 경로와 충돌해 macOS objc
            // 예외로 앱이 죽는다(실측). DevTools 는 항상 CEF 네이티브 창으로 허용한다(0).
            if url.starts_with("devtools://") {
                eprintln!("[chromium] on_before_popup → DevTools 네이티브 창 허용");
                return 0;
            }
            if POPUP_AS_WINDOW.load(Ordering::Relaxed) {
                eprintln!("[chromium] on_before_popup → 새 창(네이티브) url={url}");
                return 0; // 새 창 = 엔진 네이티브 팝업 허용
            }
            // 새 탭 = 팝업 취소 + URL 을 host.emit 으로 채널에 배달(플러그인이 인앱 새 탭으로 연다).
            // 소스 브라우저 id 동반 — 어댑터가 자기 소유 id 만 소비(멀티창 중복 수신 구조 교정).
            let src = browser.map(|b| b.identifier()).and_then(engine_id_of);
            host_emit_json(&serde_json::json!({
                "event": "popup-url",
                "url": url,
                "id": src,
            }));
            eprintln!("[chromium] on_before_popup → 새 탭(emit popup-url) url={url}");
            1 // cancel
        }
        fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
            // 우리 임베드 child(set_as_child, BROWSERS 등록)만 1 = 앱이 close 를 소유(0 이면 CEF 가
            // Tauri 소유 호스트 NSWindow 를 닫으려다 행 — 실측). 1 을 반환하면 CEF 는 "앱이 네이티브
            // 뷰를 파괴"하길 기다린다(cefclient 는 이 지점에서 자기 NSWindow 를 close) — 그래서 여기서
            // child NSView 를 부모에서 제거해 파괴를 완결시킨다. 안 하면 브라우저가 영원히 산 채로
            // 남는 유령 child 가 된다(실측: 탭은 닫혔는데 서피스 잔존·stats 누적). CEF 소유 독립
            // 창(비등록)은 0(CEF 정상 닫기).
            let Some(b) = browser else { return 0 };
            let cid = b.identifier();
            let engine_id = BROWSERS
                .lock()
                .ok()
                .and_then(|l| l.iter().find(|(_, br)| br.identifier() == cid).map(|(i, _)| *i));
            let Some(eid) = engine_id else {
                // 비등록 = CEF 소유 독립 창(window 모드 팝업 등) — CEF 정상 닫기.
                eprintln!("[chromium] do_close → CEF 소유 창 정상 닫기 (cef_id={cid})");
                return 0;
            };
            // offscreen(windowless)은 OS 창도, 파괴할 CEF 네이티브 뷰도 없다(parent_view=null). windowed
            // 처럼 1 을 돌려 "앱이 네이티브 뷰를 파괴"하길 기다리게 하면 그 파괴가 영영 안 와 on_before_close
            // 가 안 나고 브라우저가 산 채 남는다(실측: closeBrowser=n, reaped=0 — 탭을 닫아도 프레젠터가
            // 화면에 잔존). 0 을 돌려 CEF 정상 close 시퀀스를 태운다 — 프레젠터 NSView 회수는
            // on_before_close→reap_closing 의 offscreen::destroy 가 한다.
            if crate::presenter::is_offscreen(eid) {
                eprintln!("[chromium] do_close → offscreen(windowless) 정상 닫기 (eid={eid}, cef_id={cid})");
                return 0;
            }
            #[cfg(target_os = "macos")]
            if let Some(host) = b.host() {
                let view = host.window_handle() as *mut c_void;
                if !view.is_null() {
                    // 코어 SURFACES(hitTest 위임) 등록 해제 — 등록과 대칭.
                    host_emit_json(&serde_json::json!({
                        "event": "surface-destroyed", "view": view as usize
                    }));
                    unsafe {
                        let v = &*(view as *const objc2::runtime::AnyObject);
                        let _: () = objc2::msg_send![v, removeFromSuperview];
                    }
                }
            }
            eprintln!("[chromium] do_close → 임베드 child 뷰 제거(파괴 완결) (cef_id={cid})");
            1
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            // 콜백 안에서 Browser 를 drop 하지 않는다(파괴 재진입 크래시). identifier 만 기록하고
            // 실제 제거/drop 은 다음 pump(reap_closing)로 미룬다.
            if let Some(b) = browser {
                // 메시지 라우터에 브라우저 파괴 통지 — 이 브라우저의 대기 쿼리를 취소(on_query_canceled
                // 발화 → 보관 콜백 정리). 라우터 계약(OnBeforeClose 에서 반드시 호출).
                if let Some(router) = BROWSER_ROUTER.get() {
                    router.on_before_close(Some(b.clone()));
                }
                let closing = b.identifier();
                if let Ok(mut q) = CLOSING.lock() {
                    q.push(closing);
                }
                schedule_pump(0);
                eprintln!("[chromium] on_before_close (cef_id={closing})");
            }
        }
    }
}

// LoadHandler — 로딩 상태. 로딩 중엔 LOADING 에 넣어 렌더 틱을 확실히 유지(present 가 가장 필요한 구간),
// 완료 시 빼고 grace 를 준다(로드 후 지연 페인트).
wrap_load_handler! {
    struct CefLoadHandler {}
    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            // DevTools 프론트엔드면 screencast 설정을 선주입(메인 프레임, 앱 모듈이 읽기 전).
            if !frame.map(|f| f.is_main() == 1).unwrap_or(false) {
                return;
            }
            if let Some(b) = browser {
                enforce_devtools_screencast(b);
            }
        }
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            if let Some(b) = browser {
                let cid = b.identifier();
                if let Ok(mut s) = LOADING.lock() {
                    if is_loading == 1 { s.insert(cid); } else { s.remove(&cid); }
                }
                // 로딩 상태 + 히스토리 가능 여부를 채널로 배달 — 소비자 UI(스피너/정지 버튼 표시,
                // 뒤로/앞으로 버튼 활성)의 단일 소스. nav/title 이벤트와 동형.
                host_emit_json(&serde_json::json!({
                    "event": "loading", "id": engine_id_of(cid),
                    "loading": is_loading == 1,
                    "canBack": can_go_back == 1, "canForward": can_go_forward == 1,
                }));
                if let Some(eid) = engine_id_of(cid) {
                    bump_id(eid);
                }
            }
        }
    }
}

// DisplayHandler — 주소/제목 변화를 채널 이벤트로 배달(nav/title). URL 바·탭 제목의 단일 소스.
// 메인 프레임만(iframe 주소 변화는 잡음). id = 엔진-로컬(어댑터가 자기 소유만 소비).
wrap_display_handler! {
    struct CefDisplayHandler {}
    impl DisplayHandler {
        fn on_address_change(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if !frame.map(|f| f.is_main() == 1).unwrap_or(false) {
                return;
            }
            let id = browser.map(|b| b.identifier()).and_then(engine_id_of);
            let url = url.map(|u| u.to_string()).unwrap_or_default();
            host_emit_json(&serde_json::json!({ "event": "nav", "id": id, "url": url }));
        }
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let id = browser.map(|b| b.identifier()).and_then(engine_id_of);
            let title = title.map(|t| t.to_string()).unwrap_or_default();
            host_emit_json(&serde_json::json!({ "event": "title", "id": id, "title": title }));
        }
        // 파비콘 URL 변화 — nav/title 과 동형의 콘텐츠 사실. 소비자(탭 아이콘)의 단일 소스.
        // 리스트 소유권: CEF 소유 리스트를 빈 것으로 스왑해 읽는다(Borrowed 라 Drop 무해, 소비 안전).
        fn on_favicon_urlchange(
            &self,
            browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            let id = browser.map(|b| b.identifier()).and_then(engine_id_of);
            let urls: Vec<String> = icon_urls
                .map(|l| std::mem::replace(l, CefStringList::new()).into_iter().collect())
                .unwrap_or_default();
            host_emit_json(&serde_json::json!({
                "event": "favicon", "id": id,
                "url": urls.first().cloned().unwrap_or_default(),
                "urls": urls,
            }));
        }
        // offscreen 커서 미러링(스펙 §8 cursor 이벤트) — windowless 엔 네이티브 커서가 없으므로
        // 플러그인이 DOM 셀의 CSS cursor 로 반영한다. windowed child 는 기본 처리(0) 그대로.
        fn on_cursor_change(
            &self,
            browser: Option<&mut Browser>,
            _cursor: CefCursorArg, // OS별 cursor 핸들 = 모듈 상단 별칭(매크로가 param attribute 불허 → 별칭 해소).
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(id) = browser.map(|b| b.identifier()).and_then(engine_id_of) else {
                return 0;
            };
            if !crate::presenter::is_offscreen(id) {
                return 0;
            }
            host_emit_json(&serde_json::json!({
                "event": "cursor", "id": id, "type": css_cursor(type_)
            }));
            1
        }
    }
}

// CEF 커서 타입 → CSS cursor 값. 매핑 없는 타입은 "default"(플러그인이 그대로 CSS 에 대입).
fn css_cursor(t: CursorType) -> &'static str {
    use sys::cef_cursor_type_t as C;
    let raw: C = t.into();
    match raw {
        C::CT_POINTER => "default",
        C::CT_CROSS => "crosshair",
        C::CT_HAND => "pointer",
        C::CT_IBEAM | C::CT_VERTICALTEXT => "text",
        C::CT_WAIT => "wait",
        C::CT_HELP => "help",
        C::CT_EASTRESIZE | C::CT_WESTRESIZE | C::CT_EASTWESTRESIZE => "ew-resize",
        C::CT_NORTHRESIZE | C::CT_SOUTHRESIZE | C::CT_NORTHSOUTHRESIZE => "ns-resize",
        C::CT_NORTHEASTRESIZE | C::CT_SOUTHWESTRESIZE | C::CT_NORTHEASTSOUTHWESTRESIZE => {
            "nesw-resize"
        }
        C::CT_NORTHWESTRESIZE | C::CT_SOUTHEASTRESIZE | C::CT_NORTHWESTSOUTHEASTRESIZE => {
            "nwse-resize"
        }
        C::CT_COLUMNRESIZE => "col-resize",
        C::CT_ROWRESIZE => "row-resize",
        C::CT_MOVE => "move",
        C::CT_CELL => "cell",
        C::CT_CONTEXTMENU => "context-menu",
        C::CT_ALIAS => "alias",
        C::CT_PROGRESS => "progress",
        _ => "default",
    }
}

// CDP 응답 수신 — Target.getTargetInfo 의 targetId 로 DevTools 프론트엔드 URL 을 만들어 일반
// CreateReq 로 재큐잉한다(엔진 id 는 요청 시 선배정된 값). UI 스레드에서 불린다.
wrap_dev_tools_message_observer! {
    struct CefDevToolsObserver {}
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            let entry = DEVTOOLS_WAIT.lock().ok().and_then(|mut q| {
                q.iter().position(|e| e.msg_id == message_id).map(|i| q.remove(i))
            });
            let Some(e) = entry else { return }; // 다른 CDP 응답(우리 것 아님)
            if success != 1 {
                eprintln!("[chromium] devtools 타깃 조회 실패 (msg={message_id})");
                return;
            }
            let Some(bytes) = result else { return };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                eprintln!("[chromium] devtools 타깃 응답 파싱 실패 (msg={message_id})");
                return;
            };
            let Some(tid) = v.pointer("/targetInfo/targetId").and_then(|t| t.as_str()) else {
                eprintln!("[chromium] devtools 응답에 targetId 없음 (msg={message_id})");
                return;
            };
            let port = DEVTOOLS_PORT.load(Ordering::Relaxed);
            let url = format!(
                "http://127.0.0.1:{port}/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/{tid}"
            );
            eprintln!("[chromium] devtools 프론트엔드 탭 생성 (id={}, target={tid})", e.engine_id);
            // 프론트엔드 로드 완료 시 LoadHandler 가 이 값으로 screencast 설정을 강제한다.
            if let Ok(mut m) = DEVTOOLS_SCREENCAST.lock() {
                m.insert(e.engine_id, e.screencast);
            }
            if let Ok(mut q) = PENDING.lock() {
                q.push(CreateReq {
                    id: e.engine_id,
                    nsview: e.nsview,
                    x: e.x,
                    y: e.y,
                    w: e.w,
                    h: e.h,
                    url,
                    devtools_of: None,
                    screencast: false,
                    offscreen_scale: None, // DevTools 프론트엔드 탭은 windowed 검증 경로 그대로
                });
            }
            schedule_pump(0);
        }
    }
}

wrap_client! {
    struct CefClient {}
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(CefLifeSpanHandler::new())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(CefLoadHandler::new())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(CefDisplayHandler::new())
        }
        // 렌더 프로세스 → 브라우저 프로세스 IPC 를 메시지 라우터로 전달한다(cefQuery 라운드트립의
        // 브라우저 측 수신부). CEF 는 borrowed 인자를 주고 라우터는 owned 를 원해 .clone() 으로 잇는다.
        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            if let Some(router) = BROWSER_ROUTER.get() {
                let handled = router.on_process_message_received(
                    browser.map(|b| b.clone()),
                    frame.map(|f| f.clone()),
                    source_process,
                    message.map(|m| m.clone()),
                );
                return handled as ::std::os::raw::c_int;
            }
            0
        }
    }
}

// ── offscreen 렌더 핸들러 — windowless 브라우저의 뷰포트/DPI 보고 + 공유 텍스처 수신(스펙 §8) ──
// 콜백은 전부 CEF UI 스레드 = 메인 스레드(external_message_pump + multi_threaded_message_loop=0).
wrap_render_handler! {
    struct CefOsrRenderHandler {}
    impl RenderHandler {
        fn view_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let size = browser
                .map(|b| b.identifier())
                .and_then(engine_id_of)
                .and_then(crate::presenter::logical_size)
                .or_else(|| OSR_CREATING.lock().ok().and_then(|c| c.map(|(w, h, _)| (w, h))));
            let (w, h) = size.unwrap_or((1, 1));
            *rect = Rect { x: 0, y: 0, width: w.max(1), height: h.max(1) };
        }
        fn screen_info(
            &self,
            browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(info) = screen_info else { return 0 };
            let id = browser.map(|b| b.identifier()).and_then(engine_id_of);
            let scale = id
                .and_then(crate::presenter::scale_of)
                .or_else(|| OSR_CREATING.lock().ok().and_then(|c| c.map(|(_, _, s)| s)))
                .unwrap_or(1.0);
            let (w, h) = id
                .and_then(crate::presenter::logical_size)
                .or_else(|| OSR_CREATING.lock().ok().and_then(|c| c.map(|(w, h, _)| (w, h))))
                .unwrap_or((1, 1));
            info.device_scale_factor = scale;
            info.depth = 32;
            info.depth_per_component = 8;
            info.rect = Rect { x: 0, y: 0, width: w, height: h };
            info.available_rect = info.rect.clone();
            1
        }
        // 팝업 위젯 표시/기하 — CEF 가 팝업(select 등)을 별도 페인트 대상(PET_POPUP)으로 그리며
        // 표시 상태와 뷰-로컬 rect(DIP)를 이 두 콜백으로 알린다(스펙 §8 M4 합성의 제어 신호).
        fn on_popup_show(&self, browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            let Some(id) = browser.map(|b| b.identifier()).and_then(engine_id_of) else { return };
            crate::presenter::popup_show(id, show != 0);
        }
        fn on_popup_size(&self, browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let (Some(id), Some(r)) = (browser.map(|b| b.identifier()).and_then(engine_id_of), rect) else { return };
            crate::presenter::popup_rect(id, r.x, r.y, r.width, r.height);
        }
        fn on_accelerated_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            let Some(id) = browser.map(|b| b.identifier()).and_then(engine_id_of) else { return };
            // default = PET_VIEW, 그 외 = PET_POPUP(select 드롭다운·자동완성 위젯) — 서브레이어 합성.
            // 공유 텍스처는 OS별(mac IOSurface·win D3D11 HANDLE·linux DMA-BUF planes) — presenter 가
            // AcceleratedPaintInfo 에서 자기 필드를 추출한다(인터페이스 일반화, engine 은 중립 전달).
            if type_ != PaintElementType::default() {
                crate::presenter::present_popup(id, info);
                return;
            }
            crate::presenter::present(id, info);
        }
        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // 공유 텍스처 비활성 환경(SW GL/lavapipe 등)의 CPU 폴백 — PET_VIEW 만. presenter 가 BGRA 버퍼를
            // 소비한다(macOS 는 공유텍스처 전용이라 드랍, wgpu 프레젠터는 업로드해 렌더). 조용한 강등 금지.
            if type_ != PaintElementType::default() {
                return;
            }
            if let Some(id) = browser.map(|b| b.identifier()).and_then(engine_id_of) {
                crate::presenter::present_cpu(id, buffer, width as i32, height as i32);
            }
        }
    }
}

// offscreen 전용 클라이언트 — 렌더 핸들러만 추가되고 나머지(수명/로드/표시/라우터)는 windowed 와
// 동일 핸들러를 공유한다(이벤트·close 시퀀스·cefQuery 의미 불변).
wrap_client! {
    struct CefOsrClient {}
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(CefOsrRenderHandler::new())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(CefLifeSpanHandler::new())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(CefLoadHandler::new())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(CefDisplayHandler::new())
        }
        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            if let Some(router) = BROWSER_ROUTER.get() {
                let handled = router.on_process_message_received(
                    browser.map(|b| b.clone()),
                    frame.map(|f| f.clone()),
                    source_process,
                    message.map(|m| m.clone()),
                );
                return handled as ::std::os::raw::c_int;
            }
            0
        }
    }
}

// ── CefAppProtocol 충족(unrecognized-selector 크래시의 근치) ───────────────────────────────────
// Chromium(macOS)은 NSApp 이 CefAppProtocol(isHandlingSendEvent / setHandlingSendEvent:)을 구현한다고
// 전제하고 일부 경로(예: DevTools 세션이 붙은 브라우저 close)에서 프로토콜 메서드를 직접 호출한다.
// cefclient 는 NSApplication 서브클래스로 구현하지만 우리 NSApp 은 Tao(Tauri) 소유라 서브클래스 불가 →
// Objective-C 런타임 class_addMethod 로 실제 NSApp 클래스에 두 메서드를 주입해 프로토콜을 충족시킨다.
// (미충족 시 doesNotRecognizeSelector → NSApplication _crashOnException 으로 앱 전체 사망 — 실측 4회.)
#[cfg(target_os = "macos")]
static NSAPP_HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
fn install_cef_app_protocol() {
    use objc2::ffi::{
        class_addMethod, class_addProtocol, class_getInstanceMethod, objc_getProtocol,
        object_getClass,
    };
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    unsafe extern "C-unwind" fn is_handling(_this: *mut AnyObject, _cmd: Sel) -> Bool {
        Bool::new(NSAPP_HANDLING_SEND_EVENT.load(Ordering::Relaxed))
    }
    unsafe extern "C-unwind" fn set_handling(_this: *mut AnyObject, _cmd: Sel, v: Bool) {
        NSAPP_HANDLING_SEND_EVENT.store(v.as_bool(), Ordering::Relaxed);
    }
    unsafe {
        let app: *mut AnyObject =
            objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let cls = object_getClass(app) as *mut AnyClass;
        if cls.is_null() {
            return;
        }
        let sel_get = objc2::sel!(isHandlingSendEvent);
        let sel_set = objc2::sel!(setHandlingSendEvent:);
        if class_getInstanceMethod(cls.cast_const(), sel_get).is_null() {
            class_addMethod(
                cls,
                sel_get,
                std::mem::transmute::<unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool, Imp>(
                    is_handling,
                ),
                c"B@:".as_ptr(), // arm64 BOOL = C bool('B')
            );
        }
        if class_getInstanceMethod(cls.cast_const(), sel_set).is_null() {
            class_addMethod(
                cls,
                sel_set,
                std::mem::transmute::<unsafe extern "C-unwind" fn(*mut AnyObject, Sel, Bool), Imp>(
                    set_handling,
                ),
                c"v@:B".as_ptr(),
            );
        }
        // 명시적 conformsToProtocol 검사 경로까지 커버(프로토콜은 CEF framework 이미지가 등록).
        let proto = objc_getProtocol(c"CefAppProtocol".as_ptr());
        if !proto.is_null() {
            class_addProtocol(cls, proto);
        }
        eprintln!("[chromium] NSApp CefAppProtocol 주입 완료");
    }
}

#[cfg(target_os = "macos")]
fn load_framework_at(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { load_library(Some(&*c.as_ptr().cast())) == 1 }
}

// 엔진 초기화 — 호스트의 soksak_sidecar_engine_init(메인스레드)에서 1회. 모든 경로는 사이드카 dist 디렉토리
// 기준(자기 위치 상대 해소 — PLUGIN-CONTRACT §5): framework/helper/main-bundle 이 전부 dist 안에 산다.
// 서브프로세스는 전용 helper 바이너리가 담당하므로 여기서 execute_process 를 부르지 않는다(cefsimple
// 의 분리-helper 패턴). 성공 시 true.
pub fn initialize(dist_dir: &std::path::Path) -> bool {
    // framework dlopen — dist/Chromium Embedded Framework.framework (macOS 전용 번들).
    #[cfg(target_os = "macos")]
    {
        let framework_dir = dist_dir.join("Chromium Embedded Framework.framework");
        let framework_bin = framework_dir.join("Chromium Embedded Framework");
        if !load_framework_at(&framework_bin) {
            eprintln!("[chromium] framework 로드 실패: {}", framework_bin.display());
            return false;
        }
    }
    // linux/windows 는 libcef 를 빌드타임 링크(cef-dll-sys `rustc-link-lib=dylib=cef`/`libcef`)하므로 런타임
    // 프레임워크 로드가 불요하다(.framework 런타임 로더는 macOS 전용). 리소스 경로는 아래 settings 에서 지정.
    // NSApp 에 CefAppProtocol 주입 — framework 로드 후(프로토콜 심볼이 그 이미지에 등록됨),
    // cef::initialize 이전. 메서드 주입 + 프로토콜 conformance 둘 다 여기서 확정된다.
    #[cfg(target_os = "macos")]
    install_cef_app_protocol();
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    // DevTools 표면 — 원격 디버깅 서버(127.0.0.1 전용)를 빈 포트에 연다. DevTools 는 이 서버가
    // 서빙하는 프론트엔드 웹앱을 일반 탭으로 여는 방식(CEF 의 DevTools 창 제약 원천 회피, 위 주석).
    // 포트는 커맨드라인 콜백(remote-allow-origins)보다 먼저 확정돼야 하므로 여기서 선점한다.
    let devtools_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(0);
    DEVTOOLS_PORT.store(devtools_port as u32, Ordering::Relaxed);

    let args = Args::new();
    let mut settings = Settings::default();
    settings.no_sandbox = 1;
    settings.external_message_pump = 1; // 스레드루프 안 돌고 OnScheduleMessagePumpWork 로 pump 지시
    settings.remote_debugging_port = devtools_port as i32;
    // root_cache_path = 프로세스별 고유 경로 — 미설정이면 Chromium 이 공유 기본 프로필
    // (~/Library/Application Support/CEF/User Data)을 쓰고, 그 안의 ProcessSingleton 이 앱 경계를
    // 넘는다: 두 번째 앱(dev↔debug)의 엔진 기동이 첫 앱으로 위임되어 첫 앱에 네이티브 "New Tab"
    // 창이 뜨고 두 번째 앱 엔진은 기동 실패(그 앱의 크로미움 뷰 전부 blank, reload 불복구 — 실측).
    // identity 독립 원칙(소켓·데이터와 동일)에 따라 프로필도 프로세스 단위로 격리한다 — 어떤 조합
    // (dev+debug·동일 identity 중복)에서도 싱글턴 충돌이 구조적으로 불가능하다. 프로필 영속은 원래
    // 없던 동작이라 그대로 없음: 종료 시 자기 디렉토리 삭제 + 기동 시 죽은 pid 잔재 청소.
    // 키체인("Chromium Safe Storage") 회피는 use-mock-keychain 스위치가 담당(command-line 처리부).
    let cache_root = std::env::temp_dir().join("soksak-chromium");
    let _ = std::fs::create_dir_all(&cache_root);
    if let Ok(entries) = std::fs::read_dir(&cache_root) {
        for e in entries.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                if pid != std::process::id() as i32 && unsafe { libc::kill(pid, 0) } != 0 {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }
    let cache_dir = cache_root.join(std::process::id().to_string());
    let _ = CACHE_DIR.set(cache_dir.clone());
    settings.root_cache_path = CefString::from(cache_dir.to_string_lossy().as_ref());
    // CEF 프로세스/리소스 경로 = macOS .framework/.app 레이아웃 전용. linux/windows 는 libcef 를 dist 형제로
    // 두는 다른 배치라 Phase E 에서 별도 배선(browser_subprocess_path=helper 바이너리·resources=libcef 형제).
    #[cfg(target_os = "macos")]
    {
        let framework_dir = dist_dir.join("Chromium Embedded Framework.framework");
        // 서브프로세스 = dist 의 전용 helper(.app 안 — CEF 정본 macOS 배치에서 dist 가 Frameworks 역할).
        // 실행파일명은 Chromium 관례("<이름> Helper") — 렌더러는 형제 "<이름> Helper (Renderer).app" 변형
        // 번들에서 뜬다(실측: 변형 부재 시 렌더러 spawn 이 조용히 실패해 콘텐츠 blank). 변형 4종
        // (Renderer/GPU/Plugin/Alerts)은 스테이징(make sidecar-chromium)이 배치한다.
        let helper_app = dist_dir.join("soksak-sidecar-browser-chromium Helper.app");
        let helper_bin = helper_app.join("Contents/MacOS/soksak-sidecar-browser-chromium Helper");
        settings.browser_subprocess_path = CefString::from(helper_bin.to_string_lossy().as_ref());
        // dlopen 한 framework 의 리소스(icudtl.dat/locales/.pak) 위치 — 없으면 "icudtl.dat not found" 로 죽음.
        settings.framework_dir_path = CefString::from(framework_dir.to_string_lossy().as_ref());
        settings.resources_dir_path =
            CefString::from(framework_dir.join("Resources").to_string_lossy().as_ref());
        // main_bundle_path — CEF mach-port rendezvous 서비스명은 메인 번들 정체성에서 파생된다. helper 의
        // 최외곽 번들이 곧 helper .app 이므로 브라우저 쪽도 같은 번들을 메인으로 선언해 서비스명을 일치시킨다
        // (검증된 기존 메커니즘의 재지향).
        settings.main_bundle_path = CefString::from(helper_app.to_string_lossy().as_ref());
    }
    // linux/windows: libcef 링크됨 → CEF 리소스(icudtl.dat·locales·*.pak)·helper 서브프로세스를 dist 레이아웃으로.
    // framework_dir_path·main_bundle_path 는 macOS .framework/.app 전용 개념이라 설정하지 않는다.
    #[cfg(not(target_os = "macos"))]
    {
        let helper = if cfg!(target_os = "windows") {
            dist_dir.join("soksak-sidecar-browser-chromium-helper.exe")
        } else {
            dist_dir.join("soksak-sidecar-browser-chromium-helper")
        };
        settings.browser_subprocess_path = CefString::from(helper.to_string_lossy().as_ref());
        settings.resources_dir_path = CefString::from(dist_dir.to_string_lossy().as_ref());
        settings.locales_dir_path =
            CefString::from(dist_dir.join("locales").to_string_lossy().as_ref());
    }
    let mut app = CefApp::new();
    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if ok {
        install_message_router(); // 페이지↔호스트 cefQuery 라우터(UI 스레드, initialize 성공 후)
        eprintln!("[chromium] initialize OK (in-process, dist={})", dist_dir.display());
    } else {
        eprintln!("[chromium] initialize 실패");
    }
    ok
}

// 호스트발 JS 실행 요청 — evalId 를 즉시 반환하고, 결과는 eval-result 이벤트(비동기)로 배달된다.
pub fn request_eval(id: u32, js: String) -> u64 {
    let eval_id = NEXT_EVAL_ID.fetch_add(1, Ordering::Relaxed);
    request_op(Op::Eval { id, eval_id, js });
    eval_id
}

// 플러그인/커맨드가 부르는 임베드 요청 — nsview(부모)·rect·url 로 pane 에 CEF child 를 만든다. id 반환.
// offscreen_scale: Some(devicePixelRatio) = offscreen 호스팅 모드(SIDECARS.md §8), None = windowed.
// PENDING 에 넣고 즉시 pump 를 예약(메인 스레드에서 apply_pending 이 실제 생성).
pub fn request_create(
    nsview: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    url: String,
    offscreen_scale: Option<f32>,
    owner: String,
) -> u32 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut o) = OWNERS.lock() {
        o.insert(id, owner);
    }
    if let Ok(mut q) = PENDING.lock() {
        q.push(CreateReq {
            id,
            nsview,
            x,
            y,
            w,
            h,
            url,
            devtools_of: None,
            screencast: false,
            offscreen_scale,
        });
    }
    schedule_pump(0);
    id
}

// DevTools 를 inspected 브라우저의 임베드 child 로 여는 요청 — 일반 create 와 같은 surface(창 NSView)+rect,
// URL 대신 inspected 엔진 id. id 반환(이후 bounds/hidden/close/분할/이동이 일반 탭과 동일 경로).
// screencast: 프론트엔드의 페이지 미리보기 패널 강제값(플러그인 설정이 정본 — in-memory 프로필).
pub fn request_create_devtools(
    nsview: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    inspected: u32,
    screencast: bool,
    owner: String,
) -> u32 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut o) = OWNERS.lock() {
        o.insert(id, owner);
    }
    if let Ok(mut q) = PENDING.lock() {
        q.push(CreateReq {
            id,
            nsview,
            x,
            y,
            w,
            h,
            url: String::new(),
            devtools_of: Some(inspected),
            screencast,
            offscreen_scale: None,
        });
    }
    schedule_pump(0);
    id
}

// 살아있는 엔진 child id 목록 — E2E/진단용(close 가 실제로 child 를 파괴했는지의 단일 진실).
// CEF API 호출 없이 레지스트리만 읽으므로 어느 스레드든 안전.
pub fn browser_ids() -> Vec<u32> {
    BROWSERS.lock().map(|l| l.iter().map(|(i, _)| *i).collect()).unwrap_or_default()
}

// 프로세스별 프로필 디렉토리(root_cache_path) — initialize 가 채우고 shutdown 이 지운다.
static CACHE_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

// surface(부모 NSView)는 호스트가 ABI ambient 파라미터로 주입한다 — 창 취득은 코어(sidecar.rs
// content_view_of)가 소유하고, 이 크레이트는 usize 만 받는다(tauri 무의존).
pub fn shutdown_engine() {
    shutdown();
    // 자기 프로필 잔재 정리 — 프로필은 세션성(영속 없음)이라 남길 이유가 없다.
    if let Some(dir) = CACHE_DIR.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}
