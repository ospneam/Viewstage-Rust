use viewink::ink::history::HistoryManager;
use viewink::ink::stroke::DrawStroke;
use std::collections::HashMap;
use std::sync::Arc;

const MAX_SOURCE_CACHE: usize = 50;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum SourceId {
    Camera,
    Pic(u64),
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceId::Camera => write!(f, "cam"),
            SourceId::Pic(n) => write!(f, "pic-{}", n),
        }
    }
}

#[derive(Clone)]
pub struct SourceState {
    pub strokes: Vec<DrawStroke>,
    /// 橡皮擦集快照（共享只读，非深拷贝）。
    ///
    /// 切源期间可能有后台计算任务仍持有这份快照，所以必须是**不可变**的
    /// 共享引用——`EraserSet` 的写时复制契约保证了这一点。
    pub eraser_strokes: Arc<Vec<DrawStroke>>,
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub current_image_index: Option<usize>,
    pub showing_image: bool,
    pub history: HistoryManager,
    pub timestamp: u64,
}

pub struct SourceManager {
    cache: HashMap<SourceId, SourceState>,
    current: Option<SourceId>,
    pic_counter: u64,
}

impl SourceManager {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            current: None,
            pic_counter: 0,
        }
    }

    pub fn alloc_pic_id(&mut self) -> SourceId {
        self.pic_counter += 1;
        SourceId::Pic(self.pic_counter)
    }

    pub fn save_current(&mut self, state: SourceState) {
        if let Some(id) = self.current.clone() {
            self.save(id, state);
        }
    }

    pub fn save(&mut self, id: SourceId, mut state: SourceState) {
        state.timestamp = now_ms();

        if self.cache.len() >= MAX_SOURCE_CACHE && !self.cache.contains_key(&id) {
            let oldest = self
                .cache
                .iter()
                .min_by_key(|(_, s)| s.timestamp)
                .map(|(k, _)| k.clone());
            if let Some(old) = oldest {
                self.cache.remove(&old);
            }
        }

        self.cache.insert(id, state);
    }

    /// 切换到目标信号源，返回缓存的之前状态（如果有）
    pub fn switch_to(&mut self, id: SourceId) -> Option<SourceState> {
        let state = self.cache.get(&id).cloned();
        self.current = Some(id);
        state
    }

    /// 进入一个新信号源（不保留之前的缓存）
    pub fn enter(&mut self, id: SourceId) {
        self.current = Some(id);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
