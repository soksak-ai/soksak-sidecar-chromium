// offscreen 프레젠터 — macOS 프로덕션 구현. presenter/{windows,linux} 과 대등한 peer 다. 동작 정답은
// 동결 오라클 src/offscreen.rs 이고, 이 모듈은 그 IOSurface→Metal→CALayer 순서를 충실 재현한다 —
// 알고리즘 발명 0, 구조만 presenter 인터페이스 뒤로 정렬. 검증 하니스가 이 출력 == 오라클 출력을 단언한다.
//
// 엔진(Chromium)이 offscreen 렌더한 공유 텍스처(IOSurface)를 모듈 소유 layer-hosting NSView 의 CALayer 로
// present 한다. 픽셀은 이 파일 밖으로 나가지 않는다(SIDECARS.md §8: 프레임 데이터의 호스트 vtable/IPC/JS 통과 금지).
//
// CEF 계약(cef_render_handler_t::OnAcceleratedPaint 원문): 전달된 IOSurface 는 CEF 풀 소유 —
// 콜백 밖 사용/캐시 금지, "contents should be copied to a texture owned by the client". 그래서
// 모듈 소유 IOSurface 풀(3장)에 Metal blit 으로 복사한 뒤 그 풀 서피스를 layer.contents 로 스왑한다.
// CPU 픽셀 복사 0회 — 복사는 GPU blit 1회(계약상 필수)뿐.
//
// 스레딩: 모든 함수는 메인 스레드 전용. external_message_pump + multi_threaded_message_loop=0 이라
// CEF UI 스레드 == 메인 스레드이고, OnAcceleratedPaint 도 do_message_loop_work 안(메인)에서 온다.
// SURFS 의 raw 포인터들은 이 계약 아래에서만 유효하다(Send 는 컨테이너 요구를 위한 형식적 구현).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, OnceLock};

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

// IOSurface.framework — 풀 서피스 생성(kIOSurface* 키는 프레임워크 export 상수, 문자열 재선언 금지).
#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: *const AnyObject;
    static kIOSurfaceHeight: *const AnyObject;
    static kIOSurfaceBytesPerElement: *const AnyObject;
    static kIOSurfacePixelFormat: *const AnyObject;
    fn IOSurfaceCreate(properties: *const AnyObject) -> *mut AnyObject;
}

// Metal.framework — 시스템 기본 디바이스(blit 전용, 렌더패스 없음).
#[link(name = "Metal", kind = "framework")]
extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut AnyObject;
}

// present 완료 프레임 총계 — stats.dbg 로 노출(E2E 가 픽셀 경로 생존을 수치로 단언한다).
pub(crate) static FRAMES_PRESENTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const POOL: usize = 3; // 컴포지터가 이전 contents 를 읽는 동안 다음 프레임을 쓰기 위한 트리플 버퍼
const MTL_PIXEL_BGRA8: usize = 80; // MTLPixelFormatBGRA8Unorm — CEF OSR 기본 포맷(BGRA)과 일치
const FOURCC_BGRA: u32 = 0x4247_5241; // 'BGRA'

// 엔진 id 하나의 present 상태. 포인터는 전부 메인 스레드에서만 만지고, 소유권 규칙은 각 필드 주석.
// 프레임 풀 — 대상(뷰/팝업)마다 하나. IOSurfaceRef(Create 소유)+감싼 MTLTexture(new* 소유),
// coded 크기 불일치 시 rebuild, 라운드로빈 스왑.
struct Pool {
    slots: [usize; POOL],
    tex: [usize; POOL],
    w: i32,
    h: i32,
    next: usize,
}
impl Pool {
    const fn empty() -> Self {
        Pool { slots: [0; POOL], tex: [0; POOL], w: 0, h: 0, next: 0 }
    }
}

