//! uinput 内核级输入引擎
//!
//! 通过 /dev/uinput 创建虚拟键盘+鼠标设备，
//! 生成的事件无 Synthetic 标记，等同于真实硬件。

use anyhow::Result;
use tracing::{debug, info};

use crate::humanizer::Humanizer;

/// uinput 设备管理器
pub struct InputEngine {
    humanizer: Humanizer,
}

/// Linux input_event 结构体 (24 bytes on x86_64)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    r#type: u16,
    code: u16,
    value: i32,
}

// Linux input event types
#[allow(dead_code)]
const EV_SYN: u16 = 0x00;
#[allow(dead_code)]
const EV_KEY: u16 = 0x01;
#[allow(dead_code)]
const EV_REL: u16 = 0x02;
#[allow(dead_code)]
const SYN_REPORT: u16 = 0x00;

// Mouse relative axes
#[allow(dead_code)]
const REL_X: u16 = 0x00;
#[allow(dead_code)]
const REL_Y: u16 = 0x01;

// Mouse buttons
#[allow(dead_code)]
const BTN_LEFT: u16 = 0x110;

impl InputEngine {
    /// 创建输入引擎（需要 root 或 uinput 组权限）
    pub fn new() -> Result<Self> {
        info!("🎮 初始化 uinput 输入引擎...");

        Ok(Self {
            humanizer: Humanizer::new(),
        })
    }

    /// 初始化虚拟设备（需要在 Linux 上运行）
    pub fn init_devices(&mut self) -> Result<()> {
        // TODO: 通过 ioctl 创建虚拟键盘和鼠标
        // 设备名: "Logitech HID Virtual Input"
        // Vendor ID: 0x046d (Logitech)
        // Product ID: 0xc52b
        info!("⚠️ uinput 设备初始化 (TODO: 需要在 Linux 容器中运行)");
        Ok(())
    }

    /// 模拟按键（带拟人化延迟）
    pub async fn press_key(&self, keycode: u16) -> Result<()> {
        self.write_event(EV_KEY, keycode, 1)?;
        self.write_event(EV_SYN, SYN_REPORT, 0)?;

        let hold_ms = self.humanizer.key_hold_duration();
        tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;

        self.write_event(EV_KEY, keycode, 0)?;
        self.write_event(EV_SYN, SYN_REPORT, 0)?;

        Ok(())
    }

    /// 模拟打字（带拟人化字符间延迟）
    pub async fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if let Some(kc) = char_to_keycode(ch) {
                self.press_key(kc).await?;
                let delay_ms = self.humanizer.typing_delay();
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        Ok(())
    }

    /// 写入 input_event（低级接口）
    fn write_event(&self, r#type: u16, code: u16, value: i32) -> Result<()> {
        // TODO: 写入实际的虚拟设备文件
        debug!("input_event: type={type} code={code} value={value}");
        Ok(())
    }
}

/// 字符到 Linux keycode 映射（基础 ASCII）
#[allow(dead_code)]
fn char_to_keycode(ch: char) -> Option<u16> {
    match ch {
        'a'..='z' => Some(ch as u16 - 'a' as u16 + 30),
        'A'..='Z' => Some(ch as u16 - 'A' as u16 + 30),
        '0' => Some(11),
        '1'..='9' => Some(ch as u16 - '1' as u16 + 2),
        ' ' => Some(57),
        '\n' => Some(28),
        _ => None,
    }
}
