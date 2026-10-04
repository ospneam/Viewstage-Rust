//! Connect a window with a renderer.
use crate::core::Color;
use crate::graphics::color;
use crate::graphics::compositor;
use crate::graphics::error;
use crate::graphics::{self, Shell, Viewport};
use crate::settings::{self, Settings};
use crate::{Engine, Renderer};

/// A window graphics backend for iced powered by `wgpu`.
pub struct Compositor {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    engine: Engine,
    settings: Settings,
    /// VIEWSTAGE_DCOMP 门控：true 时 surface 走 DirectComposition 合成链。
    dcomp: bool,
}

/// iced 窗口的呈现载体：经典 wgpu surface 或 DComp 合成 surface。
pub enum Surface {
    /// 经典路径：wgpu 直接在窗口上创建的 surface。
    Wgpu(wgpu::Surface<'static>),
    /// P2 主视觉接管：DirectComposition composition swapchain
    /// （VIEWSTAGE_DCOMP=1 门控，仅 windows）。
    #[cfg(windows)]
    Dcomp(super::dcomp_surface::DcompSurface),
}

/// A compositor error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The surface creation failed.
    #[error("the surface creation failed: {0}")]
    SurfaceCreationFailed(#[from] wgpu::CreateSurfaceError),
    /// The surface is not compatible.
    #[error("the surface is not compatible")]
    IncompatibleSurface,
    /// No adapter was found for the options requested.
    #[error("no adapter was found for the options requested: {0:?}")]
    NoAdapterFound(String),
    /// No device request succeeded.
    #[error("no device request succeeded: {0:?}")]
    RequestDeviceFailed(Vec<(wgpu::Limits, wgpu::RequestDeviceError)>),
}

impl From<Error> for graphics::Error {
    fn from(error: Error) -> Self {
        Self::GraphicsAdapterNotFound {
            backend: "wgpu",
            reason: error::Reason::RequestFailed(error.to_string()),
        }
    }
}

/// 呈现耗时统计的取样窗口（帧数）。每这么多帧打一条汇总，避免逐帧刷日志。
const PRESENT_TIMING_WINDOW: u64 = 120;

/// 单调取大。**不能**顺手 `fetch_add`——那会把"最大值"槽变成累加器，报出
/// 一个随窗口单调增长、看起来像卡顿的假数字（第一版就踩了这个坑）。
fn bump_max(counter: &std::sync::atomic::AtomicU64, value: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut max = counter.load(Relaxed);
    while value > max {
        match counter.compare_exchange_weak(max, value, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(actual) => max = actual,
        }
    }
}

/// 记录一次呈现的耗时，满一个窗口就打一条日志并清零。
///
/// 单帧最大值比平均值重要得多：卡顿是**偶发**的，平均值会把它抹平。
fn record_present_timing(submit_us: u64, present_us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static SUBMIT_SUM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static SUBMIT_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static PRESENT_SUM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static PRESENT_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    bump_max(&SUBMIT_MAX, submit_us);
    bump_max(&PRESENT_MAX, present_us);
    let _ = SUBMIT_SUM.fetch_add(submit_us, Relaxed);
    let _ = PRESENT_SUM.fetch_add(present_us, Relaxed);
    let n = FRAMES.fetch_add(1, Relaxed) + 1;
    if n < PRESENT_TIMING_WINDOW {
        return;
    }

    log::info!(
        "[present] n={n} submit avg={:.1}us max={}us | surface avg={:.1}us max={}us",
        SUBMIT_SUM.swap(0, Relaxed) as f64 / PRESENT_TIMING_WINDOW as f64,
        SUBMIT_MAX.swap(0, Relaxed),
        PRESENT_SUM.swap(0, Relaxed) as f64 / PRESENT_TIMING_WINDOW as f64,
        PRESENT_MAX.swap(0, Relaxed),
    );
    FRAMES.store(0, Relaxed);
}

/// 显式指定适配器的环境变量名（见 [`adapter_filter`]）。
const ADAPTER_ENV: &str = "VIEWSTAGE_ADAPTER";

/// 读取显式适配器选择；未设置或为空则返回 `None`（走 wgpu 默认选择）。
fn adapter_filter() -> Option<String> {
    std::env::var(ADAPTER_ENV).ok().filter(|s| !s.trim().is_empty())
}

/// `want` 是否匹配该适配器。支持 `discrete` / `integrated` / `cpu` 三类设备，
/// 或适配器名字的大小写无关子串（如 `mx330`）。
fn adapter_matches(info: &wgpu::AdapterInfo, want: &str) -> bool {
    match want.trim().to_ascii_lowercase().as_str() {
        "discrete" => info.device_type == wgpu::DeviceType::DiscreteGpu,
        "integrated" => info.device_type == wgpu::DeviceType::IntegratedGpu,
        "cpu" => info.device_type == wgpu::DeviceType::Cpu,
        other => info.name.to_ascii_lowercase().contains(other),
    }
}

impl Compositor {
    /// Requests a new [`Compositor`] with the given [`Settings`].
    ///
    /// Returns `None` if no compatible graphics adapter could be found.
    pub async fn request<W: compositor::Window>(
        settings: Settings,
        compatible_window: Option<W>,
        shell: Shell,
    ) -> Result<Self, Error> {
        #[allow(unused_variables)]
        let dcomp = {
            #[cfg(windows)]
            {
                super::dcomp_surface::dcomp_enabled()
            }
            #[cfg(not(windows))]
            {
                false
            }
        };

        // DComp 链路依赖 wgpu DX12 后端（as_hal 拿 ID3D12CommandQueue；
        // Vulkan/GL 后端拿不到）。P0 spike 即显式 DX12。
        // 注意必须在 instance 创建前生效（backends 是 instance 的属性）。
        let mut settings = settings;
        if dcomp {
            settings.backends = wgpu::Backends::DX12;
        }
        let settings = settings;

        let instance = wgpu::util::new_instance_with_webgpu_detection(
            &wgpu::InstanceDescriptor {
                backends: settings.backends,
                flags: if cfg!(feature = "strict-assertions") {
                    wgpu::InstanceFlags::debugging()
                } else {
                    wgpu::InstanceFlags::empty()
                },
                ..Default::default()
            },
        )
        .await;

        log::info!("{settings:#?}");

        #[cfg(not(target_arch = "wasm32"))]
        if log::max_level() >= log::LevelFilter::Info {
            let available_adapters: Vec<_> = instance
                .enumerate_adapters(settings.backends)
                .iter()
                .map(wgpu::Adapter::get_info)
                .collect();
            log::info!("Available adapters: {available_adapters:#?}");
        }

        #[allow(unsafe_code)]
        let compatible_surface = if dcomp {
            // DComp 路径不走经典 surface；adapter 选择不再依赖 surface 兼容性
            None
        } else {
            compatible_window.and_then(|window| instance.create_surface(window).ok())
        };

        let adapter_options = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::from_env()
                .unwrap_or(wgpu::PowerPreference::HighPerformance),
            compatible_surface: compatible_surface.as_ref(),
            force_fallback_adapter: false,
        };

