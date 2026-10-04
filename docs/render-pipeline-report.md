# ViewStage 渲染管线技术报告与整改记录

> 环境：Linux 静态审查，**未编译、未运行**（目标平台 Windows + DX12，无 cargo/rustc）。
> 审查范围：`src/**`、`crates/{viewink,viewcamera,viewgfx,viewcore-touch,viewicon,viewcomp}/**`、
> `iced-fork/iced_{wgpu,winit,runtime,graphics}/**` 中与渲染相关的全部代码，
> 以及 `tmp-probe/` 内真机 `[rstat]` 实测日志。

---

## 1. 现有架构（结论性描述）

### 1.1 合成栈

每帧由 `iced_wgpu::Renderer::render` 组装，自底向上：

| 顺序 | 内容 | 实现位置 |
| --- | --- | --- |
| 0 | 底色 `(26,26,26)` | `BackdropChannel`（`clear_color()` → `LoadOp::Clear`，零 draw） |
| 1 | 摄像头 / 图片背景 | `CameraChannel`（直通通道） |
| 2 | 批注笔迹 | `AnnotationLayer`（直通通道） |
| 3 | iced UI 层栈 | 上游原版 quad / triangle / image / text 通路 |

直通通道（`iced_wgpu::DirectChannel`）绕过 `view()` → `Scene` → `Box<dyn Stored>` → primitive 队列，
在 `Renderer::prepare` 阶段直读宿主共享状态、在 `render` 阶段画进最终 frame。
这是本项目性能的核心设计，**保留**。

### 1.2 批注层数据模型

CPU 只产出**线段表**（`StrokeSeg`，64B/段：两端点+半宽+两端直通色），
四边形扩展、圆头关节、端帽、SDF 抗锯齿全部由 `annotation.wgsl` 在 GPU 完成。
三个上传槽：

| 槽 | 内容 | 更新时机 | 机制 |
| --- | --- | --- | --- |
| `static_slot` | 已晾干笔画 | 快照代数变化 | 单 grow-only storage 缓冲 + 视口剔除 + 最长公共前缀增量追加 |
| `dynamic_slot` | 进行中笔画（满批块 + 尾块）+ 触控预测轨迹 | 每追加点 | 两层上传（稳定层 ptr 增量 / 尾层每帧重传） |
| `cursor_slot` | 橡皮擦悬停光标 | 悬停移动 | 指纹门控独立小上传 |

### 1.3 输入路径

`sys::touch` 的 `WH_CALLWNDPROC` hook 捕获 `WM_POINTER*` / 鼠标消息 →
写入 `Mutex` 队列 + `InvalidateRect` → winit `RedrawRequested` →
`canvas::Program::update` 内 `drain_touch_zero_msg()` **零消息**直写 `LiveCanvasState` →
`direct` 通道 `prepare` 上传。收笔 finalize 因需 `&mut` 延迟为 `TouchFinalize` 消息（每笔一次）。

实测 `view_avg=0.02ms`：UI widget 树在书写期间完全不重建。**这条链路正确，未改动。**

---

## 2. 稳态每帧成本模型

设 `V`=可见静态段数，`T`=动态段数，`P`=全屏物理像素。

| 项 | CPU | GPU 片段 | 显存带宽 |
| --- | --- | --- | --- |
| 背板 clear | 0 | 0 | `P` 写 |
| 摄像头/图片 quad | O(1) 指纹 | ≈`P` | `P` 读+写 |
| 静态笔迹几何重放 | O(1) | ≈`V` 个胶囊四边形 | 仅覆盖像素写 |
| 动态批 | O(T) 打包 | ≈`T` | 写 `T×64` |
| iced 层（上游原版） | O(层数) | 仅 quad 覆盖 | 仅覆盖像素写 |

---

## 3. 发现的问题与整改

### P1-1 静态笔迹「瓦片光栅缓存」退化为全屏 blit，且并非更优　→ 已删除

`annotation.rs` 原先有一个 `StaticRasterCache`：把静态笔迹离屏光栅化进一张与
surface 同尺寸的纹理，draw 阶段对「含笔迹瓦片」逐实例 blit。

真机日志（`tmp-probe/annot_verify3.log`，1280×720，笔迹静止）：

```
ink_tiles content=15 blit=450 pass=0 up=30      # 15 瓦片/帧 × 30fps = 450
anno_up static=2(26880B) raster=2/0
```

三个事实：

1. **覆盖集 = 全部 15 个瓦片**（5×3 网格全命中）。笔迹 AABB 一旦横跨页面就铺满瓦片网格，
   "空白瓦片零成本"的前提在本场景下不成立——blit 成本 = 全屏。
