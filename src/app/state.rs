/// 画布交互模式。
///
/// 原先有 `Blackboard` 变体：进入时清空全部笔迹与撤销栈并冻结摄像头。
/// 工具栏按钮移除后该模式不可达，变体随之删除——相机画面本身即可批注，
/// 原黑板场景已被「批注模式」完全覆盖。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrawMode {
    Move,
    Comment,
    Eraser,
}
