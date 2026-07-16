// E2E 하니스 — 앱(soksak) 없이 이 크레이트의 엔진을 통째로 구동해 호스팅 계약을 검증한다.
// 코어(sidecar.rs)의 역할을 스스로 수행한다: 호스트 vtable(emit 수신) 제공 + init/message 호출 +
// surface(NSView) 주입. 플러그인의 역할도 수행한다: query 이벤트에 query-reply 로 응답.
//
// 사용: cargo run --release --features harness --bin harness -- <dist-dir> [windowed|offscreen]
// 판정: 테스트 페이지가 window.cefQuery 라운드트립 결과를 document.title 로 보고하고, 하니스가
// title 이벤트로 그것을 관찰한다 — Q_OK:pong 이면 PASS(exit 0), 아니면 FAIL(exit 1).

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod run {
    use std::ffi::{c_void, CStr};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use soksak_sidecar_browser_chromium as lib;

    static SURFACE: AtomicUsize = AtomicUsize::new(0);
    static QUERY_SEEN: AtomicBool = AtomicBool::new(false);
    static LAST_TITLE: Mutex<String> = Mutex::new(String::new());

    // 호스트 vtable emit — 코어의 host_emit 대역. 모든 이벤트를 stdout 에 남기고, query 에는
    // 플러그인처럼 즉시 pong 응답(query-reply)한다.
    extern "C" fn host_emit(_ctx: *mut c_void, json: *const u8, len: usize) {
        let txt = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(json, len) }).to_string();
        println!("[emit] {txt}");
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return };
        match v.get("event").and_then(|e| e.as_str()) {
            Some("query") => {
                QUERY_SEEN.store(true, Ordering::Relaxed);
                let qid = v.get("queryId").and_then(|q| q.as_i64()).unwrap_or(-1);
                let reply = serde_json::json!({
                    "type": "query-reply", "queryId": qid, "success": true,
                    "response": "pong", "keep": false
                });
                send(&reply);
            }
            Some("title") => {
                if let Some(t) = v.get("title").and_then(|t| t.as_str()) {
                    *LAST_TITLE.lock().unwrap() = t.to_string();
                }
            }
            _ => {}
        }
    }

    extern "C" fn host_log(_ctx: *mut c_void, level: i32, msg: *const u8, len: usize) {
        let txt = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(msg, len) });
        println!("[log:{level}] {txt}");
    }

    fn send(msg: &serde_json::Value) -> serde_json::Value {
        let payload = msg.to_string();
        let mut reply = lib::SoksakBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
        let code = lib::soksak_sidecar_engine_message(
            payload.as_ptr(),
            payload.len(),
            SURFACE.load(Ordering::Relaxed),
            &mut reply,
        );
        let body = if reply.ptr.is_null() {
            serde_json::Value::Null
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(reply.ptr, reply.len) };
            let v = serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
            lib::soksak_sidecar_engine_free(reply);
            v
        };
        println!("[send] {} -> code={code} body={body}", msg.get("type").and_then(|t| t.as_str()).unwrap_or("?"));
        body
    }

    // 라운드트립·입력·IME 를 title 로 보고하는 테스트 페이지('#'는 %23 — data URL fragment 방지).
    // 입력 판정: input 포커스=CLICK_OK, wheel=WHEEL_OK, keydown=KEY_OK, input 값 변화=IME_OK.
    const TEST_PAGE: &str = "data:text/html,<title>boot</title><body style=\"background:%23223;color:%23eee;font:20px monospace\">HARNESS<input id=t style=\"position:fixed;left:20px;top:60px;width:220px;height:40px;font-size:20px\"><script>var t=document.getElementById('t');var keyDone=false;t.addEventListener('focus',function(){document.title='CLICK_OK';});window.addEventListener('wheel',function(e){if(document.title.indexOf('CLICK_OK')===0)document.title='WHEEL_OK:'+(e.deltaY>0?'down':'up');});window.addEventListener('keydown',function(e){if(!keyDone&&document.title.indexOf('WHEEL_OK')===0){keyDone=true;document.title='KEY_OK:'+e.keyCode;}});t.addEventListener('input',function(){if(document.title.indexOf('KEY_OK')===0||document.title.indexOf('IME')===0)document.title='IME_OK:'+t.value;});document.title='Q_TYPEOF:'+(typeof window.cefQuery);if(window.cefQuery){window.cefQuery({request:'harness-ping',persistent:false,onSuccess:function(r){document.title='Q_OK:'+r;},onFailure:function(c,m){document.title='Q_FAIL:'+c+':'+m;}});}</script></body>";

    struct App {
        dist: std::path::PathBuf,
        mode: String,
        window: Option<winit::window::Window>,
        started: Option<Instant>,
        created: bool,
        // 입력 검증 상태기계(offscreen 전용) — title 전이에 맞춰 다음 입력 메시지를 보낸다.
        // 0=쿼리 대기, 1=클릭 송신됨, 2=휠 송신됨, 3=키 송신됨, 4=IME 송신됨.
        input_phase: u8,
        // 현재 phase 입력을 마지막으로 보낸 시각 — 오래 전이가 없으면 재전송한다(racy 유실 견고화).
        last_input: Instant,
    }

    // 입력 검증 단계 — 표면 id=1(단일 브라우저) 가정. 좌표는 표면-로컬 논리 px.
    fn send_input_for_phase(phase: u8) {
        match phase {
            1 => {
                // input(20,60~240,100) 중심 클릭 — 포커스 → CLICK_OK.
                send(&serde_json::json!({ "type": "focus", "id": 1 }));
                send(&serde_json::json!({ "type": "mouse", "id": 1, "kind": "move", "x": 120, "y": 80 }));
                send(&serde_json::json!({ "type": "mouse", "id": 1, "kind": "down", "x": 120, "y": 80, "button": 0, "clicks": 1 }));
                send(&serde_json::json!({ "type": "mouse", "id": 1, "kind": "up", "x": 120, "y": 80, "button": 0, "clicks": 1 }));
            }
            2 => {
                // wheel(DOM 부호: +아래) → WHEEL_OK:down 이어야 한다(엔진이 CEF 부호로 변환).
                send(&serde_json::json!({ "type": "wheel", "id": 1, "x": 350, "y": 250, "dx": 0, "dy": 120 }));
            }
            3 => {
                // 'A'(65) down+char+up → KEY_OK:65.
                send(&serde_json::json!({ "type": "key", "id": 1, "kind": "down", "code": 65 }));
                send(&serde_json::json!({ "type": "key", "id": 1, "kind": "char", "code": 65, "char": "a" }));
                send(&serde_json::json!({ "type": "key", "id": 1, "kind": "up", "code": 65 }));
            }
            4 => {
                // 한글 조합: preedit '하' → '한' → commit '한글' — input 값 변화 → IME_OK:...한글.
                send(&serde_json::json!({ "type": "ime", "id": 1, "kind": "set", "text": "하", "caret": 1 }));
                send(&serde_json::json!({ "type": "ime", "id": 1, "kind": "set", "text": "한", "caret": 1 }));
                send(&serde_json::json!({ "type": "ime", "id": 1, "kind": "commit", "text": "한글" }));
            }
            _ => {}
        }
    }

    impl winit::application::ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = winit::window::Window::default_attributes()
                .with_title("engine harness")
                .with_inner_size(winit::dpi::LogicalSize::new(760.0, 520.0));
            let window = event_loop.create_window(attrs).expect("window");
            // surface = 코어가 넘기는 부모 핸들(content_view_of 대역): macOS=창 콘텐츠 NSView,
            // linux=창 XID(프레젠터가 그 아래 X11 child 창을 만든다). 엔진은 usize 로 받아 per-OS 해석.
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let raw = window.window_handle().expect("handle").as_raw();
            #[cfg(target_os = "macos")]
            let surface = {
                let RawWindowHandle::AppKit(h) = raw else { panic!("AppKit 핸들이 아님") };
                h.ns_view.as_ptr() as usize
            };
            #[cfg(target_os = "linux")]
            let surface = {
                let RawWindowHandle::Xlib(h) = raw else {
                    panic!("Xlib 핸들이 아님 — X11 백엔드 필요(WINIT_UNIX_BACKEND=x11)")
                };
                h.window as usize
            };
            #[cfg(target_os = "windows")]
            let surface = {
                let RawWindowHandle::Win32(h) = raw else { panic!("Win32 핸들이 아님") };
                h.hwnd.get() as usize
            };
            SURFACE.store(surface, Ordering::Relaxed);
            self.window = Some(window);

            // 코어의 init 대역 — 메인 스레드(여기), distDir 전달.
            let host = lib::SoksakSidecarEngineHost {
                abi: lib::HOST_ABI_VERSION,
                ctx: std::ptr::null_mut(),
                emit: host_emit,
                log: host_log,
            };
            let cfg = serde_json::json!({
                "name": "browser-chromium",
                "distDir": self.dist.to_string_lossy(),
            })
            .to_string();
            let abi = lib::soksak_sidecar_engine_abi();
            let iface = unsafe { CStr::from_ptr((*abi).interface_id) }.to_string_lossy().to_string();
            let iver = unsafe { CStr::from_ptr((*abi).interface_version) }.to_string_lossy().to_string();
            println!("[harness] abi={} interface={iface} version={iver}", unsafe { (*abi).abi });
            let rc = lib::soksak_sidecar_engine_init(&host, cfg.as_ptr(), cfg.len());
            println!("[harness] init rc={rc}");
            assert_eq!(rc, 0, "engine init 실패");

            let mut create = serde_json::json!({
                "type": "create", "x": 10, "y": 10, "w": 700, "h": 440, "url": TEST_PAGE
            });
            if self.mode == "offscreen" {
                create["mode"] = "offscreen".into();
                create["scale"] = 2.0.into();
            }
            let out = send(&create);
            println!("[harness] create({}) -> {out}", self.mode);
            self.created = out.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
            self.started = Some(Instant::now());
        }

        fn window_event(
            &mut self,
            event_loop: &winit::event_loop::ActiveEventLoop,
            _id: winit::window::WindowId,
            event: winit::event::WindowEvent,
        ) {
            if matches!(event, winit::event::WindowEvent::CloseRequested) {
                event_loop.exit();
            }
        }

        fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            // 비-macOS: 코어(=하니스) 가 CEF 펌프를 tick 한다(macOS 는 GCD 가 코어 런루프에 자동 전달).
            // 이게 Phase F 에서 실 코어의 메인루프가 하는 일의 하니스판이다.
            #[cfg(not(target_os = "macos"))]
            lib::soksak_sidecar_engine_tick();
            let Some(t0) = self.started else {
                event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(50),
                ));
                return;
            };
            let title = LAST_TITLE.lock().unwrap().clone();
            // offscreen 은 픽셀 경로 생존(stats.dbg.framesPresented > 0)까지 요구 — 쿼리만 빠르게
            // 성공하고 첫 페인트 전에 종료해 픽셀 경로를 미검증으로 남기는 오판을 막는다.
            let frames = if self.mode == "offscreen" {
                send(&serde_json::json!({ "type": "stats" }))
                    .get("dbg")
                    .and_then(|d| d.get("framesPresented"))
                    .and_then(|f| f.as_u64())
                    .unwrap_or(0)
            } else {
                u64::MAX // windowed 는 CEF 네이티브 present — 이 카운터의 대상 아님
            };

            // offscreen 입력 상태기계 — title 전이에 맞춰 다음 단계 입력을 1회 송신.
            if self.mode == "offscreen" {
                let next = match (self.input_phase, title.as_str()) {
                    (0, t) if t.starts_with("Q_OK:") && frames > 0 => Some(1),
                    (1, "CLICK_OK") => Some(2),
                    (2, t) if t.starts_with("WHEEL_OK:down") => Some(3),
                    // KEY_OK:65 은 char 'a' input 이벤트가 즉시 IME_OK:a 로 덮어써(테스트페이지 설계) title
                    // 폴링이 그 찰나를 놓치면 phase 4 를 못 보내 플래키 FAIL 이 났다(베이스라인도 동일: 2/6 FAIL,
                    // 리팩터 전부터 있던 레이스). key 성공의 정착 상태 IME_OK:a 도 트리거로 인정해 결정적으로
                    // 만든다. 최종 단언(IME_OK:a한글)은 불변 — 검증 범위 약화 아님. IME_OK:a 는 KEY_OK 를
                    // 함의하므로(input 핸들러가 KEY_OK/IME 일 때만 IME_OK 로 전이) key 포워딩 검증도 유지된다.
                    (3, t) if t.starts_with("KEY_OK:65") || t == "IME_OK:a" => Some(4),
                    _ => None,
                };
                if let Some(p) = next {
                    println!("[harness] 입력 단계 {p} 송신 (title={title:?})");
                    self.input_phase = p;
                    self.last_input = Instant::now();
                    send_input_for_phase(p);
                } else if self.input_phase >= 1
                    && self.last_input.elapsed() > Duration::from_millis(700)
                {
                    // 전이가 안 일어난 채 오래 멈췄다 — 현재 phase 입력이 racy 하게 유실됐을 수 있다
                    // (Windows OSR: 뷰 준비 직후 첫 입력이 드롭되는 타이밍 실측). 같은 phase 입력을
                    // 재전송한다. 페이지 핸들러는 멱등(focus/wheel/key(keyDone 가드)/ime) 이고 최종
                    // 단언(IME_OK:a한글)은 불변이라 검증 범위 약화가 아니다 — flaky 테스트의 견고화다.
                    // linux/macOS 는 <700ms 에 전이하므로 이 재전송이 발동하지 않는다.
                    println!(
                        "[harness] 입력 단계 {} 재전송 (stuck {}ms, title={title:?})",
                        self.input_phase,
                        self.last_input.elapsed().as_millis()
                    );
                    self.last_input = Instant::now();
                    send_input_for_phase(self.input_phase);
                }
            }

            // offscreen 최종 단언: key 단계의 'a' 뒤에 IME commit '한글'이 붙는다 → "IME_OK:a한글".
            let title_pass = if self.mode == "offscreen" {
                title.starts_with("IME_OK:") && title.ends_with("한글")
            } else {
                title.starts_with("Q_OK:pong")
            };
            let done = (title_pass && frames > 0) || title.starts_with("Q_FAIL:");
            if t0.elapsed() > Duration::from_secs(20) || done {
                let q = QUERY_SEEN.load(Ordering::Relaxed);
                println!(
                    "[harness] 판정: created={} query_seen={q} frames={frames} phase={} last_title={title:?}",
                    self.created, self.input_phase
                );
                let pass = self.created && q && title_pass && frames > 0;
                println!("[harness] {}", if pass { "PASS" } else { "FAIL" });
                std::process::exit(if pass { 0 } else { 1 });
            }
            // 비-macOS 는 이 poll 이 곧 펌프 tick 주기 → ~60fps 로 촘촘히(CEF 로드·페인트 진행 확보).
            // macOS 는 GCD 가 펌프를 따로 구동하므로 폴링만 100ms.
            #[cfg(target_os = "macos")]
            let poll = Duration::from_millis(100);
            #[cfg(not(target_os = "macos"))]
            let poll = Duration::from_millis(16);
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + poll,
            ));
        }
    }

    pub fn main() {
        let mut args = std::env::args().skip(1);
        let dist = std::path::PathBuf::from(args.next().unwrap_or_else(|| "dist".into()))
            .canonicalize()
            .expect("dist 경로");
        let mode = args.next().unwrap_or_else(|| "windowed".into());
        println!("[harness] dist={} mode={mode}", dist.display());
        let event_loop = winit::event_loop::EventLoop::new().expect("event loop");
        let mut app = App {
            dist,
            mode,
            window: None,
            started: None,
            created: false,
            input_phase: 0,
            last_input: Instant::now(),
        };
        event_loop.run_app(&mut app).expect("run");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn main() {
    run::main();
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("harness: macOS·linux·windows 전용");
    std::process::exit(1);
}