struct Surf {
    view: usize,  // NSView(alloc-init 소유, destroy 에서 removeFromSuperview+release)
    layer: usize, // CALayer(view 가 setLayer: 로 retain — view 수명에 종속, 별도 release 안 함)
    scale: f32,   // devicePixelRatio(create 시 고정) — layer.contentsScale·screen_info 로 일관 노출
    log_w: i32,   // 논리(px) 크기 — CEF view_rect 가 보고하는 값(bounds 가 갱신)
    log_h: i32,
    hidden: bool,
    pool: Pool, // 뷰 프레임 풀
    // 팝업 위젯(select 드롭다운·자동완성) 합성(스펙 §8 M4) — PET_POPUP 프레임을 루트 레이어 위의
    // 서브레이어로 그린다. 레이어는 지연 생성(0=미생성), 수명은 루트 레이어에 종속(superlayer 소유).
    popup_layer: usize,
    popup_shown: bool,
    popup_rect: (i32, i32, i32, i32), // DIP, 뷰-로컬 top-left(on_popup_size 원본)
    popup_pool: Pool,
}
unsafe impl Send for Surf {} // 메인 스레드 전용 계약(파일 헤더) — 컨테이너 Mutex 요구용

static SURFS: LazyLock<Mutex<HashMap<u32, Surf>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// Metal 디바이스/커맨드큐 — 프로세스에 하나(서피스 간 공유). 실패 시 None 고정(present 가 조용히
// 스킵되지 않도록 present 쪽에서 1회 에러 로그).
struct Gpu {
    device: usize,
    queue: usize,
}
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}
static GPU: OnceLock<Option<Gpu>> = OnceLock::new();

fn gpu() -> Option<&'static Gpu> {
    GPU.get_or_init(|| unsafe {
        let device = MTLCreateSystemDefaultDevice();
        if device.is_null() {
            eprintln!("[chromium] offscreen: Metal 디바이스 없음 — offscreen present 불가");
            return None;
        }
        let queue: *mut AnyObject = msg_send![&*device, newCommandQueue];
        if queue.is_null() {
            eprintln!("[chromium] offscreen: Metal 커맨드큐 생성 실패");
            return None;
        }
        Some(Gpu { device: device as usize, queue: queue as usize })
    })
    .as_ref()
}

// 존재 여부 — 엔진이 windowed/offscreen 분기(hidden/bounds/cursor 이벤트 필터)에 쓴다.
pub(crate) fn is_offscreen(id: u32) -> bool {
    SURFS.lock().map(|m| m.contains_key(&id)).unwrap_or(false)
}

// CEF view_rect 용 논리 크기. 등록 전(생성 중)은 None — 호출부가 CREATING 스태시로 폴백.
pub(crate) fn logical_size(id: u32) -> Option<(i32, i32)> {
    SURFS.lock().ok().and_then(|m| m.get(&id).map(|s| (s.log_w, s.log_h)))
}

pub(crate) fn scale_of(id: u32) -> Option<f32> {
    SURFS.lock().ok().and_then(|m| m.get(&id).map(|s| s.scale))
}

// layer-hosting NSView + CALayer 를 만들어 부모의 형제 맨 아래(메인 webview 아래)에 삽입하고 등록한다.
// windowed 와 달리 코어 hitTest(SURFACES)에 알리지 않는다 — DOM 셀이 입력을 소유(스펙 §8).
// 좌표는 top-left 논리 px — apply 시 y-flip(엔진과 동일 관행).
pub(crate) fn create_surface(id: u32, parent: usize, x: i32, y: i32, w: i32, h: i32, scale: f32) {
    let parent_ptr = parent as *mut AnyObject;
    if parent_ptr.is_null() {
        return;
    }
    unsafe {
        // layer-hosting(setLayer 먼저, setWantsLayer 나중) — layer-backed 와 달리 AppKit 이
        // contents 를 다시 그리지 않아 우리가 스왑한 IOSurface 가 살아남는다.
        let layer: *mut AnyObject = msg_send![class!(CALayer), new];
        let _: () = msg_send![&*layer, setContentsScale: scale as f64];
        let view: *mut AnyObject = msg_send![class!(NSView), alloc];
        let frame = objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(x as f64, crate::engine::flip_y(parent_ptr as *mut c_void, y, h.max(1)) as f64),
            objc2_foundation::NSSize::new(w.max(1) as f64, h.max(1) as f64),
        );
        let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
        let _: () = msg_send![&*view, setLayer: &*layer];
        let _: () = msg_send![&*view, setWantsLayer: true];
        // 형제 맨 아래 = 메인(투명) webview 아래 — windowed 재배치와 동일한 삽입 규칙.
        let _: () = msg_send![&*parent_ptr, addSubview: &*view, positioned: -1isize, relativeTo: std::ptr::null::<AnyObject>()];
        if let Ok(mut m) = SURFS.lock() {
            m.insert(
                id,
                Surf {
                    view: view as usize,
                    layer: layer as usize,
                    scale,
                    log_w: w.max(1),
                    log_h: h.max(1),
                    hidden: false,
                    pool: Pool::empty(),
                    popup_layer: 0,
                    popup_shown: false,
                    popup_rect: (0, 0, 0, 0),
                    popup_pool: Pool::empty(),
                },
            );
        }
        // setLayer: 가 retain 했으므로 우리 몫(new 의 +1)은 놓는다 — 수명은 view 에 종속.
        let _: () = msg_send![&*layer, release];
    }
}

