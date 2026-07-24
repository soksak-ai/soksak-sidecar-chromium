// DOM 프레젠터 프레임 스트림(스파이크, 기본 off) — offscreen 풀 IOSurface 를 CPU 로 읽어
// JPEG 인코드 후 localhost MJPEG(multipart/x-mixed-replace)로 내보낸다. 소비자는 플러그인의
// <img src="http://127.0.0.1:<port>/s/<id>"> — 브라우저 표면이 진짜 DOM 요소가 되어 이동·z·
// 클립이 웹뷰 컴포지터 소유가 된다(두-컴포지터 오케스트레이션 이음새의 구조적 제거 실험).
// 판정 기준(사용자): 매끄럽지 않으면 실패 프리픽스 보존 후 폐기.
//
// 설계 결: ① 인코드는 전용 워커 1개 — present 경로는 최신 프레임 슬롯에 복사만 하고 반환
// (latest-wins, 인코더가 밀리면 오래된 프레임은 자연 드랍). ② 구독자 없으면 복사조차 안 한다.
// ③ 서버는 첫 enable 에서 기동(127.0.0.1 임시 포트), 이후 재사용.
#![cfg(target_os = "macos")]

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Condvar, LazyLock, Mutex, OnceLock};

use objc2::runtime::AnyObject;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: *mut AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut AnyObject) -> *mut u8;
    fn IOSurfaceGetBytesPerRow(surface: *mut AnyObject) -> usize;
}
const K_IOSURFACE_LOCK_READ_ONLY: u32 = 1;

struct Frame {
    id: u32,
    w: usize,
    h: usize,
    bgra: Vec<u8>,
}

static PORT: OnceLock<u16> = OnceLock::new();
static ENABLED: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static SUBS: LazyLock<Mutex<HashMap<u32, Vec<TcpStream>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LATEST: LazyLock<(Mutex<Option<Frame>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(None), Condvar::new()));

/// 스트림 켜기/끄기. 반환 = 서버 포트(끔이면 기존 포트 그대로 — 소비자 참고용).
pub(crate) fn enable(id: u32, on: bool) -> u16 {
    if on {
        ensure_server();
        if let Ok(mut s) = ENABLED.lock() {
            s.insert(id);
        }
        crate::engine::kick_paint(id); // 정적 페이지 첫 프레임 킥
    } else {
        if let Ok(mut s) = ENABLED.lock() {
            s.remove(&id);
        }
        if let Ok(mut subs) = SUBS.lock() {
            subs.remove(&id); // 스트림 연결 종료(드랍 = FIN)
        }
    }
    PORT.get().copied().unwrap_or(0)
}

fn wants(id: u32) -> bool {
    let enabled = ENABLED.lock().map(|s| s.contains(&id)).unwrap_or(false);
    if !enabled {
        return false;
    }
    SUBS.lock()
        .map(|m| m.get(&id).map(|v| !v.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

/// present 경로 탭 — 구독자가 있을 때만 풀 서피스를 CPU 복사해 인코더 워커에 넘긴다.
/// 호출 스코프: blit waitUntilCompleted 직후(서피스 내용 확정 상태).
pub(crate) fn maybe_submit(id: u32, surface: *mut AnyObject, w: i32, h: i32) {
    if surface.is_null() || w <= 0 || h <= 0 || !wants(id) {
        return;
    }
    let (w, h) = (w as usize, h as usize);
    let mut bgra = vec![0u8; w * h * 4];
    unsafe {
        let mut seed = 0u32;
        if IOSurfaceLock(surface, K_IOSURFACE_LOCK_READ_ONLY, &mut seed) != 0 {
            return;
        }
        let base = IOSurfaceGetBaseAddress(surface);
        let stride = IOSurfaceGetBytesPerRow(surface);
        if !base.is_null() && stride >= w * 4 {
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    base.add(row * stride),
                    bgra.as_mut_ptr().add(row * w * 4),
                    w * 4,
                );
            }
        }
        IOSurfaceUnlock(surface, K_IOSURFACE_LOCK_READ_ONLY, &mut seed);
    }
    let (slot, cv) = &*LATEST;
    if let Ok(mut l) = slot.lock() {
        *l = Some(Frame { id, w, h, bgra }); // latest-wins — 인코더가 밀리면 이전 프레임 드랍
        cv.notify_one();
    }
}

fn ensure_server() {
    if PORT.get().is_some() {
        return;
    }
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[chromium] frame-stream 바인드 실패: {e}");
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if PORT.set(port).is_err() {
        return;
    }
    eprintln!("[chromium] frame-stream 서버 127.0.0.1:{port}");
    // 접속 수락 스레드 — GET /s/<id> 만 인지(그 외 404 종료).
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || accept_client(stream));
        }
    });
    // 인코더 워커 — 최신 프레임을 JPEG 로 인코드해 해당 id 구독자 전원에 multipart 파트 송신.
    // 20초 무프레임이면 하트비트 파트(text/plain 1바이트)를 흘린다 — WebKit(NSURLSession)의
    // 60초 무데이터 워치독이 정적 페이지 스트림을 "Load failed" 로 끊는 실측의 근치. 겸사겸사
    // 죽은 구독자도 이때 정리된다(write 실패 prune).
    std::thread::spawn(|| {
        let (slot, cv) = &*LATEST;
        loop {
            let frame = {
                let mut guard = slot.lock().unwrap();
                loop {
                    if let Some(f) = guard.take() {
                        break Some(f);
                    }
                    let (g, timeout) = cv
                        .wait_timeout(guard, std::time::Duration::from_secs(20))
                        .unwrap();
                    guard = g;
                    if timeout.timed_out() {
                        break None;
                    }
                }
            };
            match frame {
                Some(f) => encode_and_fanout(f),
                None => heartbeat_all(),
            }
        }
    });
}