2. **增量光栅化从未发生**：`raster=2/0`（2 次全量、0 次增量）、`pass=0`
   （按瓦片 `LoadOp::Load` 的脏瓦片 pass 从未触发）。该机制最核心的省带宽设计未生效。
3. **每次全量光栅化消耗一次独立 `queue.submit`**（实测 `own_submit=2`）。

代价对比（720p）：

| | 片段数 | 带宽 |
| --- | --- | --- |
| blit（退化后） | 921,600（≈`P`） | 全屏纹理读 3.7MB + 写 3.7MB |
| 几何重放 | ≈`V`（稀疏细线覆盖面积），无纹理读 | 仅覆盖像素写 |

笔迹是细长胶囊，`merge` 后段 AABB 紧凑；把稀疏细线换成全屏纹理搬运是净损失。

**整改**：删除 `StaticRasterCache` 全套（纹理 / blit 管线 / 瓦片集 / 光栅状态机 /
`RasterAction` 枚举 / 瓦片辅助函数 / `annotation_blit.wgsl` / `blit_enabled` 开关），
回到几何重放。保留视口剔除、前缀增量、单缓冲单 draw。
净减少：一张全屏纹理（4K ≈ 33MB）、每帧 1 次全屏采样、每次光栅化 1 次 submit、约 700 行。

### P1-2 iced fork 的「层缓存」在本应用中永不命中　→ 已删除

`iced_wgpu::layer_cache` 对每层 quad/triangle 批次做内容指纹，命中则跳过 prepare
并 blit 层纹理。

`fingerprint_layer` 的可缓存条件是 `layer.images.is_empty() && layer.primitives.is_empty()`。
本应用 UI 层**必然含 image**：工具栏按钮图标全部是 `views::icons::icon()` 产出的
`image` widget → `Layer::images` 非空 → 判定不可缓存 → `DrawPlain`。
摄像头/白板已迁到直通通道，不再进 `layer.primitives`。

即**每层都走 `DrawPlain`，层缓存 100% 空转**，只留下每帧对所有层 quad/mesh 字节的哈希开销。

**整改**：删除 `layer_cache.rs`（486 行）、`layer_blit.rs`（117 行）、`layer_blit.wgsl`、
`tests/direct.rs`（测已删机制），以及 `Renderer` 中 `layer_cache` / `blit` 字段、
`prepare` 的 fingerprint 分支、`render` 的缓存合成路径（约 290 行）。
连带删除 `quad::Batch::fingerprint`、`quad::State::skip_prepare`、
`triangle::State::skip_prepare`、`triangle::Pipeline::has_msaa`（仅层缓存使用）。
`prepare` / `render` 回到上游原版语义，保留 direct 通道与 `clear_color`。

### P1-3 每帧多次 `Queue::write_buffer` ⇒ 每帧多次独立 submit　→ 已改为 belt

wgpu 的 `Queue::write_buffer` 内部会创建临时 staging 缓冲并**单独 submit 一次**
（这正是 `padded_plane.rs` 注释里「消除 write_buffer 内部再拷贝的双拷贝路径」要绕开的东西）。

但 `annotation.rs` 与 `camera.rs` 仍在用：

| 位置 | 频率 |
| --- | --- |
| `prepare` 变换 uniform（annotation / camera） | pan/zoom 时每帧 |
| `upload_static` 全量 / 前缀增量 | 每次快照代数变化 |
| `upload_dynamic` 尾层 | **书写时每帧** |
| `upload_cursor` | 橡皮擦悬停每帧 |
| camera `quad_vertex` | 视口变化时 |

`DirectChannel::prepare` 已经把帧 encoder 与 `StagingBelt` 传进来（签名里此前写成
`_encoder` / `_belt`）。

**整改**：新增 `viewink::gpu::write_buffer_belt`（单点封装，含 4 字节对齐
`debug_assert`），把上述 6 处写入全部改为经 belt 记录进帧 encoder，
随 iced 主帧 `finish → submit → recall` 统一提交。稳态每帧 submit 数归一为 1。
管线初始化时的一次性写入保留 `queue.write_buffer`（无 encoder 可用，且非热路径）。

### P2-1 相机帧每帧做一次全缓冲 alpha 扫描　→ 已修正

`SingleTextureGpu::upload` 对每次内容变化都跑 `scan_opaque(rgba)`
（`chunks_exact(4).all(...)`，O(字节数)）。RGB 回退路径下每帧 854×480×4 ≈ 1.6MB、
60fps ≈ 100MB/s 的纯 CPU 扫描，而该路径 alpha 由 `CpuFrameProcessor` 恒置 255。
NV12 路径不用该字段（`pipeline_nv12` 固定 `REPLACE`，`fs_main_nv12` 输出 alpha=1.0）。

