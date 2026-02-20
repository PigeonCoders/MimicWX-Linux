//! uinput 内核级输入引擎
//!
//! 通过 /dev/uinput 创建虚拟键盘+鼠标设备，
//! 生成的事件无 Synthetic 标记，等同于真实硬件。
//!
//! 中文输入通过 xdotool（剪贴板方式）实现。

use anyhow::{Context, Result};
use evdev::{
    uinput::VirtualDeviceBuilder, AttributeSet, InputEvent, InputId, Key, RelativeAxisType,
};
use std::process::Command;
use tracing::{debug, info, warn};

use crate::humanizer::Humanizer;

/// 虚拟设备 ID (伪装为 Logitech HID 设备)
const VENDOR_ID: u16 = 0x046d;  // Logitech
const PRODUCT_ID: u16 = 0xc52b; // Unifying Receiver
const DEVICE_VERSION: u16 = 0x0111;

/// uinput 设备管理器
pub struct InputEngine {
    keyboard: evdev::uinput::VirtualDevice,
    mouse: evdev::uinput::VirtualDevice,
    humanizer: Humanizer,
}

impl InputEngine {
    /// 创建输入引擎（需要 root 或 input 组权限）
    pub fn new() -> Result<Self> {
        info!("🎮 初始化 uinput 输入引擎...");

        let keyboard = Self::create_keyboard()
            .context("创建虚拟键盘失败 (需要 /dev/uinput 权限)")?;
        let mouse = Self::create_mouse()
            .context("创建虚拟鼠标失败")?;

        info!("✅ 虚拟键盘+鼠标已创建");
        Ok(Self {
            keyboard,
            mouse,
            humanizer: Humanizer::new(),
        })
    }

    /// 创建虚拟键盘设备
    fn create_keyboard() -> Result<evdev::uinput::VirtualDevice> {
        let id = InputId::new(evdev::BusType::BUS_USB, VENDOR_ID, PRODUCT_ID, DEVICE_VERSION);

        // 注册所有常用按键
        let mut keys = AttributeSet::<Key>::new();
        // 字母键 A-Z
        for k in Key::KEY_A.code()..=Key::KEY_Z.code() {
            keys.insert(Key::new(k));
        }
        // 数字键 0-9
        for k in Key::KEY_0.code()..=Key::KEY_9.code() {
            keys.insert(Key::new(k));
        }
        // 功能键
        keys.insert(Key::KEY_SPACE);
        keys.insert(Key::KEY_ENTER);
        keys.insert(Key::KEY_BACKSPACE);
        keys.insert(Key::KEY_TAB);
        keys.insert(Key::KEY_ESC);
        keys.insert(Key::KEY_LEFTSHIFT);
        keys.insert(Key::KEY_RIGHTSHIFT);
        keys.insert(Key::KEY_LEFTCTRL);
        keys.insert(Key::KEY_RIGHTCTRL);
        keys.insert(Key::KEY_LEFTALT);
        keys.insert(Key::KEY_RIGHTALT);
        keys.insert(Key::KEY_DELETE);
        keys.insert(Key::KEY_HOME);
        keys.insert(Key::KEY_END);
        keys.insert(Key::KEY_UP);
        keys.insert(Key::KEY_DOWN);
        keys.insert(Key::KEY_LEFT);
        keys.insert(Key::KEY_RIGHT);
        // 标点
        keys.insert(Key::KEY_MINUS);
        keys.insert(Key::KEY_EQUAL);
        keys.insert(Key::KEY_LEFTBRACE);
        keys.insert(Key::KEY_RIGHTBRACE);
        keys.insert(Key::KEY_SEMICOLON);
        keys.insert(Key::KEY_APOSTROPHE);
        keys.insert(Key::KEY_GRAVE);
        keys.insert(Key::KEY_BACKSLASH);
        keys.insert(Key::KEY_COMMA);
        keys.insert(Key::KEY_DOT);
        keys.insert(Key::KEY_SLASH);
        // Ctrl+V 粘贴需要
        keys.insert(Key::KEY_V);

        let device = VirtualDeviceBuilder::new()?
            .name("Logitech HID Virtual Keyboard")
            .input_id(id)
            .with_keys(&keys)?
            .build()?;

        debug!("虚拟键盘已注册: Logitech HID Virtual Keyboard");
        Ok(device)
    }

