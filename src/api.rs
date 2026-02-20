//! HTTP/WebSocket API 服务
//!
//! 提供 OneBot v11 兼容的消息接口，
//! 同时用 WebSocket 推送实时消息。

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::WxMessage;

#[cfg(target_os = "linux")]
use crate::input::InputEngine;

// ================================================================
// 共享状态
// ================================================================

/// API 服务共享状态
struct AppState {
    /// 最近消息缓存
    recent_messages: RwLock<Vec<WxMessage>>,
    /// 广播通道: 向所有 WS 客户端推送
    ws_broadcast: broadcast::Sender<WxMessage>,
    /// 输入引擎 (uinput)
    #[cfg(target_os = "linux")]
    input_engine: Option<Arc<Mutex<InputEngine>>>,
    #[cfg(not(target_os = "linux"))]
    input_engine: Option<Arc<Mutex<()>>>,
}

// ================================================================
// 启动入口
// ================================================================

/// 启动 API 服务
#[cfg(target_os = "linux")]
pub async fn run(
    mut msg_rx: mpsc::Receiver<WxMessage>,
    input_engine: Option<Arc<Mutex<InputEngine>>>,
) -> anyhow::Result<()> {
    run_inner(msg_rx, input_engine).await
}

#[cfg(not(target_os = "linux"))]
pub async fn run(
    mut msg_rx: mpsc::Receiver<WxMessage>,
    input_engine: Option<Arc<Mutex<()>>>,
) -> anyhow::Result<()> {
    run_inner(msg_rx, input_engine).await
}

async fn run_inner(
    mut msg_rx: mpsc::Receiver<WxMessage>,
    #[cfg(target_os = "linux")] input_engine: Option<Arc<Mutex<InputEngine>>>,
    #[cfg(not(target_os = "linux"))] input_engine: Option<Arc<Mutex<()>>>,
) -> anyhow::Result<()> {
    info!("🌐 API 服务启动中...");

    let (ws_tx, _) = broadcast::channel::<WxMessage>(128);

    let state = Arc::new(AppState {
        recent_messages: RwLock::new(Vec::new()),
        ws_broadcast: ws_tx.clone(),
        input_engine,
    });

    // 消息转发任务: mpsc → 缓存 + 广播
    let forward_state = state.clone();
    tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            info!("📨 收到消息 [{}]: {}", msg.source, msg.text);

            // 缓存
            {
                let mut cache = forward_state.recent_messages.write().await;
                cache.push(msg.clone());
                // 保留最近 100 条
                let len = cache.len();
                if len > 100 {
                    cache.drain(0..len - 100);
                }
            }

            // 广播到所有 WS 客户端
            let _ = ws_tx.send(msg);
        }
    });

    // 路由
    let app = Router::new()
        .route("/", get(index))
        .route("/status", get(status))
        .route("/messages", get(get_messages))
        .route("/send", post(send_message))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8899").await?;
    info!("✅ API 服务就绪: http://0.0.0.0:8899");

    axum::serve(listener, app).await?;
    Ok(())
}

// ================================================================
// Handlers
// ================================================================

async fn index() -> &'static str {
    "MimicWX-Linux API v0.1.0 (Rust)"
}

async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let has_input = state.input_engine.is_some();
    Json(serde_json::json!({
        "status": "running",
        "version": "0.1.0",
        "engine": "rust + zbus + atspi-rs + uinput",
        "input_engine": has_input,
    }))
}

async fn get_messages(State(state): State<Arc<AppState>>) -> Json<Vec<WxMessage>> {
    let cache = state.recent_messages.read().await;
    Json(cache.clone())
}

// ================================================================
// 发送消息
// ================================================================

#[derive(Deserialize)]
struct SendRequest {
    /// 目标联系人/群名
    to: String,
    /// 消息内容
    text: String,
}

#[derive(Serialize)]
struct SendResponse {
    success: bool,
    message: String,
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendRequest>,
) -> Json<SendResponse> {
    info!("📤 发送请求: [{}] → {}", req.to, req.text);

    #[cfg(target_os = "linux")]
    {
        let Some(ref engine) = state.input_engine else {
            return Json(SendResponse {
                success: false,
                message: "InputEngine 未初始化 (uinput 不可用)".into(),
            });
        };

        let engine = engine.clone();
        let to = req.to;
        let text = req.text;

        // 在独立任务中执行输入操作 (因为涉及 sleep)
        let result = tokio::spawn(async move {
            let mut eng = engine.lock().await;
            send_message_impl(&mut eng, &to, &text).await
        }).await;

        match result {
            Ok(Ok(())) => Json(SendResponse {
                success: true,
                message: "消息已发送".into(),
            }),
            Ok(Err(e)) => Json(SendResponse {
                success: false,
                message: format!("发送失败: {e}"),
            }),
            Err(e) => Json(SendResponse {
                success: false,
                message: format!("任务异常: {e}"),
            }),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Json(SendResponse {
            success: false,
            message: "非 Linux 环境，无法发送".into(),
        })
    }
}

/// 实际发送消息的实现
///
/// 流程:
/// 1. 在搜索框搜索联系人
/// 2. 点击搜索结果
/// 3. 在消息输入框输入文本
/// 4. 按 Enter 发送
#[cfg(target_os = "linux")]
async fn send_message_impl(
    engine: &mut InputEngine,
    to: &str,
    text: &str,
) -> anyhow::Result<()> {
    use evdev::Key;

    info!("📤 [send] 开始发送: [{}] → {}", to, text);

    // Step 1: Ctrl+F 打开搜索框 (微信 Linux 快捷键)
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    engine.key_combo(Key::KEY_LEFTCTRL, Key::KEY_F).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Step 2: 输入联系人名称
    engine.type_text(to).await?;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    // Step 3: 按 Enter 选择第一个搜索结果
    engine.press_enter().await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Step 4: 按 Esc 关闭搜索面板
    engine.press_key(Key::KEY_ESC).await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Step 5: 在消息框输入文本
    engine.type_text(text).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Step 6: Enter 发送
    engine.press_enter().await?;

    info!("✅ [send] 消息已发送: [{}]", to);
    Ok(())
}

// ================================================================
// WebSocket
// ================================================================

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    info!("🔌 WebSocket 客户端已连接");

    let mut rx = state.ws_broadcast.subscribe();

    loop {
        tokio::select! {
            // 推送新消息给客户端
            Ok(msg) = rx.recv() => {
                let json = serde_json::to_string(&msg).unwrap_or_default();
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            // 接收客户端消息 (可扩展为命令)
            Some(Ok(client_msg)) = socket.recv() => {
                match client_msg {
                    Message::Text(text) => {
                        debug!("WS 收到: {text}");
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            else => break,
        }
    }

    info!("🔌 WebSocket 客户端断开");
}