**整改**：`gpu_image_upload` / `SingleTextureGpu::upload` 增加 `scan_alpha` 参数。
图片槽传 `true`（可能带透明像素，需选 ALPHA/REPLACE 管线），相机槽传 `false`。

### P2-2 逐帧诊断心跳　→ 已删除

`AnnotationLayer::prepare` 与 `CameraChannel::prepare` 各自每帧一次
`AtomicU64::fetch_add` + `% 300` 取模。属于调试脚手架，常态零收益。已删除。
`VIEWSTAGE_*` 环境开关保留（纯 env 读取，无逐帧成本）。

### P2-3 死代码与死成员　→ 已删除

| 项 | 依据 |
| --- | --- |
| `crates/spike-dcomp`（1237 行）、`crates/dcomp-rs-min`（225 行） | P0 调研 spike，已被 `viewcomp` 取代；仅作 workspace member 白占编译时间。已移出 `members` |
| `crates/viewcore-touch/src/batch.rs`（185 行 `EventBatch`） | 全仓库零引用；压缩逻辑已在 `sys::touch::record_event` 中实现 |
| `viewink::stats` 的 `tile_*` / `raster_*` / `own_submit` / `iced_layer_*` 计数 | 随 P1-1/P1-2 失效 |
| `.gitignore` 只忽略 `/target` | `tmp-probe/`（64 个探针产物）易被误提交，已加入忽略 |
| `iced_graphics::mesh::vertex_bytes` 的注释引用 "layer cache" | 已改写为通用表述（方法本身保留，`Pod` 转换仍有用） |

### 保留不动（有意为之）

- `CameraLayer::fit_rect` 的 0.9 适配、`quad_uv_rotated` 的 UV 重排：与旧 `CameraCanvas` 逐位一致。
- `MipGenerator`：仅静态图片一次性生成，相机帧显式传 `None`。
- `viewink::gpu::create_write_buffer`：相机 `quad_vertex` / `quad_index` 仍在用。
- `annotation_fps_cap` 节流：书写中刻意不节流（`paced_redraw` 已注明理由）。
- `diag_no_camera`（`VIEWSTAGE_NO_CAMERA=1`）：真机分层定位 GPU 占用仍有价值。

### P2-4 静态快照每次重建都重扫全部段表（累计 O(n²)）　→ 已修复

`StaticStrokeRef::new` 内部调 `geometry.bounds()` 扫全部段算 AABB。`bounds()` 的文档
写着"每笔只算一次并随引用缓存，不进入帧热路径"——**但这条不变量从未成立**：

```
refresh_static_snapshot（每次 mesh cache 代数变化，即每加一笔）
  └─ 对缓存里的**每一笔**调 StaticStrokeRef::new → bounds() 扫该笔全部段
```

于是每加一笔要重扫**所有**笔画的全部段，整场书写累计 O(n²)，且发生在主线程
`view()` 内。1000 笔 × ~100 段 = 每加一笔扫 100k 段。

**整改**：AABB 改为与几何**同代**产出并随缓存条目存放（`viewgfx::CachedMesh::bounds`
+ `CachedMesh::new` 构造器），快照构建改用 `StaticStrokeRef::with_bounds`（O(1) 取用）。
`StaticStrokeRef::new`（现场扫描）保留但明确标注仅供一次性/测试场景。

### P2-5 批量重算时橡皮擦索引被重建 N 次　→ 已修复

`tessellate_stroke_canvas` 每次调用都从零 `EraserIndex::build(...)`。单笔重算没问题，
但两处批量路径会被放大成主导开销：

| 批量路径 | 旧复杂度 |
| --- | --- |
| `rebuild_stroke_mesh_cache`（切换信号源恢复） | O(笔画数 × 橡皮擦数 × 橡皮擦点数) |
| `retessellate_affected_by_eraser`（橡皮擦撤销） | 同上（对每条相交笔画重建一遍） |

索引构建（散列所有橡皮擦段到网格）比单笔镶嵌本身还贵，于是"批量重算"里
**索引构建反客为主**。

**整改**：新增 `build_eraser_index` + `tessellate_stroke_indexed`，批量路径建一次索引
供全部笔画复用。橡皮擦集不变时索引结果完全确定，复用不改变任何输出——配了 4 个
等价性测试逐段断言（含空索引快路径、`current_eraser` 参与、橡皮擦笔画本身不产几何）。

### P2-6 直通通道注册表每帧一次 Mutex　→ 已优化

