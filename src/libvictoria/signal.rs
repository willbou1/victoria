use std::time::{Duration, Instant};

pub struct Decay {
    half_life: Duration,
    value: f64,
    last_update: Instant,
}

impl Decay {
    pub fn new(half_life: Duration, first: Option<f64>) -> Self {
        debug_assert!(half_life != Duration::ZERO);
        Self {
            value: first.unwrap_or(0.),
            last_update: Instant::now(),
            half_life,
        }
    }

    fn update(&mut self) {
        self.value *=
            2.0_f64.powf(-self.last_update.elapsed().as_secs_f64() / self.half_life.as_secs_f64());
        self.last_update = Instant::now();
    }

    pub fn change(&mut self, f: impl FnOnce(f64) -> f64) {
        self.update();
        self.value = f(self.value);
    }

    pub fn value(&mut self) -> f64 {
        self.update();
        self.value
    }
}

pub struct AsymmetricEMA {
    alpha_up: f64,
    alpha_down: f64,
    value: f64,
}

impl AsymmetricEMA {
    fn alpha(time_constant: Duration, interval: Duration) -> f64 {
        // This is from solving the ODE for a 1st order LP and discretizing it
        let dt = interval.as_secs_f64();
        1.0 - (-dt / time_constant.as_secs_f64()).exp()
    }

    pub fn new(
        interval: Duration,
        tau_down: Duration,
        tau_up: Duration,
        first: Option<f64>,
    ) -> Self {
        let alpha_up = Self::alpha(tau_up, interval);
        let alpha_down = Self::alpha(tau_down, interval);
        debug_assert!((0.0..=1.0).contains(&alpha_up));
        debug_assert!((0.0..=1.0).contains(&alpha_down));
        Self {
            alpha_down,
            alpha_up,
            value: first.unwrap_or(0.),
        }
    }

    pub fn update(&mut self, sample: f64) {
        let alpha = if sample > self.value {
            self.alpha_up
        } else {
            self.alpha_down
        };
        self.value = alpha * sample + (1. - alpha) * self.value;
    }

    pub fn value(&self) -> f64 {
        self.value
    }
}
