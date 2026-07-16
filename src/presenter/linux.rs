// offscreen 프레젠터 — Linux 구현. CEF 공유 텍스처(DMA-BUF)를 cef 크레이트 osr_texture_import
// (accelerated_osr, wgpu 29)로 wgpu::Texture 로 가져와, 코어 부모창(X11 XID) 아래 모듈 소유 child 창의
// wgpu::Surface 에 화면정렬 quad 로 렌더한다. GPU 파이프라인은 cef-rs 공식 OSR 예제(examples/osr)를 그대로
// 따른다: import_texture → sampler+bind_group → render_pass(quad) → present. windows 와 wgpu 메커니즘 공유,
// macOS(raw Metal, offscreen.rs)만 별개. 상태 계약(레지스트리·논리크기·scale·hidden)은 offscreen.rs 미러.
//
// ⚠️ 온스크린 렌더의 진위(child 창이 실제로 부모 아래 뜨고 프레임이 보이는지)는 GPU 런타임(CI/xvfb)으로만
// 판정된다 — 컴파일은 API 정합만 보장한다.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle,
};
use wgpu::util::DeviceExt;

// present 완료 프레임 총계 — stats.dbg 로 노출(reference 와 동일 표면).
pub(crate) static FRAMES_PRESENTED: AtomicU64 = AtomicU64::new(0);

// ── quad 정점(cef-rs 예제와 동일: 화면 전체 -1..1, tex 0..1) ─────────────────────────────────
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
}
impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

// 프로세스 하나의 wgpu 컨텍스트(서피스 간 공유) — 파이프라인·quad·sampler·bind-group 레이아웃 포함.
struct WgpuCtx {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    quad: wgpu::Buffer,
    quad_count: u32,
}
static CTX: OnceLock<Option<WgpuCtx>> = OnceLock::new();

fn ctx() -> Option<&'static WgpuCtx> {
    CTX.get_or_init(|| {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::from_comma_list("vulkan"),
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            });
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .ok()?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    required_limits: wgpu::Limits {
                        max_non_sampler_bindings: 2048,
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .ok()?;
            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("Cef Texture Bind Group Layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Cef Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Cef Pipeline Layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Cef Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[Vertex::desc()],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: SURFACE_FORMAT,
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent::OVER,
                            alpha: wgpu::BlendComponent::OVER,
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            });
            let (x, y, w, h, z) = (-1.0f32, 1.0f32, 2.0f32, 2.0f32, 1.0f32);
            let verts = [
                Vertex { position: [x, y, z], tex_coords: [0.0, 0.0] },
                Vertex { position: [x + w, y, z], tex_coords: [1.0, 0.0] },
                Vertex { position: [x, y - h, z], tex_coords: [0.0, 1.0] },
                Vertex { position: [x + w, y - h, z], tex_coords: [1.0, 1.0] },
            ];
            let quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Cef Quad"),
                contents: bytemuck::cast_slice(&verts),
                usage: wgpu::BufferUsages::VERTEX,
            });
            Some(WgpuCtx {
                instance,
                device,
                queue,
                pipeline,
                bind_group_layout,
                sampler,
                quad,
                quad_count: verts.len() as u32,
            })
        })
    })
    .as_ref()
}

// 엔진 id 하나의 present 상태. X11 child 창 + 그 위의 wgpu 서피스 + 논리 상태.
struct Surf {
    display: *mut x11_dl::xlib::Display,
    window: std::os::raw::c_ulong,
    surface: wgpu::Surface<'static>,
    scale: f32,
    log_w: i32,
    log_h: i32,
    hidden: bool,
}
// 모든 함수 메인(CEF UI) 스레드 전용 — Display* 는 그 계약 아래에서만 만진다(offscreen.rs 헤더와 동일).
unsafe impl Send for Surf {}

static SURFS: LazyLock<Mutex<HashMap<u32, Surf>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn xlib() -> Option<&'static x11_dl::xlib::Xlib> {
    static XLIB: OnceLock<Option<x11_dl::xlib::Xlib>> = OnceLock::new();
    XLIB.get_or_init(|| x11_dl::xlib::Xlib::open().ok()).as_ref()
}

fn surface_config(w: i32, h: i32) -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: SURFACE_FORMAT,
        view_formats: vec![SURFACE_FORMAT],
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        width: w.max(1) as u32,
        height: h.max(1) as u32,
        desired_maximum_frame_latency: 2,
        present_mode: wgpu::PresentMode::AutoVsync,
    }
}