`direct::take_registered()` 每渲染帧调用（惰性接管兜底），常态下注册表恒空，
却是每帧一次 `Mutex` lock/unlock + `drain().collect()`。加 `AtomicBool` 非空标志，
常态走一次 relaxed `swap` 直接返回空 `Vec`，完全不触碰 `Mutex`。

### P2-7 空 iced 层仍开一个零 draw 的 render pass　→ 已优化

`Renderer::prepare` / `render` 的层循环对**空层**（无 quad/triangle/primitive/image/text）
照常开 render pass 并置位 `frame_initialized`。两处均加 `layer.is_empty()` 提前跳过。

等价性论证（不是想当然）：空层的各批次皆空，因此各游标
（`quad_layer` / `mesh_layer` / `text_layer` / `image_layer`）在原实现里也不会推进；
`frame_initialized` 保持 false 后由后续真实层或末尾的兜底 clear 承担——清屏语义不变。

### P2-8 撤销时的权威列表不一致　→ 已修复（正确性）

`history.undo` 的 `strokes` 参数名是 `_strokes`（未使用）：撤销绘制只删渲染缓存，
**不动权威列表 `self.strokes`**。两处后果：

1. 任何全量重建（切换信号源恢复）会把撤销的笔迹捞回来；
2. 更直接：后续任何橡皮擦操作的 targeted rebuild 遍历 `self.strokes` 给它发重算任务，
   后台结果回来后判定通过 → **撤销的笔迹在用过橡皮擦后复活**。

**整改**：`undo` 就地改写权威列表（`rposition` 移除最后同 id 项，保证后进先出的
绘制顺序），返回值只用于定向缓存失效。配 7 个单元测试（含 id 重复、id 缺失、
栈裁剪区间的数值模拟验证）。

### P2-9 尾部块每个输入点两次堆分配（"稳态零分配"名不副实）　→ 已修复

`refresh_live_geometry` 是**全局最热的 CPU 路径**——每个输入点执行一次
（1000Hz 鼠标 / 240Hz 触屏）。旧实现每点做两件事：

```
tessellate_live_range_into → Vec::with_capacity(...)   // ① 新建段缓冲
                          .map(Arc::new)                // ② 新建 Arc 头
```

函数文档写着"稳态**零分配、零 realloc**"——**这句话只对 `scratch`（采样点
缓冲）成立，段缓冲从来都在分配**。240Hz 触屏上是 ~480 次/秒。

**整改**：新增 `tessellate_live_range_segs`（写入调用方持有的 `out`，只
`clear` 不新建）；`refresh_live_geometry` 先摘下上一帧的 head，用
`Arc::try_unwrap` 收回底层 `Vec`（渲染侧不持有时 refcount 恒为 1）再复用。
回收失败（渲染侧仍持有）不是错误，退回一次性新缓冲，行为与旧实现一致。

注意区分：只有**会被整体替换**的短命缓冲能这样回收；追加式累积的
`chunks`（满批块要随笔画长期持有）必须每块独立分配。

### P2-10 橡皮擦轨迹每点 O(轨迹总长) 重建　→ 已修复

橡皮擦预览轨迹的增量缓存同样是每追加点就跑一次，但旧实现的做法是：

```
cached_pts > 0 时：构造临时 DrawStroke（含 points / variable_widths 两个 Vec）
                  → 对整条轨迹 tessellate_eraser_trail
```

即**每点 O(轨迹总长)** + 2 个临时 `Vec`。长轨迹橡皮擦会明显变慢。

**整改**：改为分块追加——每个追加点只镶嵌新增的段成一个新块 push 进
`eraser_chunks`，每点成本 O(新增点数)。

块边界处有个必须说清的细节。段 `i` = 「点 i → 点 i+1」，`i ∈ [0, n-2]`。
上一块发射到段 `cached-2` 为止，**段 `cached-1`（跨边界那条）尚未发射**，
所以本块必须从它开始：

```
if cached > 0 { emit(seg[cached-1]) }      // 跨边界的段，上一块没发过
for i in cached..current-1 { emit(seg[i]) }
```

我第一版写成"跳过桥接段"，用 Python 模拟序列后发现拼接结果与整条镶嵌
**不等价**（少了每块边界的那一段，笔迹会断格）。改为上述形式后模拟通过：
分块序列与整条序列逐项相同，零宽跳过场景也相同。

顺带修了一个隐患：轨迹被截短时（`current_pts < cached_pts`）旧实现两个分支
都不进，块缓存不重置 → 留下残段。现在显式清空重建。

颜色常量提升为 `mesh::ERASER_TRAIL_COLOR` 供两条路径共用——分块拼接处在
动态层同一位置，颜色不一致会看到色差。

配了 4 个测试：拼接结果 == 整条镶嵌（逐段断言）、块数按点递增（增量语义
判据）、截短后重置、零宽跳过。

