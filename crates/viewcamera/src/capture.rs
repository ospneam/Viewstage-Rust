use std::time::Duration;

use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, RequestedFormat,
    RequestedFormatType, Resolution,
};
use nokhwa::Camera;

/// 摄像头管理器
///
/// 双路径取帧：
/// - NV12 直传（默认优先）：`frame()` 返回 Media Foundation 原生 NV12 采样
///   （后端 `MF_READWRITE_DISABLE_CONVERTERS=true`，无驱动/软件转换），
///   字节以 `bytes::Bytes` 引用计数持有，可零拷贝切片后跨线程上传 GPU。
///   CPU 每帧像素级工作为零（YUV→RGB 由渲染管线着色器完成）。
/// - RGB 回退：设备仅提供 MJPEG 等格式时，走 nokhwa 软件解码为 RGB，
///   再由上层 CpuFrameProcessor 转换（旧行为）。
pub struct CameraManager {
    camera: Option<Camera>,
    is_open: bool,
    /// 当前流是否输出原生 NV12
    native_nv12: bool,
    /// 可复用的 RGB 解码缓冲（回退路径，避免每帧分配）
    decode_buf: Vec<u8>,
    /// 协商到的帧率（设备**声称**的能力，取所选格式/分辨率下的最大值）。
    negotiated_fps: u32,
    /// 实际投递周期的滚动样本（升序维护，中位数即 [`Self::observed_period`]）。
    ///
    /// # 为什么必须实测，不能只信 `negotiated_fps`
    ///
    /// `compatible_camera_formats()` 报的是设备**声称**的最大帧率。本机实测：
    ///
    /// ```text
    /// 协商：NV12 1280x720@30
    /// MF 源端时间戳间隔：min 31.6  中位 47.6  p95 47.9  max 48.1 ms
    /// ```
    ///
    /// 设备以 **≈21fps** 稳定输出，而非声称的 30fps，而且**极其规律**（p95 与
    /// 中位仅差 0.3ms）。按声称值定节拍会一直比实际快：采集线程以 60Hz 空转
    /// poll（每次都阻塞等样本），显示侧按 33ms 网格取帧——于是"每 33ms 显示
    /// 一次、实际每 47.6ms 才有一张新画面"，观感是**隔一帧才动一下**。
    source_periods: std::collections::VecDeque<Duration>,
    /// 上一个源端时间戳（`VecDeque` 存的是间隔，不是时间戳本身）。
    last_source_ts: Option<Duration>,
}

/// 滚动样本数：约 3 秒 @20fps。够短到能跟上设备节拍变化，够长到不被单帧抖动带偏。
const SOURCE_PERIOD_SAMPLES: usize = 64;

impl CameraManager {
    pub fn new() -> Self {
        Self {
            camera: None,
            is_open: false,
            native_nv12: false,
            decode_buf: Vec::new(),
            negotiated_fps: 0,
            source_periods: std::collections::VecDeque::with_capacity(SOURCE_PERIOD_SAMPLES),
            last_source_ts: None,
        }
    }

    /// 设备**声称**的最大帧率（协商值）。样本不足时作为帧率的唯一依据。
    pub fn negotiated_fps(&self) -> u32 {
        self.negotiated_fps
    }

    /// 设备**实际**的投递周期（中位数）。
    ///
    /// 样本不足 [`SOURCE_PERIOD_SAMPLES`] 的一半时返回 `None` —— 宁可让调用方
    /// 退回协商值，也不要用三五个样本的结论去改节拍（中位数在极少样本下会被
    /// 单帧抖动主导）。
    pub fn observed_period(&self) -> Option<Duration> {
        self.source_periods.get(self.source_periods.len() / 2).copied().filter(|_| {
            self.source_periods.len() >= SOURCE_PERIOD_SAMPLES / 2
        })
    }

    /// 采集节拍：取"请求值"与"实测周期"中**较慢**的那个。
    ///
    /// 设备慢于请求 → 以实测为准，否则 `poll` 每轮都阻塞等样本，纯空转；
    /// 请求慢于设备 → 以请求为准，那是用户主动要的降帧（如"低帧率"省电）。
    pub fn pacing_period(&self, requested: Duration) -> Duration {
        match self.observed_period() {
            Some(observed) => observed.max(requested),
            None => requested,
        }
    }