pub(crate) fn is_offscreen(id: u32) -> bool {
    SURFS.lock().map(|m| m.contains_key(&id)).unwrap_or(false)
}

pub(crate) fn logical_size(id: u32) -> Option<(i32, i32)> {
    SURFS.lock().ok().and_then(|m| m.get(&id).map(|s| (s.log_w, s.log_h)))
}

pub(crate) fn scale_of(id: u32) -> Option<f32> {
    SURFS.lock().ok().and_then(|m| m.get(&id).map(|s| s.scale))
}

// 부모(XID) 아래 child 창을 만들고 그 위에 wgpu 서피스를 붙여 등록한다. X11 은 top-left 원점이라 y-flip 불요.
pub(crate) fn create_surface(id: u32, parent: usize, x: i32, y: i32, w: i32, h: i32, scale: f32) {
    if parent == 0 {
        return;
    }
    let Some(ctx) = ctx() else {
        log_once(id, "wgpu 컨텍스트 초기화 실패 — offscreen present 불가");
        return;
    };
    let Some(xl) = xlib() else {
        log_once(id, "Xlib 로드 실패");
        return;
    };
    let (w, h) = (w.max(1), h.max(1));
    unsafe {
        let display = (xl.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            log_once(id, "XOpenDisplay 실패");
            return;
        }
        let screen = (xl.XDefaultScreen)(display);
        let window = (xl.XCreateSimpleWindow)(
            display,
            parent as std::os::raw::c_ulong,
            x,
            y,
            w as u32,
            h as u32,
            0,
            (xl.XBlackPixel)(display, screen),
            (xl.XBlackPixel)(display, screen),
        );
        (xl.XMapWindow)(display, window);
        (xl.XFlush)(display);

        let mut dh = XlibDisplayHandle::new(NonNull::new(display as *mut c_void), screen);
        let _ = &mut dh;
        let wh = XlibWindowHandle::new(window);
        let surface = match ctx.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Xlib(dh)),
            raw_window_handle: RawWindowHandle::Xlib(wh),
        }) {
            Ok(s) => s,
            Err(e) => {
                log_once(id, "wgpu 서피스 생성 실패");
                eprintln!("[chromium] offscreen(id={id}): create_surface_unsafe: {e:?}");
                (xl.XDestroyWindow)(display, window);
                (xl.XCloseDisplay)(display);
                return;
            }
        };
        surface.configure(&ctx.device, &surface_config(w, h));
        if let Ok(mut m) = SURFS.lock() {
            m.insert(
                id,
                Surf { display, window, surface, scale, log_w: w, log_h: h, hidden: false },
            );
        }
    }
}

pub(crate) fn set_bounds(id: u32, x: i32, y: i32, w: i32, h: i32) {
    let Some(xl) = xlib() else { return };
    let Some(ctx) = ctx() else { return };
    let (w, h) = (w.max(1), h.max(1));
    if let Ok(mut m) = SURFS.lock() {
        if let Some(s) = m.get_mut(&id) {
            s.log_w = w;
            s.log_h = h;
            unsafe {
                (xl.XMoveResizeWindow)(s.display, s.window, x, y, w as u32, h as u32);
                (xl.XFlush)(s.display);
            }
            s.surface.configure(&ctx.device, &surface_config(w, h));
        }
    }
}

pub(crate) fn set_hidden(id: u32, hidden: bool) {
    let Some(xl) = xlib() else { return };
    if let Ok(mut m) = SURFS.lock() {
        if let Some(s) = m.get_mut(&id) {
            s.hidden = hidden;
            unsafe {
                if hidden {
                    (xl.XUnmapWindow)(s.display, s.window);
                } else {
                    (xl.XMapWindow)(s.display, s.window);
                }
                (xl.XFlush)(s.display);
            }
        }
    }
}

pub(crate) fn destroy(id: u32) {
    let Some(xl) = xlib() else { return };
    if let Some(s) = SURFS.lock().ok().and_then(|mut m| m.remove(&id)) {
        // 서피스를 창보다 먼저 놓는다(창 파괴 후 서피스가 죽은 핸들을 만지지 않도록).
        drop(s.surface);
        unsafe {
            (xl.XDestroyWindow)(s.display, s.window);
            (xl.XCloseDisplay)(s.display);
        }
    }
}