### P2-11 快照双份分配　→ 已评估，决定不改

`refresh_static_snapshot` 先填 `scratch` 再 `Arc::from(scratch.as_slice())` 全量拷贝
成新 `Arc`。每次缓存代数变化（每加一笔）一次 O(n) 分配 + 拷贝，n 为全量笔画数。
绝对量不大且不在帧热路径，**不改**——改动需要引入"可变 Arc"或手工引用计数管理，
收益不足以换取正确性风险。

---

## 3.5 笔迹算法本身的深度优化（第五轮）

前四轮都在管线与数据流层面。这一轮直接进笔迹算法，先建立成本模型再动手。

**每帧 GPU 成本 = 上传的段数 × 6 顶点**（`annotation.rs:881` 一次
`draw(0..6, 0..seg_count)`，视口剔除在上传期做，`collect_static_segs` 逐笔
AABB）。所以批注开销的杠杆只有一个：**段数**。而段数由两个数相乘决定：

```
段数 = 采样点数 × (1 / 合并率)
```

晾干路径早就做了自适应胶囊合并（直行段 30:1），**直播路径完全没做**。

### P2-12 曲线求值：段级不变量被逐采样点重算　→ 已修复

`catmull_rom` 每个采样点都重算 `d0/d1/d2`（3 次 `hypot` + 3 次 `sqrt`），
而这三个值**只由四个控制点决定，与段内参数 t 无关**。一段要采样 `k_sub`
（最多 8）次，等于把同一组常量算 8 遍，其中 3 遍是 `hypot`——溢出安全实现，
比 `sqrt` 慢一个数量级。

> 顺带澄清一个容易误判的点：`.hypot(...).sqrt()` **不是笔误**。向心
> Catmull-Rom（α=0.5）的节点间距定义就是 `|ΔP|^0.5`，`hypot` 给出 `|ΔP|`
> 再开方恰好得到 `|ΔP|^0.5`。我一度准备把它当 bug 修，改前用数值推演核对
> 才发现定义如此——已在代码注释里写明，免得后人"顺手修掉"。

**整改**：抽 `CurveCtx`，把节点时间、五个 `crw` 跨度及其退化判定全部提到
段级；采样循环内只剩纯多项式求值，**零超越函数**。一段的开销从
`4 + 3·k_sub` 次 `hypot` 降到 4 次（含调用方要的段长）。

**逐位等价**：跨度仍是同样两个 f32 相减（`t1 - t0` 等），退化判据仍是
`span.abs() < 1e-6`，除法与插值顺序一字未动。测试里放了旧 `catmull_rom`
的**逐字副本**做对照，覆盖 9 组控制点（含 `p1==p2` / `p0==p1` / `p2==p3` /
三点重合 / 间距极不均匀 / 尖角回折 / 负大坐标）× 8 个采样点，断言
`assert_eq!`（不是近似）。

### P2-13 直播满批块未做胶囊合并 ⇒ 长笔画动态层实例数约为晾干后的 10 倍　→ 已修复

`tessellate_live_range_into`（`chunks` 路径）逐采样点发射胶囊，而晾干路径
`append_stroke` 走 `emit_merged` 贪心合并。一条 80 点笔画的实测压缩比
**10.7:1**（139 采样 → 13 实例，Python 复刻合并算法验证）。

后果：书写一条长笔画时，动态层里常驻的是**未合并**的段表，随笔画变长线性
膨胀，且每帧被完整重绘（`draw(0..6, 0..dynamic_slot.seg_count)`）。
抬笔时整条被换成合并后的静态几何——**这一次重排改动前就已存在**，不是我
引入的。

**整改**：满批块改走 `emit_merged`，与晾干路径**共用同一份合并代码**（不是
复制一份），因此"书写中的满批部分"与"晾干后的静态部分"胶囊切分逐段一致。

**只改满批块、不改尾部块**是刻意的：尾部块是每追加一点就整体重建的活跃
部分，让它保持逐采样发射，用户盯着看的笔尖就是精确样条几何，不引入任何
新的抬笔重排。改动前后的抬笔重排完全相同（未合并 → 已合并）。

顺带把 `run` 从 `Vec::with_capacity(...)` 换成定长栈数组 `MergeRun`——长度
上界由 `extend` 里的 `len >= MERGE_MAX_RUN` 判定保证（到界即断链发射），
所以定长数组不会溢出，**镶嵌路径的合并状态也变成零堆分配**。

测试锁了四件事：合并后实例数至少减半、两条直播路径策略确实不同、
**满批块 == 同区间晾干镶嵌**（逐段断言）、零宽的两种形态（单个零宽点 =
笔锋收尖，产生末端半宽渐缩到 0 的锥尖胶囊；连续 ≥2 个零宽点 = 断链，
两侧不桥接）。