    /// 记一个源端间隔样本（保持 `VecDeque` **升序**，便于取中位数）。
    ///
    /// 用**中位数**而不是均值：源端偶发的一两帧长间隔（设备瞬时卡顿）会把均值
    /// 拉高一截，而中位数几乎不受影响——节拍要的是"常态"，不是"平均"。
    fn note_source_period(&mut self, ts: Duration) {
        let Some(previous) = self.last_source_ts.replace(ts) else {
            // 首样本只锚定基准，不构成间隔
            return;
        };
        // 先比再减：`Duration` 的 `Sub` 在下溢时 panic，而源端时间戳回退
        // （设备重置 / 换流）是真实会发生的，直接 `checked_sub` 反而把一个
        // 应当"丢弃样本"的情况升级成 panic。
        if ts <= previous {
            return;
        }
        let delta = ts - previous;
        // 剔除明显不是"帧间隔"的值：<1ms（重复时间戳）与 >1s（停流/长停顿）。
        if delta < Duration::from_millis(1) || delta > Duration::from_secs(1) {
            return;
        }
        let at = self.source_periods.partition_point(|&d| d <= delta);
        self.source_periods.insert(at, delta);
        while self.source_periods.len() > SOURCE_PERIOD_SAMPLES {
            self.source_periods.pop_front();
        }
    }

    /// 枚举可用摄像头设备
    pub fn enumerate_devices(&mut self) -> Vec<CameraInfo> {
        nokhwa::query(ApiBackend::Auto).unwrap_or_default()
    }

    /// 打开指定摄像头
    ///
    /// 格式协商阶梯：先按默认请求打开并读取设备原生媒体类型表；若目标分辨率上存在
    /// NV12 且其帧率不低于 MJPEG，则以精确格式重新打开为 NV12（下游直传管线接管，
    /// 免掉每帧软件解码）；条件不满足或重开失败时保持默认协商结果走 RGB 回退路径。
    pub fn open(&mut self, device_index: usize, width: u32, height: u32) -> Result<(), String> {
        if self.is_open {
            self.close();
        }

        let index = CameraIndex::Index(device_index as u32);
        let target = Resolution::new(width, height);

        let format =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
        let mut camera =
            Camera::new(index.clone(), format).map_err(|e| format!("Failed to open camera: {}", e))?;

        let _ = camera.set_resolution(target);

        // 设备原生媒体类型表（compatible_fourcc 只返回前 2 个格式，不能用于能力判断）
        let native = camera.compatible_camera_formats().unwrap_or_default();
        let best_frame_rate = |wanted: FrameFormat| {
            native
                .iter()
                .filter(|entry| entry.format() == wanted && entry.resolution() == target)
                .map(CameraFormat::frame_rate)
                .max()
                .unwrap_or(0)
        };
        let (nv12_fps, mjpeg_fps) = (
            best_frame_rate(FrameFormat::NV12),
            best_frame_rate(FrameFormat::MJPEG),
        );

        if nv12_fps > 0 && nv12_fps >= mjpeg_fps {
            // 后置 set_frame_format 在此后端不可用：它复用缓存的当前格式，而当前帧率被
            // 误读为 1（取 MF_MT_FRAME_RATE 低 32 位即分母），导致匹配不到任何原生媒体
            // 类型；因此必须在打开时就以精确格式协商。
            let request = RequestedFormat::with_formats(
                RequestedFormatType::Exact(CameraFormat::new(target, FrameFormat::NV12, nv12_fps)),
                &[FrameFormat::NV12],
            );
            match Camera::new(index, request) {
                Ok(nv12_camera) => {
                    log::info!("Negotiated native NV12 {width}x{height}@{nv12_fps}");
                    camera = nv12_camera;
                }
                Err(e) => log::warn!("NV12 negotiation rejected ({}), keeping RGB fallback", e),
            }
        }

        camera
            .open_stream()
            .map_err(|e| format!("Failed to open camera stream: {}", e))?;

        let actual = camera.frame_format();
        self.native_nv12 = actual == FrameFormat::NV12;
        log::info!(
            "Camera stream format: {:?} ({})",
            actual,
            if self.native_nv12 {
                "zero-copy direct upload path"
            } else {
                "RGB decode fallback path"
            }
        );

        self.camera = Some(camera);
        self.is_open = true;
        // 记下**声称**的帧率。它只作为实测前的初值，以及实测样本不足时的兜底。
        self.negotiated_fps = nv12_fps;
        self.source_periods.clear();
        Ok(())
    }