// bounds 갱신(top-left 논리 px). 프레임 세팅은 엔진의 windowed 경로와 동일 헬퍼를 재사용한다.
pub(crate) fn set_bounds(id: u32, x: i32, y: i32, w: i32, h: i32) {
    let view = match SURFS.lock().ok().and_then(|mut m| {
        m.get_mut(&id).map(|s| {
            s.log_w = w.max(1);
            s.log_h = h.max(1);
            s.view
        })
    }) {
        Some(v) => v,
        None => return,
    };
    unsafe {
        let v = view as *mut AnyObject;
        let sup: *mut AnyObject = msg_send![&*v, superview];
        let ns_y = crate::engine::flip_y(sup as *mut c_void, y, h.max(1));
        crate::engine::set_view_frame(view as *mut c_void, x, ns_y, w, h);
    }
}

pub(crate) fn set_hidden(id: u32, hidden: bool) {
    let view = match SURFS.lock().ok().and_then(|mut m| {
        m.get_mut(&id).map(|s| {
            s.hidden = hidden;
            s.view
        })
    }) {
        Some(v) => v,
        None => return,
    };
    unsafe {
        let v = view as *mut AnyObject;
        let _: () = msg_send![&*v, setHidden: hidden];
    }
}

// 파괴(reap 시) — 뷰 제거 + 풀 반환. 등록이 없으면 no-op(windowed id).
pub(crate) fn destroy(id: u32) {
    let surf = match SURFS.lock().ok().and_then(|mut m| m.remove(&id)) {
        Some(s) => s,
        None => return,
    };
    unsafe {
        release_pool(&surf.pool);
        release_pool(&surf.popup_pool);
        let v = surf.view as *mut AnyObject;
        let _: () = msg_send![&*v, removeFromSuperview];
        let _: () = msg_send![&*v, release];
    }
}

// 팝업 위젯 표시/숨김(on_popup_show) — 서브레이어 지연 생성, 숨김 시 contents 해제(스냅샷 잔상 방지).
pub(crate) fn popup_show(id: u32, show: bool) {
    let Ok(mut m) = SURFS.lock() else { return };
    let Some(surf) = m.get_mut(&id) else { return };
    surf.popup_shown = show;
    unsafe {
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        if show {
            if surf.popup_layer == 0 {
                let sub: *mut AnyObject = msg_send![class!(CALayer), new];
                let _: () = msg_send![&*sub, setContentsScale: surf.scale as f64];
                let root = surf.layer as *mut AnyObject;
                let _: () = msg_send![&*root, addSublayer: &*sub];
                // addSublayer 가 retain — 우리 몫(+1)을 놓아 superlayer(→view) 수명에 종속시킨다.
                let _: () = msg_send![&*sub, release];
                surf.popup_layer = sub as usize;
            }
            apply_popup_frame(surf);
            let l = surf.popup_layer as *mut AnyObject;
            let _: () = msg_send![&*l, setHidden: false];
        } else if surf.popup_layer != 0 {
            let l = surf.popup_layer as *mut AnyObject;
            let _: () = msg_send![&*l, setHidden: true];
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![&*l, setContents: nil];
        }
        let _: () = msg_send![class!(CATransaction), commit];
    }
}

// 팝업 위젯 rect(on_popup_size — DIP, 뷰-로컬 top-left) 기록·적용.
pub(crate) fn popup_rect(id: u32, x: i32, y: i32, w: i32, h: i32) {
    let Ok(mut m) = SURFS.lock() else { return };
    let Some(surf) = m.get_mut(&id) else { return };
    surf.popup_rect = (x, y, w, h);
    if surf.popup_layer != 0 {
        unsafe {
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: true];
            apply_popup_frame(surf);
            let _: () = msg_send![class!(CATransaction), commit];
        }
    }
}