**连带修正了一个被合并打破的旧不变量**。原 `live_range_batch_consistency`
断言"稳定批 ++ 头部 == 整条镶嵌"——那是**未合并**发射才有的区间可分解性
（每个采样点产一段，与区间无关）。合并后 run 从区间起点起算，跨边界的 run
被切成两条，实测整条 16 段 vs 拆分 17 段，等价性不再成立。

好在生产并不需要它：满批块的区间是固定推进的（`[0..15]`、`[16..31]`…），
一经镶嵌即定形并长期驻留，**从不与相邻区间重新合并**；尾部块走未合并路径，
区间可分解性保持。生产真正依赖的是**拼接处几何连续**——稳定批末点与头部
首点是同一个采样点 `path[16]`，位置与宽度都完全相同（实测偏差 0.0），
所以没有缝隙也没有重叠。测试已按这个真实性质重写。

> 顺带说一句：把"零宽点"当成一律断链也是错的。单个零宽采样点会被
> `extend` 的同点去重吃掉（位置与前一点相同时宽度进不了 run），真实数据里
> 笔锋一边前进一边收窄、位置必然不同，所以单个零宽点表现为**锥尖**而非断链。
> 我第一版测试把零宽点放在与前一点相同的位置，结果根本没触发收尖——已改为
> 不同位置并对两种形态分别断言。

### P2-14 橡皮擦扣除无笔画级早退　→ 已修复

`append_stroke` 对**每个采样段**都做一次网格查询（`subsample_segment_into`
→ `segment_touches` → 若干次 `HashMap` 查表）。画布上只要存在任何橡皮擦，
每条笔画都得付这笔钱——哪怕它离橡皮擦十万八千里。

**整改**：`EraserIndex` 记录全部橡皮擦段的并集**中心线**包围盒 +
最大半宽；`append_stroke` 先做一次 O(采样数) 的笔画级判定，明显够远就
走无扣除快路径。

等价性推导（`may_touch_samples` 的文档注释里有完整版）：

1. 采样段 `a ⊆ S`、橡皮擦段 `b ⊆ E` ⇒ `box_sep(a,b) ≥ box_sep(S,E)`
   （子集的点对更少，最小距离只会更大）；
2. 两段都落在各自中心线盒内 ⇒ `dist(a,b) ≥ box_sep(a,b)`；
3. 合并 ⇒ `dist(a,b) ≥ box_sep(S,E)`；
4. 命中判据是 `dist(a,b) < draw_hw + e_half_w ≤ S_hw + E_hw`。

所以 `box_sep(S,E) > S_hw + E_hw` 时判据恒假。

> 这里我**第一版写错了**：把半宽 padding 折进了包围盒再比相交。padding 一旦
> 并入盒子，"盒子分离"只能推出距离 > 0，推不出 > `draw_hw + eraser_hw`，
> 会把**仍需扣除的笔画误判为可跳过**——漏擦。改正为"半宽单独参与比较"
> 后推导才成立。这类错误肉眼 review 极难发现，所以配了穷举测试：
> 4 种橡皮擦粗细 × 11 个横向距离 × 7 个纵向距离（含贴边、擦穿、擦一半），
> 断言"早退版 == 强制扣除版"逐段一致；另配一个反向测试确保早退**不是**靠
> "全都跳过"蒙对的（擦得掉的场景必须真的擦掉）。

### P2-15 颜色逐次解字符串　→ 已修复

直播镶嵌**每个输入点**都要取一次笔画颜色，旧实现每次都
`trim_start_matches('#')` + 3 次 `from_str_radix`。颜色在构造后不再变更
（全仓无 `stroke.color =` 赋值），故在 `DrawStroke::new` 里解析一次存进
`color_rgba`，热路径只读数组。

顺带把解析本身写稳了：旧实现用 `&hex[0..2]` 做**字节下标切片**，非 ASCII
输入会 panic；新实现先 `bytes().all(is_ascii_hexdigit)` 判定再逐字节算值。
（当前产品路径上 `valid_color` 已保证只传 `#rrggbb`，所以这是防御性加固，
不是线上 bug。）

### 已评估、决定不改

