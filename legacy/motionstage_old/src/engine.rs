pub trait Engine {}

pub fn default_engine() -> Box<dyn Engine> {
    ECSEngine::new()
}

pub struct ECSEngine {}

impl ECSEngine {
    pub fn new() -> Box<dyn Engine> {
        Box::new(Self {})
    }
}

impl Engine for ECSEngine {}
