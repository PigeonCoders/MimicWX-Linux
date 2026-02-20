//! 拟人化引擎
//!
//! 为所有输入操作添加符合自然人行为特征的随机延迟，
//! 使自动化操作在时间维度上不可被区分。

use rand::Rng;
use rand_distr::{Distribution, Normal};

/// 拟人化参数引擎
pub struct Humanizer {
    /// 按键持续时间 (正态分布, ms)
    key_hold_mean: f64,
    key_hold_std: f64,

    /// 打字间隔 (正态分布, ms)
    typing_delay_mean: f64,
    typing_delay_std: f64,

    /// 鼠标移动步长延迟 (ms)
    mouse_step_delay_mean: f64,
    mouse_step_delay_std: f64,

    /// 点击按压时长 (ms)
    click_hold_mean: f64,
    click_hold_std: f64,
}

impl Humanizer {
    pub fn new() -> Self {
        Self {
            key_hold_mean: 30.0,
            key_hold_std: 10.0,
            typing_delay_mean: 65.0,
            typing_delay_std: 25.0,
            mouse_step_delay_mean: 5.0,
            mouse_step_delay_std: 2.0,
            click_hold_mean: 80.0,
            click_hold_std: 20.0,
        }
    }

    /// 生成按键持续时长 (15-45ms 正态分布)
    pub fn key_hold_duration(&self) -> u64 {
        self.sample_ms(self.key_hold_mean, self.key_hold_std, 15, 45)
    }

    /// 生成打字间隔 (40-120ms 正态分布)
    pub fn typing_delay(&self) -> u64 {
        self.sample_ms(self.typing_delay_mean, self.typing_delay_std, 40, 120)
    }

    /// 生成鼠标步长间延迟
    pub fn mouse_step_delay(&self) -> u64 {
        self.sample_ms(self.mouse_step_delay_mean, self.mouse_step_delay_std, 2, 10)
    }

    /// 生成点击按压时长
    pub fn click_hold_duration(&self) -> u64 {
        self.sample_ms(self.click_hold_mean, self.click_hold_std, 40, 150)
    }

    /// 将鼠标大位移分解为小步骤（简化贝塞尔曲线）
    pub fn mouse_move_steps(&self, dx: i32, dy: i32) -> Vec<(i32, i32)> {
        let distance = ((dx * dx + dy * dy) as f64).sqrt();
        let num_steps = (distance / 5.0).max(3.0).min(50.0) as usize;

        let mut rng = rand::thread_rng();
        let mut steps = Vec::with_capacity(num_steps);
        let mut remaining_x = dx as f64;
        let mut remaining_y = dy as f64;

        for i in 0..num_steps {
            let progress = (i + 1) as f64 / num_steps as f64;
            // 添加轻微随机偏移
            let jitter_x: f64 = rng.gen_range(-2.0..2.0);
            let jitter_y: f64 = rng.gen_range(-2.0..2.0);

            let step_x = (remaining_x * progress + jitter_x) as i32;
            let step_y = (remaining_y * progress + jitter_y) as i32;

            remaining_x -= step_x as f64;
            remaining_y -= step_y as f64;

            if step_x != 0 || step_y != 0 {
                steps.push((step_x, step_y));
            }
        }

        // 补偿剩余距离
        let final_x = remaining_x.round() as i32;
        let final_y = remaining_y.round() as i32;
        if final_x != 0 || final_y != 0 {
            steps.push((final_x, final_y));
        }

        steps
    }

    /// 从正态分布采样并裁剪到范围内
    fn sample_ms(&self, mean: f64, std: f64, min: u64, max: u64) -> u64 {
        let normal = Normal::new(mean, std).unwrap_or(Normal::new(mean, 1.0).unwrap());
        let sample = normal.sample(&mut rand::thread_rng());
        (sample.round() as u64).clamp(min, max)
    }
}

impl Default for Humanizer {
    fn default() -> Self {
        Self::new()
    }
}