| 候选 | 判断 |
| --- | --- |
| 合并容差改为按笔宽归一（修高倍缩放下的多边形感） | 容差在画布坐标、几何只上传一次，做不到随缩放自适应。收紧绝对容差能改善高倍缩放观感，但**直接增加段数**——与本轮首要目标相反。留待有实测帧时间数据再定。 |
| 尾部块也只重算最后一个段 | 文档已指出"只有最后一段形状随新点改变"，理论可把每点镶嵌从 O(批长×细分) 降到 O(细分)。但尾部有 ≤16 段上界，算术量本就有限（~1.5 Mflops/s），而引入"尾部已定形段缓存"要新增 `LiveCanvasState` 字段并与批推进逻辑耦合，风险大于收益。 |
| 片元着色器去掉冗余 storage 二次加载 | 现在只用了 4 个 `vec4` 全都参与了计算，且同一实例的 6 个片元共享缓存行。改成 varying 要多占插值器带宽，净亏。 |
| `EraserIndex` 的 `HashMap` 换无哈希网格 | P2-14 的早退已把绝大多数查询消掉，剩余查询量不足以支撑换容器带来的迁移成本。 |

---

## 3.1 前两轮已修（摘要）

| 项 | 问题 | 修法 |
| --- | --- | --- |
| P1-1 | 静态瓦片光栅缓存退化为全屏 blit | 删除整套机制，回几何重放 |
| P1-2 | iced 层缓存 100% 空转（UI 层必含 image） | 删除整套机制 |
| P1-3 | 每帧多次 `Queue::write_buffer` ⇒ 多次独立 submit | 新增 `write_buffer_belt`，6 处热路径统一走帧 encoder |
| P2-1 | 相机帧每帧 1.6MB 无用 alpha 扫描 | `assume_opaque` 参数，相机槽跳过 |
| P2-2 | 两处逐帧原子心跳 | 删除 |
| P2-3 | 死代码（spike crate / EventBatch / 失效计数） | 删除 2900+ 行 |
| — | 后台线程重算主线程已算的未扣几何 | 删除 `compute_uncut` 死链路 |
| — | 橡皮擦集按任务数深拷贝 | `Arc<Vec<DrawStroke>>` 共享快照 |
| — | `retessellate` 两趟 O(n²) + 无谓克隆 | 单趟闭包过滤 + 早退 |
| — | undo-Erase 深拷贝完整橡皮擦集 | `mem::swap` |
| — | RGB 回退每帧多拷 1.6MB | 原地转换，删 `buffer` 字段 |
| — | NV12→RGBA 每像素 4 次 push | `chunks_exact_mut(4)` |
| — | `end_frame` 每帧一次 Mutex | `AtomicU64` 节流时钟 |
| — | `request_redraw_at` 丢帧不可观测 | 新增 `dropped` 计数 |

---

## 4. 整改后的管线

```
prepare（每帧）
├─ direct 通道 prepare
│   ├─ CameraChannel：变换/滤镜 uniform（belt）+ 帧纹理上传（belt）
│   └─ AnnotationLayer
│       ├─ 变换 uniform（belt，仅变化时）
│       ├─ upload_static：快照 ptr 比对 → O(1) 早退 / 前缀增量 / 全量（belt）
│       ├─ upload_dynamic：稳定层 ptr 增量 + 尾层重传（belt）
│       └─ upload_cursor：指纹门控（belt）
├─ iced 层栈 prepare（上游原版）
└─ staging_belt.finish() → queue.submit（唯一一次 submit）→ recall

render
├─ 背板 clear（LoadOp::Clear）
├─ CameraChannel::draw（1 次 draw_indexed）
├─ AnnotationLayer::draw（≤3 次实例化 draw：静态 / 动态 / 光标，共用一条 set_pipeline）
└─ iced 层栈（上游原版逐层 pass）
```

不变量（均已保留并有单元测试覆盖）：

1. 静态层前缀增量只在「快照以旧快照为前缀 **且** 视口矩形位级未变 **且** 剔除开关一致」时启用；
2. 缓冲重建（`ensure_capacity` 返回 true）降级为全量重写并刷新 bind group；
3. 动态层缓冲重建后从偏移 0 重打包（`write_offset_segs = 0`）；
4. 悬停光标槽仅在缓冲重建时刷新 bind group；
5. `write_buffer_belt` 的数据长度按 `COPY_BUFFER_ALIGNMENT` 对齐
   （写入单元恒为 `Globals` 80B / `StrokeSeg` 64B / `QuadVertex` 16B）；
6. **`CachedMesh::bounds` 必须与 `geometry` 同代**——快照构建直接取用，
   不再现场重扫段表（见 P2-4）；
7. **批量重算必须复用同一个 `EraserIndex`**（见 P2-5），单笔路径才用
   `tessellate_stroke_canvas` 现场建索引；
8. **`strokes` 是权威列表、渲染缓存是派生数据**——撤销必须同时改写两者（见 P2-8）；
9. **曲线求值是纯性能改写**——`CurveCtx` 必须与旧 `catmull_rom` 逐位一致，
   跨度减法、退化判据、除法与插值顺序均不得改动（见 P2-12，有对照测试）；