        // 显式指定适配器。`PowerPreference::HighPerformance` 在 Vulkan 上只是
        // 给驱动的一个**提示**（`VK_PHYSICAL_DEVICE_POWER_PREFERENCE_*`），
        // 实测在一台同时有核显与独显的机器上，wgpu 仍然选了核显：
        //
        // ```text
        //   name: "Intel(R) UHD Graphics"  device_type: IntegratedGpu  backend: Vulkan
        //   name: "NVIDIA GeForce MX330"   device_type: DiscreteGpu   backend: Vulkan
        //   Selected: Intel(R) UHD Graphics            ← 独显在列表里却没被选
        // ```
        //
        // 对本应用（摄像头纹理 40MB/s 上传 + 全屏合成 + 直通通道）而言，选错
        // 适配器足以解释"我们只占 0.02ms CPU、却每帧卡 45~83ms"——等待全在
        // GPU 侧。所以必须能显式覆盖并实测，而不是指望驱动尊重提示。
        let adapter = match adapter_filter() {
            Some(want) => {
                let picked = instance
                    .enumerate_adapters(settings.backends)
                    .into_iter()
                    .find(|candidate| adapter_matches(&candidate.get_info(), &want));

                match picked {
                    Some(adapter) => {
                        log::info!(
                            "Adapter override {want:?} -> {:#?}",
                            adapter.get_info()
                        );
                        adapter
                    }
                    None => {
                        log::warn!(
                            "Adapter override {want:?} matched none of the \
                             enumerated adapters; falling back to \
                             PowerPreference selection"
                        );
                        instance.request_adapter(&adapter_options).await.map_err(
                            |_error| {
                                Error::NoAdapterFound(format!("{adapter_options:?}"))
                            },
                        )?
                    }
                }
            }
            None => {
                instance.request_adapter(&adapter_options).await.map_err(
                    |_error| {
                        Error::NoAdapterFound(format!("{adapter_options:?}"))
                    },
                )?
            }
        };