// 서브레이어 프레임 적용 — CALayer 기하는 bottom-left 원점이라 y 를 뒤집는다(뷰 콘텐츠 좌표와 정합).
unsafe fn apply_popup_frame(surf: &Surf) {
    if surf.popup_layer == 0 {
        return;
    }
    let (x, y, w, h) = surf.popup_rect;
    let l = surf.popup_layer as *mut AnyObject;
    let frame = objc2_foundation::NSRect::new(
        objc2_foundation::NSPoint::new(x as f64, (surf.log_h - y - h) as f64),
        objc2_foundation::NSSize::new(w.max(1) as f64, h.max(1) as f64),
    );
    let _: () = msg_send![&*l, setFrame: frame];
}

unsafe fn release_pool(pool: &Pool) {
    for i in 0..POOL {
        if pool.tex[i] != 0 {
            let t = pool.tex[i] as *mut AnyObject;
            let _: () = msg_send![&*t, release];
        }
        if pool.slots[i] != 0 {
            let s = pool.slots[i] as *mut AnyObject;
            let _: () = msg_send![&*s, release];
        }
    }
}

// 픽셀 크기 w×h 의 모듈 소유 IOSurface 생성(+1 소유). BGRA 4바이트/픽셀.
unsafe fn create_iosurface(w: i32, h: i32) -> *mut AnyObject {
    let n_w: *mut AnyObject = msg_send![class!(NSNumber), numberWithInt: w];
    let n_h: *mut AnyObject = msg_send![class!(NSNumber), numberWithInt: h];
    let n_bpe: *mut AnyObject = msg_send![class!(NSNumber), numberWithInt: 4i32];
    let n_fmt: *mut AnyObject = msg_send![class!(NSNumber), numberWithUnsignedInt: FOURCC_BGRA];
    let keys: [*const AnyObject; 4] =
        [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat];
    let vals: [*mut AnyObject; 4] = [n_w, n_h, n_bpe, n_fmt];
    let dict: *mut AnyObject = msg_send![
        class!(NSDictionary),
        dictionaryWithObjects: vals.as_ptr(),
        forKeys: keys.as_ptr(),
        count: 4usize
    ];
    IOSurfaceCreate(dict)
}

// IOSurface 를 감싼 MTLTexture(+1 소유). 실패 시 null.
unsafe fn wrap_texture(device: *mut AnyObject, surface: *mut AnyObject, w: i32, h: i32) -> *mut AnyObject {
    let desc: *mut AnyObject = msg_send![
        class!(MTLTextureDescriptor),
        texture2DDescriptorWithPixelFormat: MTL_PIXEL_BGRA8,
        width: w as usize,
        height: h as usize,
        mipmapped: false
    ];
    msg_send![&*device, newTextureWithDescriptor: &*desc, iosurface: &*surface, plane: 0usize]
}

// 풀을 coded 픽셀 크기로 (재)구축. 실패 슬롯은 0 으로 남고 present 가 그 슬롯을 건너뛴다.
unsafe fn ensure_pool(pool: &mut Pool, device: *mut AnyObject, w: i32, h: i32) {
    if pool.w == w && pool.h == h && pool.slots[0] != 0 {
        return;
    }
    release_pool(pool);
    for i in 0..POOL {
        let s = create_iosurface(w, h);
        let t = if s.is_null() { std::ptr::null_mut() } else { wrap_texture(device, s, w, h) };
        pool.slots[i] = s as usize;
        pool.tex[i] = if t.is_null() { 0 } else { t as usize };
    }
    pool.w = w;
    pool.h = h;
    pool.next = 0;
}

// 프레임 present — CEF 의 공유 IOSurface(콜백 스코프 한정)를 풀 슬롯에 GPU blit 후 layer.contents 스왑.
// waitUntilCompleted: 콜백 반환 즉시 CEF 가 서피스를 풀로 되돌려 다음 프레임을 그 위에 그릴 수 있으므로
// (계약 원문), 복사 완결을 기다리는 것이 정공법이다 — blit 1회는 서브밀리초.
pub(crate) fn present(id: u32, info: &cef::AcceleratedPaintInfo) {
    present_target(id, info.shared_texture_io_surface, info.extra.coded_size.width, info.extra.coded_size.height, false);
}