    /// 创建虚拟鼠标设备
    fn create_mouse() -> Result<evdev::uinput::VirtualDevice> {
        let id = InputId::new(evdev::BusType::BUS_USB, VENDOR_ID, PRODUCT_ID, DEVICE_VERSION);

        // 鼠标按钮
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::BTN_LEFT);
        keys.insert(Key::BTN_RIGHT);
        keys.insert(Key::BTN_MIDDLE);

        // 相对轴 (鼠标移动)
        let mut axes = AttributeSet::<RelativeAxisType>::new();
        axes.insert(RelativeAxisType::REL_X);
        axes.insert(RelativeAxisType::REL_Y);
        axes.insert(RelativeAxisType::REL_WHEEL);

        let device = VirtualDeviceBuilder::new()?
            .name("Logitech HID Virtual Mouse")
            .input_id(id)
            .with_keys(&keys)?
            .with_relative_axes(&axes)?
            .build()?;

        debug!("虚拟鼠标已注册: Logitech HID Virtual Mouse");
        Ok(device)
    }

    // ================================================================
    // 键盘操作
    // ================================================================

    /// 模拟单次按键（按下 + 延迟 + 释放）
    pub async fn press_key(&mut self, key: Key) -> Result<()> {
        // Key down
        self.keyboard.emit(&[
            InputEvent::new(evdev::EventType::KEY, key.code(), 1),
        ])?;

        // 拟人化按压时长
        let hold_ms = self.humanizer.key_hold_duration();
        tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;

        // Key up
        self.keyboard.emit(&[
            InputEvent::new(evdev::EventType::KEY, key.code(), 0),
        ])?;

        debug!("⌨️ key {:?} (hold={}ms)", key, hold_ms);
        Ok(())
    }

    /// 模拟组合键 (如 Ctrl+V)
    pub async fn key_combo(&mut self, modifier: Key, key: Key) -> Result<()> {
        // Modifier down
        self.keyboard.emit(&[
            InputEvent::new(evdev::EventType::KEY, modifier.code(), 1),
        ])?;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Key press
        self.press_key(key).await?;

        // Modifier up
        self.keyboard.emit(&[
            InputEvent::new(evdev::EventType::KEY, modifier.code(), 0),
        ])?;

        debug!("⌨️ combo {:?}+{:?}", modifier, key);
        Ok(())
    }

    /// 输入 ASCII 文本 (逐字符通过 uinput)
    pub async fn type_ascii(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if let Some((key, shift)) = char_to_key(ch) {
                if shift {
                    // 需要 Shift 的字符
                    self.key_combo(Key::KEY_LEFTSHIFT, key).await?;
                } else {
                    self.press_key(key).await?;
                }
                // 拟人化打字间隔
                let delay_ms = self.humanizer.typing_delay();
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        Ok(())
    }

    /// 输入任意文本（中文/emoji 通过 xdotool 剪贴板，ASCII 通过 uinput）
    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        if text.is_ascii() {
            // 纯 ASCII → uinput 直接输入
            self.type_ascii(text).await
        } else {
            // 含中文 → xclip + Ctrl+V
            self.type_via_clipboard(text).await
        }
    }

    /// 通过剪贴板粘贴文本（支持中文）
    async fn type_via_clipboard(&mut self, text: &str) -> Result<()> {
        // 写入剪贴板
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("xclip 未安装")?;

        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;

        // 随机延迟后 Ctrl+V 粘贴
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.key_combo(Key::KEY_LEFTCTRL, Key::KEY_V).await?;

        info!("📋 粘贴文本: {} 字符", text.chars().count());
        Ok(())
    }

    // ================================================================
    // 鼠标操作
    // ================================================================

    /// 移动鼠标（相对位移，带拟人化轨迹）
    pub async fn move_mouse(&mut self, dx: i32, dy: i32) -> Result<()> {
        let steps = self.humanizer.mouse_move_steps(dx, dy);

        for (sx, sy) in &steps {
            self.mouse.emit(&[
                InputEvent::new(evdev::EventType::RELATIVE, RelativeAxisType::REL_X.0, *sx),
                InputEvent::new(evdev::EventType::RELATIVE, RelativeAxisType::REL_Y.0, *sy),
            ])?;

            let delay_ms = self.humanizer.mouse_step_delay();
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        debug!("🖱️ move ({}, {}) in {} steps", dx, dy, steps.len());
        Ok(())
    }

    /// 鼠标单击（通过 xdotool 绝对坐标定位 + uinput 点击）
    pub async fn click(&mut self, x: i32, y: i32) -> Result<()> {
        // 先用 xdotool 移动到绝对坐标
        Command::new("xdotool")
            .args(["mousemove", &x.to_string(), &y.to_string()])
            .output()
            .context("xdotool mousemove 失败")?;

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // uinput 按下鼠标左键
        self.mouse.emit(&[
            InputEvent::new(evdev::EventType::KEY, Key::BTN_LEFT.code(), 1),
        ])?;

        let hold_ms = self.humanizer.click_hold_duration();
        tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;

        // uinput 释放鼠标左键
        self.mouse.emit(&[
            InputEvent::new(evdev::EventType::KEY, Key::BTN_LEFT.code(), 0),
        ])?;

        info!("🖱️ click ({}, {}) hold={}ms", x, y, hold_ms);
        Ok(())
    }

    /// 鼠标双击
    pub async fn double_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.click(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        self.click(x, y).await?;
        Ok(())
    }

    /// 发送 Enter 键
    pub async fn press_enter(&mut self) -> Result<()> {
        self.press_key(Key::KEY_ENTER).await
    }
}