        log::info!("Selected: {:#?}", adapter.get_info());

        let (format, alpha_mode) = if dcomp {
            // 合成交换链固定 Bgra8Unorm + PREMULTIPLIED（P0 spike 验证参数）；
            // iced 渲染管线对非 srgb 目标做同样的 gamma 处理，内容等价。
            (
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::CompositeAlphaMode::PreMultiplied,
            )
        } else {
            compatible_surface
                .as_ref()
                .and_then(|surface| {
                let capabilities = surface.get_capabilities(&adapter);

                let formats = capabilities.formats.iter().copied();

                log::info!("Available formats: {formats:#?}");

                let mut formats = formats.filter(|format| {
                    format.required_features() == wgpu::Features::empty()
                });

                let format = if color::GAMMA_CORRECTION {
                    formats.find(wgpu::TextureFormat::is_srgb)
                } else {
                    formats.find(|format| !wgpu::TextureFormat::is_srgb(format))
                };

                let format = format.or_else(|| {
                    log::warn!("No format found!");

                    capabilities.formats.first().copied()
                });

                let alpha_modes = capabilities.alpha_modes;

                log::info!("Available alpha modes: {alpha_modes:#?}");

                let preferred_alpha = if alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
                {
                    wgpu::CompositeAlphaMode::PostMultiplied
                } else if alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else {
                    wgpu::CompositeAlphaMode::Auto
                };

                format.zip(Some(preferred_alpha))
            })
            .ok_or(Error::IncompatibleSurface)?
        };

        log::info!(
            "Selected format: {format:?} with alpha mode: {alpha_mode:?}"
        );

        #[cfg(target_arch = "wasm32")]
        let limits = [wgpu::Limits::downlevel_webgl2_defaults()
            .using_resolution(adapter.limits())];

        #[cfg(not(target_arch = "wasm32"))]
        let limits =
            [wgpu::Limits::default(), wgpu::Limits::downlevel_defaults()];

        let limits = limits.into_iter().map(|limits| wgpu::Limits {
            max_bind_groups: 2,
            max_non_sampler_bindings: 2048,
            ..limits
        });

        let mut errors = Vec::new();

        for required_limits in limits {
            let result = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some(
                        "iced_wgpu::window::compositor device descriptor",
                    ),
                    required_features: wgpu::Features::empty(),
                    required_limits: required_limits.clone(),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                    trace: wgpu::Trace::Off,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(
                    ),
                })
                .await;

            match result {
                Ok((device, queue)) => {
                    let engine = Engine::new(
                        &adapter,
                        device,
                        queue,
                        format,
                        settings.antialiasing,
                        shell,
                    );

                    return Ok(Compositor {
                        instance,
                        adapter,
                        format,
                        alpha_mode,
                        engine,
                        settings,
                        dcomp,
                    });
                }
                Err(error) => {
                    errors.push((required_limits, error));
                }
            }
        }

        Err(Error::RequestDeviceFailed(errors))
    }
}

/// Creates a [`Compositor`] with the given [`Settings`] and window.
pub async fn new<W: compositor::Window>(
    settings: Settings,
    compatible_window: W,
    shell: Shell,
) -> Result<Compositor, Error> {
    Compositor::request(settings, Some(compatible_window), shell).await
}

