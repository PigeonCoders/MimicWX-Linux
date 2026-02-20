//! MimicWX-Linux: Zero-risk WeChat automation framework
//!
//! Architecture:
//! - AT-SPI2 accessibility tree monitoring for message detection
//! - uinput kernel-level input simulation
//! - axum HTTP/WebSocket API (OneBot v11)

#[cfg(target_os = "linux")]
mod a11y;
mod api;
mod humanizer;
#[cfg(target_os = "linux")]
mod input;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// 统一消息类型，各子系统通过 channel 传递
#[derive(Debug, Clone, serde::Serialize)]
pub struct WxMessage {
    /// 发送者名称
    pub sender: String,
    /// 消息文本
    pub text: String,
    /// 时间戳 (Unix ms)
    pub timestamp: u64,
    /// 来源: "atspi"
    pub source: String,
}

#[tokio::main]
async fn main() {
    eprintln!("[mimicwx] binary starting...");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mimicwx=info".into()),
        )
        .init();

    if let Err(e) = run().await {
        eprintln!("[mimicwx] FATAL: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    info!("🚀 MimicWX-Linux starting...");

    let (msg_tx, msg_rx) = mpsc::channel::<WxMessage>(256);

    // === 初始化 InputEngine ===
    #[cfg(target_os = "linux")]
    let input_engine = {
        match input::InputEngine::new() {
            Ok(engine) => {
                info!("🎮 InputEngine 就绪");
                Some(std::sync::Arc::new(tokio::sync::Mutex::new(engine)))
            }
            Err(e) => {
                warn!("⚠️ InputEngine 初始化失败: {e}");
                warn!("   发送消息功能将不可用，但消息检测正常");
                None
            }
        }
    };

    #[cfg(not(target_os = "linux"))]
    let input_engine: Option<std::sync::Arc<tokio::sync::Mutex<()>>> = {
        warn!("⚠️ Not on Linux — InputEngine disabled");
        None
    };

    // === 启动 AT-SPI2 监听器 ===
    #[cfg(target_os = "linux")]
    {
        let atspi_tx = msg_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = a11y::run(atspi_tx).await {
                error!("AT-SPI2 监听器异常: {e}");
            }
        });
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn!("⚠️ Not running on Linux — AT-SPI2 listener disabled");
    }

    drop(msg_tx);

    info!("✅ MimicWX-Linux ready");
    info!("   API: http://0.0.0.0:8899");

    api::run(msg_rx, input_engine).await?;

    Ok(())
}
