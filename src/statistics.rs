// Welford's Online algorithm
#[derive(Default)]
pub struct OnlineStats {
    count: usize,
    mean: f64,
    m2: f64,
}

impl OnlineStats {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn update(&mut self, value: f64) {
        self.count += 1;
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = value - self.mean;
        self.m2 += delta * delta2;
    }

    pub fn mean(&self) -> f64 {
        self.mean
    }

    pub fn variance(&self) -> f64 {
        if self.count > 1 {
            self.m2 / (self.count as f64 - 1.0)
        } else {
            0.0
        }
    }

    pub fn stddev(&self) -> f64 {
        self.variance().sqrt()
    }
}