fn accept_client(mut stream: TcpStream) {
    let mut buf = [0u8; 2048];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let id: Option<u32> = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|path| path.strip_prefix("/s/"))
        .and_then(|rest| rest.split(['.', '?']).next())
        .and_then(|s| s.parse().ok());
    let Some(id) = id else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
        return;
    };
    // CORS 개방 — 소비자는 메인 웹뷰(fetch, 교차 출처 localhost)다. <img> 와 달리 fetch 는
    // CORS 대상이라 이 헤더가 없으면 연결 직후 응답이 거부된다(루프백 한정 서버라 개방 무해).
    // Content-Type 은 불투명 바이트 스트림 — multipart/x-mixed-replace 를 주면 WebKit 이
    // 레거시 멀티파트 로더로 특별취급해 첫 파트 도착 즉시 fetch 를 죽인다(실측: 파트가 오는
    // 스트림만 Load failed, 침묵 스트림은 생존). 파트 프레이밍은 소비자가 직접 파싱한다.
    let ok = stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: application/octet-stream\r\n\
          Access-Control-Allow-Origin: *\r\n\
          Cache-Control: no-store\r\n\
          Connection: close\r\n\r\n",
    );
    if ok.is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    if let Ok(mut subs) = SUBS.lock() {
        subs.entry(id).or_default().push(stream);
    }
    crate::engine::kick_paint(id); // 구독 직후 1프레임 보장(정적 페이지)
}

// 모든 구독자에 무의미 파트 1개 — 연결 유지 전용(클라이언트 파서는 image/jpeg 아닌 파트를 버린다).
fn heartbeat_all() {
    const PART: &[u8] = b"--sksframe\r\nContent-Type: text/plain\r\nContent-Length: 1\r\n\r\n.\r\n";
    if let Ok(mut subs) = SUBS.lock() {
        for list in subs.values_mut() {
            list.retain_mut(|s| s.write_all(PART).is_ok());
        }
    }
}

fn encode_and_fanout(f: Frame) {
    if !wants(f.id) {
        return;
    }
    // BGRA → RGB(알파 무시 — 페이지 프레임은 불투명).
    let mut rgb = vec![0u8; f.w * f.h * 3];
    for i in 0..f.w * f.h {
        rgb[3 * i] = f.bgra[4 * i + 2];
        rgb[3 * i + 1] = f.bgra[4 * i + 1];
        rgb[3 * i + 2] = f.bgra[4 * i];
    }
    let mut jpeg = Vec::with_capacity(f.w * f.h / 4);
    let enc = jpeg_encoder::Encoder::new(&mut jpeg, 72);
    if enc
        .encode(&rgb, f.w as u16, f.h as u16, jpeg_encoder::ColorType::Rgb)
        .is_err()
    {
        return;
    }
    let header = format!(
        "--sksframe\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        jpeg.len()
    );
    if let Ok(mut subs) = SUBS.lock() {
        if let Some(list) = subs.get_mut(&f.id) {
            list.retain_mut(|s| {
                s.write_all(header.as_bytes())
                    .and_then(|_| s.write_all(&jpeg))
                    .and_then(|_| s.write_all(b"\r\n"))
                    .is_ok()
            });
        }
    }
}