    /// 关闭摄像头
    pub fn close(&mut self) {
        if let Some(mut camera) = self.camera.take() {
            let _ = camera.stop_stream();
        }
        self.is_open = false;
        self.native_nv12 = false;
    }

    /// 当前流是否为原生 NV12（决定上层走直传还是回退路径）。
    pub fn is_native_nv12(&self) -> bool {
        self.native_nv12
    }

    /// 【NV12 路径】取最新原始帧（零转换、零拷贝转移所有权）。
    /// 返回 (NV12 字节, 宽, 高)；布局 = Y 平面(w*h) + 交错 UV 平面(w*h/2)。
    /// 长度不符（驱动行填充等）时返回 None 并自动降级到 RGB 路径。
    pub fn poll_frame_nv12(&mut self) -> Option<(bytes::Bytes, u32, u32, Option<Duration>)> {
        let camera = self.camera.as_mut()?;
        let frame = camera.frame().ok()?;
        let w = frame.resolution().width();
        let h = frame.resolution().height();
        // **源端时间戳**：MediaFoundation 给的采集时刻。`nokhwa` 把它从
        // `raw_bytes()` 的 `capture_ts` 透传到 `Buffer::capture_timestamp()`
        // （见 nokhwa-0.10.11 `backends/capture/msmf_backend.rs`）。
        //
        // 这是判别"抖动来自哪里"的唯一硬证据。对比**源端间隔**与**我们收到帧
        // 的间隔**：
        //
        // - 源端均匀 33.3ms、我们收到 47ms → **是我们丢了样本**（MF 侧堆积 /
        //   消费不及时），解法在采集管线；
        // - 源端本身就是 47ms → 设备没兑现协商的 30fps，解法在设备/驱动，
        //   我们这边再优化也没用。
        //
        // 没有这个数就只能猜，而猜错方向会白改很久。
        let capture_ts = frame.capture_timestamp();
        if let Some(ts) = capture_ts {
            self.note_source_period(ts);
        }
        if frame.source_frame_format() != FrameFormat::NV12 {
            // 媒体类型被静默切换：清标志让上层下一帧改走 RGB 路径，避免持续空帧
            self.native_nv12 = false;
            return None;
        }
        let expected = (w as usize) * (h as usize) * 3 / 2;
        let data = frame.buffer_bytes();
        if data.len() != expected || w % 2 != 0 || h % 2 != 0 {
            log::warn!(
                "NV12 frame size mismatch (got {}, expected {}); falling back to RGB path",
                data.len(),
                expected
            );
            self.native_nv12 = false;
            return None;
        }
        Some((data, w, h, capture_ts))
    }

    /// 【RGB 回退路径】获取最新帧并解码为 RGB，写入可复用缓冲（避免每帧分配）。
    /// 返回 (宽, 高)，像素数据通过 [`Self::frame_rgb`] 读取。
    pub fn poll_frame(&mut self) -> Option<(u32, u32)> {
        let camera = self.camera.as_mut()?;
        let frame = camera.frame().ok()?;
        let w = frame.resolution().width();
        let h = frame.resolution().height();
        let needed = (w as usize) * (h as usize) * 3;
        self.decode_buf.resize(needed, 0);
        frame
            .decode_image_to_buffer::<RgbFormat>(&mut self.decode_buf)
            .ok()?;
        Some((w, h))
    }

    /// 最近一次 [`Self::poll_frame`] 解码出的 RGB 数据。
    pub fn frame_rgb(&self) -> &[u8] {
        &self.decode_buf
    }
}

impl Drop for CameraManager {
    fn drop(&mut self) {
        self.close();
    }
}