// 팝업은 v2 — PET_POPUP 프레임은 현재 드랍(로그 1회). (macOS 는 서브레이어 합성, wgpu 는 별도 서피스 필요.)
pub(crate) fn popup_show(id: u32, _show: bool) {
    log_once(id, "linux 팝업 위젯 미구현 (v2)");
}
pub(crate) fn popup_rect(_id: u32, _x: i32, _y: i32, _w: i32, _h: i32) {}

pub(crate) fn present(id: u32, info: &cef::AcceleratedPaintInfo) {
    let Some(ctx) = ctx() else { return };
    // CEF 공유 텍스처(DMA-BUF) → wgpu::Texture (crate osr_texture_import, cef-rs 예제 패턴).
    let src = {
        use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;
        let handle = SharedTextureHandle::new(info);
        if let SharedTextureHandle::Unsupported = handle {
            log_once(id, "이 플랫폼은 가속 페인트 미지원 (accelerated_osr)");
            return;
        }
        match handle.import_texture(&ctx.device) {
            Ok(t) => t,
            Err(e) => {
                log_once(id, "공유 텍스처 import 실패");
                eprintln!("[chromium] offscreen(id={id}): import_texture: {e:?}");
                return;
            }
        }
    };
    let mut m = match SURFS.lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    let Some(s) = m.get_mut(&id) else { return };
    if s.hidden {
        return;
    }
    blit_to_surface(ctx, s, id, &src.create_view(&wgpu::TextureViewDescriptor::default()));
}

pub(crate) fn present_popup(id: u32, _info: &cef::AcceleratedPaintInfo) {
    log_once(id, "linux 팝업 present 미구현 (v2)");
}

// CPU 폴백(on_paint) — 공유 텍스처(DMA-BUF) 미가용 호스트(SW GL/lavapipe 등, CI 포함)에서 CEF 가 주는
// BGRA 버퍼를 wgpu 텍스처로 업로드해 같은 surface 파이프라인으로 렌더한다(cef-rs 예제 on_paint 패턴).
pub(crate) fn present_cpu(id: u32, buffer: *const u8, w: i32, h: i32) {
    if buffer.is_null() || w <= 0 || h <= 0 {
        return;
    }
    let Some(ctx) = ctx() else { return };
    let mut m = match SURFS.lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    let Some(s) = m.get_mut(&id) else { return };
    if s.hidden {
        return;
    }
    let buf = unsafe { std::slice::from_raw_parts(buffer, (w * h * 4) as usize) };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Cef CPU Texture"),
        size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SURFACE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        buf,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * w as u32),
            rows_per_image: Some(h as u32),
        },
        wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
    );
    blit_to_surface(ctx, s, id, &texture.create_view(&wgpu::TextureViewDescriptor::default()));
}

// 소스 텍스처 뷰를 surface 에 화면정렬 quad 로 그려 present 한다(present/present_cpu 공유).
fn blit_to_surface(ctx: &WgpuCtx, s: &Surf, id: u32, src_view: &wgpu::TextureView) {
    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Cef Texture Bind Group"),
        layout: &ctx.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(src_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&ctx.sampler),
            },
        ],
    });
    let frame = match s.surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(f) => f,
        wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
            s.surface.configure(&ctx.device, &surface_config(s.log_w, s.log_h));
            f
        }
        _ => return,
    };
    let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
        format: Some(SURFACE_FORMAT),
        ..Default::default()
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Cef Encoder") });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Cef Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&ctx.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_vertex_buffer(0, ctx.quad.slice(..));
        pass.draw(0..ctx.quad_count, 0..1);
    }
    ctx.queue.submit(std::iter::once(encoder.finish()));
    frame.present();
    FRAMES_PRESENTED.fetch_add(1, Ordering::Relaxed);
    log_once(id, "첫 프레임 present (wgpu::Texture → surface)");
}

// id 별 1회 에러 로그 — 조용한 강등 금지(스펙 P 규칙), 프레임마다 폭주 금지.
static LOGGED: LazyLock<Mutex<std::collections::HashSet<(u32, &'static str)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
pub(crate) fn log_once(id: u32, msg: &'static str) {
    if LOGGED.lock().map(|mut s| s.insert((id, msg))).unwrap_or(false) {
        eprintln!("[chromium] offscreen(id={id}): {msg}");
    }
}
