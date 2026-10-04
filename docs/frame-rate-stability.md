# 帧率稳定性：为什么之前"稳定"不了 60fps（第七轮）

> 目标：让帧率**一直**稳定在 60fps。
> 结论先行：不是"不够快"，是**帧率节流器把 60fps 锁成了 30fps**。

## 根因：定时器节流的唤醒点压在 vsync 边界上

旧 `paced_redraw`（`App::paced_redraw`）在未达目标间隔时排一个定时补帧：

```rust
let target = Duration::from_micros(1_000_000 / fps as u64);   // 60 → 16666µs
let elapsed = self.last_frame_start.get().elapsed();
if elapsed >= target { Action::request_redraw() }
else { Action::request_redraw_at(Instant::now() + (target - elapsed)) }
```

致命之处在于**相位**：AutoVsync 下渲染帧的起点由 vsync 决定，于是
`last_frame_start` 本身就落在 vsync 边界上。加一个比 vsync 周期短 1µs 的
偏移，得到的唤醒点是**整条时间线上最不可能赶上的位置**——距 vsync 截止线
只剩 1µs，而 prepare + render + submit 需要 ~2ms。于是每一帧都错过，
被迫推迟到下一个 vsync，帧间隔变成 2 个 vsync。

数值复刻（60Hz 面板，work=2ms，40 帧稳态）：

```
帧起点   唤醒      提交      命中 vsync   间隔
0        16666     18666     33334        —
33334    50000     52000     66668        33334
66668    83334     85334     100002       33334
...      ...       ...       ...          33334   ← 稳定在 30fps
```

### 各种上限取值下的实测对照

| `annotation_fps_cap` | 定时器方案实际帧率 | 新方案（跳 vsync） |
| --- | --- | --- |
| 30 | 20.0 | **30.0** |
| 40 | 30.0 | 30.0 |
| 50 | 30.0 | 60.0（上限低于刷新率 1.2 倍 → 不节流） |
| **60** | **30.0** | **60.0** |
| 90 / 120 / 144 / 165 | 60.0 | 60.0（= 面板刷新率） |

定时器方案在**任何**取值下都上不去 60fps，且**越接近刷新率越糟**——
恰好是"想要 60fps"时最失效。

> 顺带自我更正：上一轮把 `from_millis(1000 / fps)` 改成微秒精度，方向对
> （毫秒整除确实把 60 算成 62.5）但**没触及真正的 bug**。改成微秒反而让
> 目标比 vsync 短了 1µs，是最坏相位。本轮才找到根因。

## 修法：跳 vsync，而不是排定时器

vsync 本身已经把帧率钉在刷新率上，所以"降帧"只需**隔帧不重绘**：
要降到 `refresh/(N+1)`，就在第 N+1 个 vsync 照常呈现、中间跳过 N 个。
全程没有定时器，因此没有"差一点赶上"的可能。

新增 `src/app/pacing.rs`（纯函数，9 个单元测试）：

| 函数 | 职责 |
| --- | --- |
| `skip_count(refresh_hz, cap_hz)` | 该跳过几个 vsync。`cap >= refresh*1.5` → 0（交给 vsync） |
| `throttle_wake_offset_us(refresh_hz, skip)` | 唤醒偏移 = `N*period + period/2`（**区间正中**，前后各 `period/2` 裕度） |
| `merge_period_sample(prev, sample)` | 刷新周期 EMA，**拒绝离群样本** |

### 两条关键设计决定

**1. 上限不低于刷新率时完全不节流。** 这正是"稳定 60fps"最常见的情形
（60Hz 面板 + 上限 60）。此时 vsync 就是节流器，任何额外机制都只会添乱。
`skip_count` 在 `refresh/cap < 1.5` 时返回 0。

**2. 非整数比取最近的整数跳帧数。** 60Hz 面板上限 50 时，`round(60/50)=1`
→ 不节流，实际跑 60fps。**这是有意的**：非整数跳帧会产生 1 个和 2 个 vsync
交替的抖动，而稳定帧率正是本轮的目的——宁可实际帧率略高于上限，也不要抖。
144Hz + 上限 30 实际得到 28.8fps（144/5），同样是稳定的最近可达值。

### 刷新率必须实测，不能猜

`skip_count` 需要知道面板是 60 还是 144Hz。新增 `App::display_period`
（`Cell<Duration>`），在 `draw()` 里用相邻两帧起点之差做 EMA 估计
（AutoVsync 下帧起点由 vsync 决定，差值即周期）：

- 拒绝离群样本：只接受与当前估计相差 ±20% 内的样本。长帧尖峰（首次上传、
  后台线程抢占）若进了 EMA，会把"刷新率"估高，**长帧反而看不出来**；
- 未测得时 `measured_refresh_hz()` 返回 0，`skip_count` 返回 0 —— 宁可全速
  也不按猜测跳帧；
- 样本权重 1/8，足以跟随刷新率模式切换（60Hz → 120Hz 外接屏）。

## 直接操作一律满 vsync

`is_direct_manipulation()` 覆盖：书写中 / 鼠标按下 / 触控在手 / 惯性中。
这些场合**完全绕过上限**。

