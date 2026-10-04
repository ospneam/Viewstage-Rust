//! 专属直通合成通道：宿主自绘 wgpu 管线直通合成器。
//!
//! 完全绕过 iced 的 widget / Scene / primitive 队列：每帧不经 view() 构建
//! 图元快照、不经 `Box<dyn Stored>` 堆分配进层栈、不经 TypeId 查表与逐层
//! 几何重放。宿主在启动早期注册通道；`Renderer::prepare` 阶段直接上传，
//! `Renderer::render` 合成序列最前端把通道内容直接画进最终 frame——通道
//! 构成合成栈的**底层**（摄像头背景 → 批注笔迹），位于全部 iced UI 层与
//! 文字之下，层缓存与 text 通路不受影响。

use crate::graphics::Viewport;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// 直通合成通道：宿主自绘管线的最小接口。
///
/// - [`DirectChannel::init`]：Renderer 就绪后调用（重建 compositor 等场景
///   可能多次调用，实现必须幂等——已持管线时直接返回）。
/// - [`DirectChannel::prepare`]：每帧先于全部 iced 层 prepare 调用，上传
///   几何/纹理、写 uniform；输入直接来自宿主共享状态，不经 widget 层。
///   上传命令应记入传入的**帧 encoder**、暂存数据写经传入的 **staging
///   belt**（与 iced 自身纹理上传同一机制）——随主帧统一 finish/submit/
///   recall，宿主通道不再自建 encoder 提交（减少驱动级 submit 次数，
///   且映射写省一次 CPU 拷贝）。
/// - [`DirectChannel::draw`]：在直通合成 pass 中绘制（与其它通道共享同一
///   pass，按注册顺序叠加；先注册者画在底层）。
///
/// frame 底色**不是**通道：它不画任何东西，用
/// [`set_frame_clear_color`] 单独声明即可。
pub trait DirectChannel: Send + Sync {
    /// 创建持久管线与 GPU 资源（幂等）。
    fn init(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat);

    /// 每帧合成前调用：上传几何/纹理、写 uniform。
    ///
    /// 纹理上传写经 `belt`、拷贝命令记入 `encoder`（frame 主编码器），
    /// 由 Renderer present 统一 `finish` → `submit` → `recall`。
    /// 不要在此自建 encoder 提交，也不要在帧中途 submit 传入的 encoder。
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        viewport: &Viewport,
    );

    /// 在直通合成 pass 中直接画进最终 frame。
    fn draw(&mut self, pass: &mut wgpu::RenderPass<'_>);
}

/// 全局注册表。
///
/// 宿主必须在 `iced_winit::run` 之前注册。`Renderer::new` 时首个 renderer
/// 一次性接管（主窗口 renderer 必然先于任何辅助窗口创建）；`prepare` 阶段
/// 的惰性接管仅作为 compositor 重建的兜底——常态下注册表已空。
fn registry() -> &'static Mutex<Vec<Box<dyn DirectChannel>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Box<dyn DirectChannel>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// frame 底色（`[r, g, b, a]`，framebuffer 原始值域）。
///
/// 底色本质是**渲染器的一帧属性**，不是一条"要画东西的通道"：它不持有
/// GPU 资源、不上传、不绘制，只声明直通合成 pass 的 `LoadOp::Clear`。
/// 早先它被建模成一个 `DirectChannel`（45 行全空实现），代价是每帧多一次
/// 堆分配的对象、两次虚调用（prepare/draw 均 no-op），外加 render 里一次
/// `find_map` 去把它找出来。改为独立的 `AtomicU64` 存储：一次 relaxed
/// load，无分配、无虚调用、无遍历。
static FRAME_CLEAR: AtomicU64 = AtomicU64::new(0);

/// 打包为 8bit/通道 位域。越界与 NaN 必须钳到 [0,1]——位域回绕会
/// 把 -1.0 变成 255（纯白），那是很难查的显示 bug。
#[inline]
fn pack_clear(color: [f32; 4]) -> u64 {
    let pack = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u64;
    (pack(color[0]) << 48) | (pack(color[1]) << 32) | (pack(color[2]) << 16) | pack(color[3])
}

/// 解包位域（`None` = 未声明的哨兵值 0）。
#[inline]
fn unpack_clear(bits: u64) -> Option<[f32; 4]> {
    if bits == 0 {
        return None;
    }
    let unpack = |shift: u32| ((bits >> shift) & 0xFF) as f32 / 255.0;
    Some([unpack(48), unpack(32), unpack(16), unpack(0)])
}

/// 声明 frame 底色。首个/任意一次调用均生效（后写覆盖先写）。
///
/// 该颜色成为直通合成 pass 的 `LoadOp::Clear`——等价于"画一个全屏纯色
/// quad"，但由硬件 clear 完成，省一次整屏 ROP 写入。没有任何直通通道
/// 时，退回主题背景色（见 `Renderer::render` 的 `frame_load`）。
pub fn set_frame_clear_color(color: [f32; 4]) {
    FRAME_CLEAR.store(pack_clear(color), Ordering::Relaxed);
}