// ================================================================
// 字符到 Key 映射
// ================================================================

/// 将字符映射为 (Key, 是否需要 Shift)
fn char_to_key(ch: char) -> Option<(Key, bool)> {
    match ch {
        'a'..='z' => {
            let offset = ch as u16 - 'a' as u16;
            Some((Key::new(Key::KEY_A.code() + offset), false))
        }
        'A'..='Z' => {
            let offset = ch as u16 - 'A' as u16;
            Some((Key::new(Key::KEY_A.code() + offset), true))
        }
        '0' => Some((Key::KEY_0, false)),
        '1'..='9' => {
            let offset = ch as u16 - '1' as u16;
            Some((Key::new(Key::KEY_1.code() + offset), false))
        }
        ' ' => Some((Key::KEY_SPACE, false)),
        '\n' => Some((Key::KEY_ENTER, false)),
        '\t' => Some((Key::KEY_TAB, false)),
        '-' => Some((Key::KEY_MINUS, false)),
        '=' => Some((Key::KEY_EQUAL, false)),
        '[' => Some((Key::KEY_LEFTBRACE, false)),
        ']' => Some((Key::KEY_RIGHTBRACE, false)),
        '\\' => Some((Key::KEY_BACKSLASH, false)),
        ';' => Some((Key::KEY_SEMICOLON, false)),
        '\'' => Some((Key::KEY_APOSTROPHE, false)),
        '`' => Some((Key::KEY_GRAVE, false)),
        ',' => Some((Key::KEY_COMMA, false)),
        '.' => Some((Key::KEY_DOT, false)),
        '/' => Some((Key::KEY_SLASH, false)),
        // Shift variants
        '!' => Some((Key::KEY_1, true)),
        '@' => Some((Key::KEY_2, true)),
        '#' => Some((Key::KEY_3, true)),
        '$' => Some((Key::KEY_4, true)),
        '%' => Some((Key::KEY_5, true)),
        '^' => Some((Key::KEY_6, true)),
        '&' => Some((Key::KEY_7, true)),
        '*' => Some((Key::KEY_8, true)),
        '(' => Some((Key::KEY_9, true)),
        ')' => Some((Key::KEY_0, true)),
        '_' => Some((Key::KEY_MINUS, true)),
        '+' => Some((Key::KEY_EQUAL, true)),
        '{' => Some((Key::KEY_LEFTBRACE, true)),
        '}' => Some((Key::KEY_RIGHTBRACE, true)),
        '|' => Some((Key::KEY_BACKSLASH, true)),
        ':' => Some((Key::KEY_SEMICOLON, true)),
        '"' => Some((Key::KEY_APOSTROPHE, true)),
        '~' => Some((Key::KEY_GRAVE, true)),
        '<' => Some((Key::KEY_COMMA, true)),
        '>' => Some((Key::KEY_DOT, true)),
        '?' => Some((Key::KEY_SLASH, true)),
        _ => {
            warn!("未映射字符: {:?} (U+{:04X})", ch, ch as u32);
            None
        }
    }
}