10. **橡皮擦早退只允许"跳过必然不命中的查询"**——半宽必须单独参与比较，
    不得折进包围盒（见 P2-14，有穷举对照测试）；
11. **满批块走合并、尾部块走逐采样**——前者与晾干结果逐段一致，后者保持
    精确样条；两条直播路径的发射策略不可互换（见 P2-13）；
12. **批边界只保证"几何连续"，不保证"区间可分解"**——合并 run 从区间起点
    起算，跨边界的 run 会被切成两条（实测整条 16 段 vs 拆分 17 段），所以
    生产依赖的是拼接处**首尾点与宽度完全相同**（实测偏差 0.0），而非
    "分段拼接 == 整条镶嵌"。未合并的尾部路径仍区间可分解，该性质由测试锁定。

---

## 5. 预期收益

| 指标 | 整改前 | 整改后 |
| --- | --- | --- |
| 4K 常驻显存 | +33MB（静态光栅纹理）+ N×33MB（层纹理） | 仅 surface |
| 稳态 submit/帧 | 1 + 每帧 0–4（`write_buffer`） | **1** |
| 静态层每帧片段 | `U×256²`（退化后 = 全屏） | ≈`V`（笔迹覆盖） |
| 每帧 CPU（层缓存） | 全层 quad+mesh 字节哈希 | 0 |
| 相机回退路径 CPU | +1.6MB/帧 alpha 扫描 | 0 |
| 每帧诊断心跳 | 2 次原子 RMW | 0 |
| 删除代码量 | — | ≈2900 行（含 1462 行 spike、485 行 batch、1400 余行缓存机制） |

第五轮（笔迹算法）新增：

| 指标 | 整改前 | 整改后 |
| --- | --- | --- |
| 曲线求值的 `hypot` 次数/段 | `4 + 3·k_sub`（k_sub ≤ 8 ⇒ ≤ 28） | **4** |
| 采样循环内超越函数 | 每采样点 3 次 `hypot` + 3 次 `sqrt` | **0** |
| 书写中长笔画的动态层段数 | ≈10.7× 晾干后（实测压缩比 10.7:1） | ≈1.0×（满批部分已合并） |
| 镶嵌的堆分配 | 每点 3 次（段缓冲 + Arc + run） | **0**（复用 + 栈上 `MergeRun`） |
| 每输入点的颜色解析 | 1 次 trim + 3 次 `from_str_radix` | **0**（构造时解析一次） |
| 擦除重建时无关笔画的网格查询 | 全部 | **0**（笔画级早退） |

## 6. 未验证事项

本机无 `cargo`/`rustc`，目标为 Windows DX12，**所有改动均未经编译或运行验证**。
需在 Windows 上确认：

1. `cargo build --release` 通过（含 iced fork 的 `Renderer::prepare`/`render` 删改）；
2. `cargo test -p viewink -p viewgfx -p viewcamera` 通过（当前 134 个测试）；
3. 真机 `[rstat]` 输出中 `own_submit` 与 `iced_lc` 字段已消失，
   静态层 `anno_draw static` 在有笔迹时 > 0、静止且有笔迹时保持稳定（不再出现 blit）；
4. 书写延迟、pan/zoom 流畅度、橡皮擦悬停与笔迹视觉无回归（瓦片光栅缓存与几何重放
   在 SDF 胶囊下应为位级一致的渲染结果，抗锯齿完全由着色器决定）。

### 第五轮专项回归

本轮改的是笔迹几何本身，视觉验证不能省：

5. **书写一条长笔画**（数百点），观察动态层段数与 GPU 占用——满批块合并后
   应显著下降；抬笔瞬间的几何重排应与改动前**完全一致**（未合并尾部 →
   已合并满批，这是既有行为，不是本轮新增）；
6. **笔锋起收尖**：起笔渐入与笔尾收尖的锥尖胶囊应照常出现（本轮测试锁了
   "单个零宽点 = 锥尖、连续零宽点 = 断链"两种形态）；
7. **擦除回归（重点）**：擦一条线，确认被擦掉的笔迹真的消失、没擦掉的没被误伤。
   P2-14 的早退若判据写错，症状是"擦不干净"或"远处笔画被误擦"——我在实现中
   恰好先写错了一版（把 padding 折进包围盒），所以这一条务必人工过一遍，
   尤其是**擦除边缘**（刚好擦到笔画边缘的那一格）；
8. **撤销 + 橡皮擦组合**：撤销绘制后再用橡皮擦，确认笔迹不会复活（P2-8 修复的落点）。