/// 读取已声明的 frame 底色（`None` = 未声明）。
#[inline]
pub(crate) fn frame_clear_color() -> Option<[f32; 4]> {
    unpack_clear(FRAME_CLEAR.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 底色经 8bit/通道 位域打包，量化误差必须 ≤ 1/255（否则白板底色会
    /// 出现可见色偏）。
    #[test]
    fn pack_unpack_stays_within_one_quantization_step() {
        // 白板底色 26/255 —— 宿主实际使用的值
        let input = [26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0];
        let got = unpack_clear(pack_clear(input)).expect("非零打包值应解出颜色");
        for (i, actual) in got.iter().enumerate() {
            let error = (actual - input[i]).abs();
            assert!(
                error <= 1.0 / 255.0,
                "通道 {i}: 期望 {}，实得 {actual}，误差 {error} 超过 1/255",
                input[i]
            );
        }
        // 三个颜色通道是同一灰度值，解包后必须逐位相等
        assert_eq!(got[0], got[1]);
        assert_eq!(got[1], got[2]);
        assert_eq!(got[3], 1.0);
    }

    /// 越界/负值/NaN 必须被钳到 [0,1]。位域回绕会把 -1.0 变成 255（纯白），
    /// `NaN as u64` 则是 0——两者都必须在钳制后得到确定值。
    #[test]
    fn pack_clamps_out_of_range_and_nan() {
        assert_eq!(
            unpack_clear(pack_clear([-1.0, 2.0, f32::NAN, 5.0])),
            Some([0.0, 1.0, 0.0, 1.0]),
            "越界与 NaN 必须钳到 [0,1]"
        );
    }

    /// 打包值 0 是"未声明"哨兵 —— 但**只有全透明黑**会撞上它。
    ///
    /// # 更正：本测试原先的前提是错的
    ///
    /// 原文断言 `unpack_clear(pack_clear([0,0,0,1])) == None`，并据此得出
    /// "显式纯黑底不可用，若需支持必须加独立 `AtomicBool`"。那是错的：
    ///
    /// ```text
    /// pack_clear([0,0,0,1]) = 0<<48 | 0<<32 | 0<<16 | pack(1.0) = 255
    /// ```
    ///
    /// alpha 通道贡献了 255，所以打包值非 0，`unpack_clear` 正常返回
    /// `Some([0,0,0,1])`。**显式纯黑底今天就能用**，不需要任何改动。
    ///
    /// 真正与"未声明"同形的只有 `[0,0,0,0]`（全透明黑）——它对 pass 的
    /// `LoadOp::Clear` 毫无意义，所以这个歧义是无害的。
    ///
    /// 记录这一点是因为原注释里的错误结论会误导人：做深色主题时容易以为
    /// "纯黑底被静默吞掉了"而去加一个其实并不需要的标记位。
    #[test]
    fn explicit_opaque_black_is_distinguishable_from_unset() {
        assert_eq!(
            unpack_clear(pack_clear([0.0, 0.0, 0.0, 1.0])),
            Some([0.0, 0.0, 0.0, 1.0]),
            "显式不透明纯黑必须与『未声明』区分得开"
        );
        // 不透明黑能表达 → 任何带 alpha 的颜色（包括黑色）都不会撞上 0 哨兵。
        assert_eq!(
            unpack_clear(pack_clear([0.0, 0.0, 0.0, 0.5])),
            Some([0.0, 0.0, 0.0, 128.0 / 255.0]),
            "半透明黑同样可区分"
        );
        // 唯一与未声明同形的：全透明黑。对 Clear 而言无意义，故接受。
        assert_eq!(
            unpack_clear(pack_clear([0.0, 0.0, 0.0, 0.0])),
            None,
            "只有全透明黑才与哨兵同形——这是唯一且无害的歧义"
        );
        assert!(unpack_clear(pack_clear([1.0, 1.0, 1.0, 1.0])).is_some());
    }

    /// 原子存储本身：写入后可被另一线程读到（relaxed 足够——它只是
    /// 启动期写一次、渲染期只读的一次性配置）。
    #[test]
    fn atomic_roundtrip_is_visible() {
        set_frame_clear_color([26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0]);
        let seen = std::thread::spawn(frame_clear_color)
            .join()
            .expect("读取线程应正常结束");
        assert_eq!(seen, frame_clear_color(), "跨线程必须读到同一底色");
    }
}

/// 非空标志（`AtomicBool`）：`take_registered` 每渲染帧调用一次，常态下
/// 注册表恒空。用一次 relaxed 原子读做快路径，把「每帧一次 `Mutex`
/// lock/unlock」降为「每帧一次 relaxed load」——注册表只在
/// `Renderer::new`（启动期）与 compositor 重建兜底时被清空。
static HAS_PENDING: AtomicBool = AtomicBool::new(false);

/// 注册直通通道（按调用顺序合成：先注册 = 底层）。
pub fn register_direct_channel(channel: Box<dyn DirectChannel>) {
    registry()
        .lock()
        .expect("Direct channel registry poisoned")
        .push(channel);
    HAS_PENDING.store(true, Ordering::Relaxed);
}

/// 取走全部已注册通道（Renderer 创建时与每帧 prepare 惰性兜底时调用）。
///
/// 常态快路径：无待接管通道时直接返回空 `Vec`，**不触碰 `Mutex`**
/// （空 `Vec` 不分配堆内存）。
pub(crate) fn take_registered() -> Vec<Box<dyn DirectChannel>> {
    if !HAS_PENDING.swap(false, Ordering::Relaxed) {
        return Vec::new();
    }

    let mut registry = registry()
        .lock()
        .expect("Direct channel registry poisoned");

    if registry.is_empty() {
        // 理论上不可达（标志与内容由同一把锁保护），防御性复位
        return Vec::new();
    }

    std::mem::take(&mut *registry)
}
