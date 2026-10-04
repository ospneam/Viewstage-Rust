# 擦除集快照零深拷贝（第十轮）

> 目标：消除"每写一笔就深拷贝整份橡皮擦集"这条随擦除次数线性增长的
> 主线程开销。

## 结论：`Arc` 之前就已经深拷贝了，所以它一点忙都没帮上

旧代码：

```rust
let erasers = Arc::new(self.eraser_strokes.clone());
```

深拷贝发生在**包进 `Arc` 之前**。所以每个任务拿到的都是一份独立的完整拷贝，
而 `finalize_stroke` 这条路径**每写一笔就走一次**。`Arc` 只在
`finalize_eraser` 内部那批 targeted rebuild 任务之间真的起了共享作用。

`DrawStroke` 每次克隆要 **4 次堆分配**（`points` / `color` /
`variable_widths` / `segment_times`），所以成本是：

```
每写一笔的代价 = 4 × 橡皮擦数 次堆分配  +  O(橡皮擦数 × 点数) memcpy
```

| 橡皮擦数（每条 80 点） | 每写一笔的堆分配 | memcpy |
| --- | --- | --- |
| 10 | 40 次 | 28 KB |
| 50 | **200 次** | **141 KB** |
| 200 | 800 次 | 566 KB |

注意触发频率与成本规模**不成比例**：橡皮擦是偶发操作，深拷贝却要**每写一笔**
都付一遍。50 次擦除的会话里，每晾干一笔就白扔 200 次分配 + 141 KB 拷贝。

撤销栈同理：每次 `Erase` 命令都深拷贝一份当时的完整集合，k 次擦除累计
O(k²) 次分配。

## 修法：`EraserSet`——权威列表 + 写时复制快照

新增 `crates/viewink/src/ink/eraser.rs`：

```rust
pub struct EraserSet {
    items: Vec<DrawStroke>,          // 私有
    shared: Arc<Vec<DrawStroke>>,    // 交给后台线程
}
```

四个调用点全部从"深拷贝"变成"引用计数自增"：

| 位置 | 旧 | 新 |
| --- | --- | --- |
| `finalize_stroke` | `Arc::new(eraser_strokes.clone())` | `erasers.shared()` |
| `finalize_eraser`（入撤销栈） | `eraser_strokes.clone()` | `erasers.shared()` |
| `finalize_eraser`（任务快照） | `Arc::new(eraser_strokes.clone())` | `erasers.shared()` |
| `capture_source_state` | `eraser_strokes.clone()` | `erasers.shared()` |
| `history::undo`（恢复） | `mem::swap` | `erasers.replace(..)` |

累计分配次数（k 次擦除，每条 80 点）：

| 橡皮擦数 | 撤销栈分配（旧） | 撤销栈分配（新） |
| --- | --- | --- |
| 50 | 392,000 | **50** |
| 500 | 39,920,000 | **500** |

## 关键设计决定：为什么 `items` 私有

最省事的修法是"在每个变更点记得刷新快照"。**那是个靠记忆的纪律**：漏一处
就让后台线程拿到过期橡皮擦集，算出本该被擦掉的笔迹——已擦内容悄悄复活，
而且极难定位（画面上只是"某块墨迹又出现了"）。

所以让 `items` **私有**，变更只能经 `push` / `replace` / `clear` / `take`，
每个方法内部同步刷新 `shared`。**编译器兜住这个不变量**，而不是靠 review。

这也覆盖了此前我漏掉的一个真实变更点：`history::undo` 内部通过
`&mut Vec<DrawStroke>` 就地 `mem::swap` 改写橡皮擦集。把参数类型换成
`&mut EraserSet` 之后，这条旁路在类型层面被封死了。

## 第二条不变式：快照永不被改动

`push` 的实现是**整体替换** `items`（`mem::take` + `push` + `replace`），
而非 `Vec::push`。这带来一个可以白拿的好处：

> `shared()` 交出去的那份快照，此后任何操作都改不动它。

于是它可以直接用作**撤销栈里的历史快照**——`DrawCommand::Erase` 从
`saved_eraser_strokes: Vec<DrawStroke>` 变成
`saved_erasers: Arc<Vec<DrawStroke>>`，入栈零深拷贝。

这条性质由 `snapshot_is_immutable_across_later_pushes` 测试锁定。

`history::undo` 用 `Arc::try_unwrap` 尝试无拷贝取回；被多个 `Erase` 命令共享
时退回整体替换（仍不深拷贝权威列表，只是多留一份旧快照）。

## 测试

新增 7 个（`eraser.rs` 6 个 + `history.rs` 1 个）：

| 测试 | 锁定的不变量 |
| --- | --- |
| `push_refreshes_snapshot` | push 后后台看到的必须是**当前**集 |
| `snapshot_is_immutable_across_later_pushes` | 后续 push/clear 不得改动已交出的快照 |
| `clear_and_replace_refresh_snapshot` | clear / replace 同步刷新 |
| `shared_is_refcount_clone_not_deep_copy` | 两次 `shared()` 必须 `Arc::ptr_eq` |
| `take_resets_both_halves` | take 后两半都清空 |
| `deref_exposes_authoritative_list` | 解引用暴露权威列表 |
| `undo_erase_refreshes_shared_snapshot` | **撤销后共享快照必须已刷新**（否则下一笔的扣除几何用到过期集，已擦内容复活） |

`history.rs` 里 8 处 `&mut Vec::new()` 改为 `&mut EraserSet::new()`；
`undo_erase_restores_previous_eraser_set` 改用 `shared()` 走真实路径。

## 影响面

| 文件 | 改动 |
| --- | --- |
| `crates/viewink/src/ink/eraser.rs` | **新增**（`EraserSet` + 6 测试） |
| `crates/viewink/src/ink/mod.rs` | `pub mod eraser;` |
| `crates/viewink/src/ink/history.rs` | `DrawCommand::Erase` 字段改 `Arc`；`undo` 参数改 `&mut EraserSet` |
| `src/app/source_manager.rs` | `SourceState.eraser_strokes` 改 `Arc<Vec<DrawStroke>>` |
| `src/main.rs` | `eraser_strokes: Vec<DrawStroke>` → `erasers: EraserSet`（11 处引用） |

## 验证方式

本机无 `cargo`/`rustc`，未经编译验证。已做：

- 括号平衡（全仓）
- `self.erasers` 11 处引用与 `EraserSet` 方法集配平
- `eraser_strokes` 残留扫描（只剩 `SourceState` 字段名与解释性注释）
- `EraserSet` 方法集与所有调用点匹配（`shared`/`push`/`replace`/`clear`/`len`/`iter`/`as_slice`/`take`）
- 测试总数 166 → **173**

Windows 上请确认：

1. `cargo build --release` 通过（`history::undo` 签名变了，调用点已同步）；
2. **擦除后笔迹保持擦除态**——若某块已擦内容重新出现，几乎一定是某条
   `EraserSet` 路径没刷新共享快照；
3. 撤销擦除 → 被擦内容恢复，且**再写一笔**后恢复的内容仍保持（这条专门验证
   `undo` 刷新了快照）；
4. 切换信号源来回 → 笔迹与擦除态都正确；
5. 擦除性能：多擦几次后再书写，主线程应无明显卡顿。