/// legacy 路径全局帧序号（viewport 诊断节流用）。
static LEGACY_FRAME_SEQ: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Presents the given primitives with the given [`Compositor`].
#[allow(clippy::too_many_arguments)]
pub fn present(
    renderer: &mut Renderer,
    surface: &mut wgpu::Surface<'static>,
    viewport: &Viewport,
    background_color: Color,
    on_pre_present: impl FnOnce(),
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> Result<(), compositor::SurfaceError> {
    match surface.get_current_texture() {
        Ok(frame) => {
            let view = &frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            // legacy 路径 viewport 诊断（与 DComp 路径同款，节流 300 帧）
            let frame_no = {
                use std::sync::atomic::Ordering;
                let n = LEGACY_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
                if std::env::var("VIEWSTAGE_LEGACY_DEBUG").as_deref() == Ok("1")
                    && n % 300 == 0
                {
                    let phys = viewport.physical_size();
                    log::info!(
                        "viewstage.legacy: frame {n} viewport {}x{} scale {:.3}",
                        phys.width,
                        phys.height,
                        viewport.scale_factor()
                    );
                }
                n
            };

            // 一次性后缓冲读回（取证：渲染端内容逐像素对比）。
            let want_readback = std::env::var("VIEWSTAGE_LEGACY_READBACK")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .map(|n| n == frame_no)
                .unwrap_or(false);

            // ---- 呈现耗时诊断（submit 与 surface present 分开计时）----
            //
            // 为什么必须分开：`App::draw` 的 CPU 只有 0.02~0.45ms，可帧间隔却有
            // 50~83ms 的空洞，两者对不上——说明时间不在我们的代码里。但"在驱动
            // 里"仍是个笼统的说法：`renderer.present()` 是编码 + `queue.submit`
            //（驱动可能在这里等 GPU 排空队列），`frame.present()` 是
            // `get_current_texture` + `surface.present`（可能在这里等一个可用
            // 的 backbuffer）。两者的修法完全不同，必须各自可测。
            let t_submit = std::time::Instant::now();
            let _submission = renderer.present(
                Some(background_color),
                frame.texture.format(),
                view,
                viewport,
            );
            let submit_us = t_submit.elapsed().as_micros() as u64;

            if want_readback {
                super::dcomp_surface::dump_texture_bmp(
                    device,
                    queue,
                    &frame.texture,
                    &format!("tmp-probe/legacy_backbuffer_f{frame_no}.bmp"),
                );
            }
            // 逐帧亮度指纹（VIEWSTAGE_FRAME_STATS=start..end）：把"闪一下"变成
            // 可定位的数值异常。必须在 present **之前**取样，采到的才是即将上屏
            // 的那一帧。
            if super::dcomp_surface::frame_stats_wanted(frame_no as u64) {
                super::dcomp_surface::log_frame_stats(
                    device,
                    queue,
                    &frame.texture,
                    frame_no as u64,
                );
            }

            // Present the frame
            on_pre_present();
            let t_present = std::time::Instant::now();
            frame.present();
            record_present_timing(submit_us, t_present.elapsed().as_micros() as u64);

            Ok(())
        }
        Err(error) => match error {
            wgpu::SurfaceError::Timeout => {
                Err(compositor::SurfaceError::Timeout)
            }
            wgpu::SurfaceError::Outdated => {
                Err(compositor::SurfaceError::Outdated)
            }
            wgpu::SurfaceError::Lost => Err(compositor::SurfaceError::Lost),
            wgpu::SurfaceError::OutOfMemory => {
                Err(compositor::SurfaceError::OutOfMemory)
            }
            wgpu::SurfaceError::Other => Err(compositor::SurfaceError::Other),
        },
    }
}

impl graphics::Compositor for Compositor {
    type Renderer = Renderer;
    type Surface = Surface;

    async fn with_backend(
        settings: graphics::Settings,
        _display: impl compositor::Display,
        compatible_window: impl compositor::Window,
        shell: Shell,
        backend: Option<&str>,
    ) -> Result<Self, graphics::Error> {
        match backend {
            None | Some("wgpu") => {
                let mut settings = Settings::from(settings);

                if let Some(backends) = wgpu::Backends::from_env() {
                    settings.backends = backends;
                }

                if let Some(present_mode) = settings::present_mode_from_env() {
                    settings.present_mode = present_mode;
                    log::info!(
                        "present_mode overridden by ICED_PRESENT_MODE -> \
                         {present_mode:?}"
                    );
                }

                // **生效**的呈现模式。`Compositor::request` 里那条 `{settings:#?}`
                // 打在覆盖**之前**（request 早于 with_backend），所以它显示的是
                // 配置值而非实际生效值——曾据此误判"环境变量没生效"。这里补一条。
                log::info!(
                    "[present] effective present_mode = {:?} \
                     (ICED_PRESENT_MODE={:?})",
                    settings.present_mode,
                    std::env::var("ICED_PRESENT_MODE").ok()
                );

                Ok(new(settings, compatible_window, shell).await?)
            }
            Some(backend) => Err(graphics::Error::GraphicsAdapterNotFound {
                backend: "wgpu",
                reason: error::Reason::DidNotMatch {
                    preferred_backend: backend.to_owned(),
                },
            }),
        }
    }

    fn create_renderer(&self) -> Self::Renderer {
        Renderer::new(
            self.engine.clone(),
            self.settings.default_font,
            self.settings.default_text_size,
        )
    }

    fn create_surface<W: compositor::Window>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Self::Surface {
        #[cfg(windows)]
        if self.dcomp {
            use super::dcomp_surface::{hwnd_of, DcompSurface};
            let hwnd = hwnd_of(&window)
                .expect("DComp surface requires a Win32 window handle");
            let surface = DcompSurface::new(
                hwnd,
                &self.engine.device,
                self.engine.queue.clone(),
                width.max(1),
                height.max(1),
            )
            .expect("Create DComp surface");
            return Surface::Dcomp(surface);
        }

        let mut surface = Surface::Wgpu(
            self.instance.create_surface(window).expect("Create surface"),
        );

        if width > 0 && height > 0 {
            self.configure_surface(&mut surface, width, height);
        }

        surface
    }

    fn configure_surface(
        &mut self,
        surface: &mut Self::Surface,
        width: u32,
        height: u32,
    ) {
        match surface {
            #[cfg(windows)]
            Surface::Dcomp(dcomp_surface) => {
                dcomp_surface.resize(width, height);
            }
            Surface::Wgpu(surface) => {
                surface.configure(
                    &self.engine.device,
                    &wgpu::SurfaceConfiguration {
                        // COPY_SRC：诊断 readback 需要
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::COPY_SRC,
                        format: self.format,
                        present_mode: self.settings.present_mode,
                        width,
                        height,
                        alpha_mode: self.alpha_mode,
                        view_formats: vec![],
                        desired_maximum_frame_latency: 1,
                    },
                );
            }
        }
    }

    fn information(&self) -> compositor::Information {
        let information = self.adapter.get_info();

        compositor::Information {
            adapter: information.name,
            backend: format!("{:?}", information.backend),
        }
    }

    fn present(
        &mut self,
        renderer: &mut Self::Renderer,
        surface: &mut Self::Surface,
        viewport: &Viewport,
        background_color: Color,
        on_pre_present: impl FnOnce(),
    ) -> Result<(), compositor::SurfaceError> {
        match surface {
            #[cfg(windows)]
            Surface::Dcomp(dcomp_surface) => dcomp_surface.present(
                renderer,
                viewport,
                background_color,
                on_pre_present,
            ),
            Surface::Wgpu(surface) => present(
                renderer,
                surface,
                viewport,
                background_color,
                on_pre_present,
                &self.engine.device,
                &self.engine.queue,
            ),
        }
    }

    fn screenshot(
        &mut self,
        renderer: &mut Self::Renderer,
        viewport: &Viewport,
        background_color: Color,
    ) -> Vec<u8> {
        renderer.screenshot(viewport, background_color)
    }
}
