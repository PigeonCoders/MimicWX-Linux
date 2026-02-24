//! MimicWX-Linux: 微信自动化框架
//!
//! 架构:
//! - atspi: AT-SPI2 底层原语 (D-Bus 通信)
//! - wechat: 微信业务逻辑 (控件查找、消息发送/验证、会话管理)
//! - chatwnd: 独立聊天窗口 (借鉴 wxauto ChatWnd)
//! - input: X11 XTEST 输入注入
//! - api: HTTP/WebSocket API

mod atspi;
mod api;
mod chatwnd;
mod input;
mod wechat;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

/// 统一消息类型 (用于 WebSocket 推送)
#[derive(Debug, Clone, serde::Serialize)]
pub struct WxMessage {
    pub sender: String,
    pub text: String,
    pub timestamp: u64,
    pub source: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mimicwx=debug,tower_http=info".into()),
        )
        .init();

    info!("🚀 MimicWX-Linux v0.2.0 启动中...");

    // ① AT-SPI2 连接
    let atspi = Arc::new(atspi::AtSpi::connect().await?);
    info!("✅ AT-SPI2 连接就绪");

    // ② X11 XTEST 输入引擎
    let engine = input::InputEngine::new()?;
    info!("✅ X11 XTEST 输入引擎就绪");

    // ③ WeChat 实例化
    let wechat = Arc::new(wechat::WeChat::new(atspi.clone()));

    // ④ 等待微信就绪 (精简日志: 只提示一次)
    let mut attempts = 0;
    let mut login_prompted = false;
    loop {
        let status = wechat.check_status().await;
        match status {
            wechat::WeChatStatus::LoggedIn => {
                info!("✅ 微信已登录");
                break;
            }
            wechat::WeChatStatus::NotRunning if attempts < 30 => {
                info!("⏳ 等待微信启动... ({}/30)", attempts + 1);
                if attempts % 5 == 4 {
                    wechat.try_reconnect().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                attempts += 1;
            }
            wechat::WeChatStatus::WaitingForLogin => {
                if !login_prompted {
                    info!("📱 请通过 noVNC (http://localhost:6080/vnc.html) 扫码登录微信");
                    info!("🔑 GDB 密钥提取已在后台运行, 登录后将自动获取数据库密钥");
                    login_prompted = true;
                }
                // 静默等待, 不再重复输出
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            _ => {
                // 即使未登录也启动 API 服务
                break;
            }
        }
    }

    // ④.5 读取 GDB 提取的数据库密钥
    let key_path = "/tmp/wechat_key.txt";
    // 等待密钥文件生成 (GDB 可能比 AT-SPI2 检测稍慢)
    for i in 0..10 {
        if std::path::Path::new(key_path).exists() {
            break;
        }
        if i == 0 {
            info!("🔑 等待 GDB 提取密钥...");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    match std::fs::read_to_string(key_path) {
        Ok(key) => {
            let key = key.trim().to_string();
            if key.len() == 64 {
                info!("🔑 数据库密钥已获取 ({}...{})", &key[..8], &key[56..]);
                wechat.set_cipher_key(key).await;
            } else {
                info!("⚠️ 密钥文件格式异常 (长度: {}), 跳过", key.len());
            }
        }
        Err(_) => {
            info!("⚠️ 未找到密钥文件, 数据库解密功能不可用");
        }
    }

    // ⑤ 标记已有消息为已读
    wechat.mark_all_read().await;

    // ⑥ 广播通道 (WebSocket)
    let (tx, _) = tokio::sync::broadcast::channel::<String>(128);

    // ⑦ API 服务
    let state = Arc::new(api::AppState {
        wechat: wechat.clone(),
        atspi: atspi.clone(),
        engine: Mutex::new(engine),
        tx: tx.clone(),
    });

    let app = api::build_router(state.clone());
    let addr = "0.0.0.0:8899";
    info!("🌐 API 服务启动: http://{addr}");
    info!("📡 WebSocket: ws://{addr}/ws");
    info!("📌 新增端点: /sessions, /chat, /listen, /listen/messages");

    // ⑧ 后台监听轮询任务
    let listen_wechat = wechat.clone();
    let listen_tx = tx.clone();
    tokio::spawn(async move {
        info!("👂 后台监听轮询任务启动");
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            interval.tick().await;
            let msgs = listen_wechat.get_listen_messages().await;
            for (who, new_msgs) in &msgs {
                for m in new_msgs {
                    let json = serde_json::json!({
                        "type": "listen_message",
                        "from": who,
                        "msg_type": m.msg_type,
                        "sender": m.sender,
                        "content": m.content,
                    });
                    let _ = listen_tx.send(json.to_string());
                }
            }
        }
    });

    // ⑨ 启动 HTTP 服务
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
