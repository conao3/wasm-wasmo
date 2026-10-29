pub trait Env {
    fn now_ms(&self) -> f64;
    fn random_fill(&mut self, buf: &mut [u8]);
}

pub struct FixedEnv {
    pub now_ms: f64,
    pub seed: u64,
}

impl Env for FixedEnv {
    fn now_ms(&self) -> f64 {
        self.now_ms
    }

    fn random_fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            *b = self.seed as u8;
        }
    }
}