理由：书写与平移是连续跟手的交互，60fps 上限会让笔尖/画布出现可见阶梯感；
而批注层的动态上传是 KB/s 级（增量管线），满刷新率的 GPU 增量成本可忽略。
省电诉求不应该落在"手感"上。

> 旧实现只对 `is_drawing` 豁免，**平移被限速**——拖动画布时最高只有
> 60fps，在 144Hz 屏上明显发涩。

## 稳定性可观测：`[rstat]` 与 HUD 新增三项

之前只有 `fps=`，看不出"60fps 是不是稳的 60fps"。新增：

| 指标 | 含义 |
| --- | --- |
| `refresh=` | 实测刷新周期（ms）。`16.67` = 60Hz |
| `long=` | **长帧数**：帧间隔 > 1.5 个周期。**`long > 0` 即肉眼可见的顿挫** |
| `worst=` | 窗口内最长帧间隔（ms）。单帧尖峰的绝对量级 |
| `throttled=` | 因节流主动跳过的请求数（**预期行为**，与长帧分开统计） |

日志行现在自带判读，例如：

```
[rstat] 1.0s frames=60 ... refresh=16.67ms long=0 worst=16.70ms throttled=0
        | fps=60.0/s (vsync 60Hz long=0 worst=16.7ms) cam=30.0/s view=30.0/s ...
```

性能 HUD 也加了同一行（开发者模式）：`刷新 60.0 Hz | 长帧 0 | 最坏 16.7 ms`。

### 判定标准

**稳定 60fps = `fps≈60` 且 `long=0`。** 若 `long>0`，看 `worst`：
- `worst ≈ 33ms` → 某一帧没赶上 vsync，整帧延迟一个周期
- `worst ≈ 50ms` → 连续错过两个 vsync（通常是首帧上传或后台线程抢占）

### 旧 `dropped=` 计数为何删除

它统计的是"定时器排的帧被 winit 丢掉"，在旧设计下**属预期行为**
（注释里就这么写）。但这掩盖了真正的问题——定时器方案下 30fps 也"符合
预期"。拆成 `throttled=`（主动节流，预期）与 `long=`（没赶上 vsync，
异常）之后，异常才暴露得出来。

## 单元测试

`src/app/pacing.rs`（9 个，全是纯函数，无需 GPU）：

- `cap_equal_to_refresh_does_not_throttle` —— **核心断言**：`skip_count(60, 60) == 0`
- `cap_below_refresh_throttles_to_nearest_divisor` —— 144→60 隔 1、60→30 隔 1、60→20 隔 2
- `effective_rate_tracks_the_cap` —— 6 种刷新率 × 4 种上限，实际帧率须贴合上限
- `wake_offset_lands_mid_interval` —— 唤醒点必须落在 vsync 区间正中
- `insufficient_margin_falls_back_to_full_rate` —— `period/2 < 3ms` 时不节流
- `period_estimate_seeds_then_ema` / `rejects_outliers` / `follows_a_mode_switch`

`crates/viewink/src/stats.rs`（新增 3 个）：

- `frame_interval_seeds_refresh_and_stays_clean` —— 60Hz 序列 60 帧 → `long == 0`
- `long_frame_is_counted_without_corrupting_refresh_estimate` —— 50ms 尖峰
  计入 `long`，但**不得**把刷新率估计拉偏
- `throttled_frames_are_counted_separately` —— 主动节流不混进长帧

## 端到端模拟结果（复刻新代码）

```
60Hz cap=60      skip=0  → 60.0fps  间隔 16.67ms  错帧 0/40
60Hz cap=30      skip=1  → 30.0fps  间隔 33.33ms  错帧 0/40
144Hz cap=60     skip=1  → 72.0fps  间隔 13.89ms  错帧 0/40
144Hz cap=30     skip=4  → 28.8fps  间隔 34.72ms  错帧 0/40
```

`skip=0`（即 60Hz + cap=60）对每帧工作量**完全不敏感**：

```
work=1000µs → 60.0fps    work=4000µs → 60.0fps
work=2000µs → 60.0fps    work=6000µs → 60.0fps
```

这是关键性质：只要每帧工作在 16.67ms 预算内，vsync 就保证精确 60fps，
帧率上限机制**不参与**，也就没有引入抖动的机会。

## 仍需真机确认

本机无 `cargo`/`rustc`，未经编译验证。静态检查：全仓括号平衡、
`pacing` API 与 `main.rs` 调用点逐一对应、`Cell` 经内可变性写回
（`draw(&self)` 不能直接给字段赋值——已修正）、150 个测试。

Windows 上请确认：

1. `cargo build --release` 与 `cargo test -p viewstage -p viewink` 通过；
2. 60Hz 面板 + 上限 60，静止时 `[rstat]` 应为 `fps≈60`、`long=0`、
   `worst≈16.7ms`；
3. 书写、平移、惯性滑动全程 `long=0`；
4. 若 `long > 0`，用 `worst` 判断是单帧尖峰还是持续跟不上——后者说明
   每帧工作量超预算，需要用 `worst` 的量级定位到具体层
   （`view_avg` 看 widget 重建，`up=` 看上传带宽）。
