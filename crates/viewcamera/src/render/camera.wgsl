//! 摄像头/图片背景层顶点/片元着色器（纹理四边形，uniform 做画布→NDC 变换）。
//!
//! 两个片段入口共享同一布局（uniform + 双纹理槽 + 采样器）：
//! - fs_main：RGBA 纹理直读（图片导入路径；tex_b 槽位绑定同一视图，不读取）
//! - fs_main_nv12：NV12 直传路径——Y 平面 + UV 平面双采样，
//!   limited-range 展开 + BT.601/709 矩阵选择（brightness_contrast.w），
//!   线性采样顺带完成缩放（替代旧版 CPU 最近邻降采样）。
//! 两者共用亮度/对比度/灰度滤镜。

struct Globals {
    transform: mat4x4<f32>,
    /// 滤镜参数，打包为 vec4：
    /// x = brightness_delta, y = contrast_scale, z = grayscale,
    /// w = YUV 矩阵选择（0=BT.601, 1=BT.709，仅 NV12 路径使用）
    brightness_contrast: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
// tex_a：RGBA 颜色 / NV12 的 Y 平面；tex_b：RGBA 路径占位 / NV12 的 UV 平面
@group(0) @binding(1) var tex_a: texture_2d<f32>;
@group(0) @binding(2) var tex_b: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.position = globals.transform * vec4<f32>(input.position, 0.0, 1.0);
    out.uv = input.uv;
    return out;
}

/// 统一滤镜：灰度 → 亮度/对比度。
///
/// identity 参数（brightness=0, contrast=1, grayscale=0）直接返回原色：
/// uniform 一致分支全像素同路（无发散惩罚），旁路后每像素省约 10 ALU
/// ——摄像头全屏合成的片元 ALU 主导项之一（App 侧 0/100 构造位级精确，
/// 非默认值自然回退原路径）。
fn apply_filter(col: vec4<f32>) -> vec4<f32> {
    let bc = globals.brightness_contrast;
    if bc.x == 0.0 && bc.y == 1.0 && bc.z == 0.0 {
        return col;
    }

    // 灰度：加权公式 R*0.299 + G*0.587 + B*0.114
    let gray = dot(col.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let desaturated = vec3<f32>(gray, gray, gray);
    let grayed = mix(col.rgb, desaturated, vec3<f32>(bc.z));

    // brightness：delta range [-1..1]，0=正常
    // contrast：range [0, 2]，1=正常
    let mid = 0.5;
    let rgb = clamp(
        (grayed - mid) * bc.y + mid + bc.x,
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    return vec4<f32>(rgb, col.a);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return apply_filter(textureSample(tex_a, smp, input.uv));
}

/// NV12 limited-range → RGB。
/// 摄像头 NV12 通常为 limited range（Y∈[16,235]）；UV 半分辨率线性上采样。
/// BT.601/709 按 bc.w 分支选择（≥720p 用 BT.709 高清惯例，标清用 BT.601）：
/// k 由 CPU 位级精确构造（0.0/1.0），一致分支全像素同路，比 4×mix 少
/// 约 8 ALU/px（k∈(0,1) 的插值场景不存在，非 1.0 一律按 601 系数）。
@fragment
fn fs_main_nv12(input: VertexOutput) -> @location(0) vec4<f32> {
    let y_raw = textureSample(tex_a, smp, input.uv).r;
    let chroma = textureSample(tex_b, smp, input.uv).rg - vec2<f32>(0.5, 0.5);

    // limited range 展开：(y - 16/255) * (255/219)
    let y = (y_raw - 0.062745098) * 1.164383562;

    // 系数：[R 对 V, G 对 U, G 对 V, B 对 U]
    let k = globals.brightness_contrast.w;
    var kr: f32;
    var gu: f32;
    var gv: f32;
    var kb: f32;
    if k == 1.0 {
        kr = 1.5748;
        gu = -0.187324;
        gv = -0.468124;
        kb = 1.8556;
    } else {
        kr = 1.402;
        gu = -0.344136;
        gv = -0.714136;
        kb = 1.772;
    }

    let r = y + kr * chroma.y;
    let g = y + gu * chroma.x + gv * chroma.y;
    let b = y + kb * chroma.x;

    return apply_filter(vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
