//! 静态层缓存的 blit 着色器：全屏三角形 + 一次纹理采样。
//!
//! 采样**不做解码**：缓存纹理用非 sRGB 格式（surface 格式去掉 sRGB 后缀），
//! 里面存的就是着色器原本要写进帧的编码值，blit 是逐字节精确的拷贝。
//!
//! uv 在 VS 里从裁空坐标算出并作为 varying 传递：这样不需要 uniform 缓冲，
//! 也就没有"窗口尺寸变化时要记得更新 inv_size"这类易漏的旁路。
//! WebGPU 的帧缓冲原点在左上、裁空 y 向上，故 uv 的 y 要翻转。
//!
//! 输出为预乘色，混合状态是 `PREMULTIPLIED_ALPHA_BLENDING`：把「透明底上的
//! 预乘墨迹」按 source-over 贴到帧上，与直接画在同一张帧上等价。

@group(0) @binding(0) var cache: texture_2d<f32>;
@group(0) @binding(1) var cache_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

/// 全屏三角形（3 顶点，比两个三角形的四边形少一条对角线上的重复光栅化）。
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let x = f32((vertex_index << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vertex_index & 2u) * 2.0 - 1.0;
    var out: VertexOutput;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(cache, cache_sampler, input.uv);
}