// PET_POPUP 프레임 — 팝업 서브레이어로 합성(스펙 §8 M4).
pub(crate) fn present_popup(id: u32, info: &cef::AcceleratedPaintInfo) {
    present_target(id, info.shared_texture_io_surface, info.extra.coded_size.width, info.extra.coded_size.height, true);
}

// CPU 폴백(on_paint) — macOS offscreen 은 공유 텍스처(IOSurface) 전용이라 CPU 프레임은 드랍(1회 로그).
pub(crate) fn present_cpu(id: u32, _buffer: *const u8, _w: i32, _h: i32) {
    log_once(id, "CPU on_paint 경로 감지 — 공유 텍스처 비활성, 프레임 드랍");
}

fn present_target(id: u32, cef_surface: *mut c_void, coded_w: i32, coded_h: i32, popup: bool) {
    if cef_surface.is_null() || coded_w <= 0 || coded_h <= 0 {
        return;
    }
    let Some(g) = gpu() else {
        log_once(id, "Metal 불가 — offscreen 프레임 드랍");
        return;
    };
    let mut m = match SURFS.lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    let Some(surf) = m.get_mut(&id) else { return };
    if surf.hidden {
        return; // 숨김 중 프레임은 버린다(다시 보일 때 was_hidden(0)→invalidate 로 재페인트)
    }
    if popup && (!surf.popup_shown || surf.popup_layer == 0) {
        return; // 팝업 닫힘 뒤 잔여 프레임 — 버린다
    }
    unsafe {
        objc2::rc::autoreleasepool(|_| {
            let device = g.device as *mut AnyObject;
            let queue = g.queue as *mut AnyObject;
            let target_layer = if popup { surf.popup_layer } else { surf.layer };
            let pool = if popup { &mut surf.popup_pool } else { &mut surf.pool };
            ensure_pool(pool, device, coded_w, coded_h);
            let slot = pool.next;
            let dst_surface = pool.slots[slot] as *mut AnyObject;
            let dst_tex = pool.tex[slot] as *mut AnyObject;
            if dst_surface.is_null() || dst_tex.is_null() {
                log_once(id, "IOSurface 풀 구축 실패 — offscreen 프레임 드랍");
                return;
            }
            // CEF 서피스는 매 콜백 재개방(캐시 금지 — 계약). 크기는 coded_size.
            let src_tex = wrap_texture(device, cef_surface as *mut AnyObject, coded_w, coded_h);
            if src_tex.is_null() {
                log_once(id, "CEF IOSurface 텍스처 개방 실패");
                return;
            }
            let cmd: *mut AnyObject = msg_send![&*queue, commandBuffer];
            let enc: *mut AnyObject = msg_send![&*cmd, blitCommandEncoder];
            // 전체 복사(동일 크기·포맷 — copyFromTexture:toTexture:). dirtyRects 부분 blit 은 v2.
            let _: () = msg_send![&*enc, copyFromTexture: &*src_tex, toTexture: &*dst_tex];
            let _: () = msg_send![&*enc, endEncoding];
            let _: () = msg_send![&*cmd, commit];
            let _: () = msg_send![&*cmd, waitUntilCompleted];
            let _: () = msg_send![&*src_tex, release];
            // 암묵 애니메이션(페이드) 차단 후 contents 스왑.
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: true];
            let layer = target_layer as *mut AnyObject;
            let _: () = msg_send![&*layer, setContents: &*dst_surface];
            let _: () = msg_send![class!(CATransaction), commit];
            pool.next = (slot + 1) % POOL;
            FRAMES_PRESENTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // 첫 프레임 1회 로그 — 픽셀 경로 생존 증거(E2E 하니스/진단이 이 라인을 관찰한다).
            log_once(id, if popup { "첫 팝업 프레임 present (shared-texture → sublayer)" } else { "첫 프레임 present (shared-texture → layer)" });
        });
    }
}

// id 별 1회 에러 로그 — 조용한 강등 금지(스펙 P 규칙), 그러나 프레임마다 로그 폭주도 금지.
static LOGGED: LazyLock<Mutex<std::collections::HashSet<(u32, &'static str)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
pub(crate) fn log_once(id: u32, msg: &'static str) {
    if LOGGED.lock().map(|mut s| s.insert((id, msg))).unwrap_or(false) {
        eprintln!("[chromium] offscreen(id={id}): {msg}");
    }
}
