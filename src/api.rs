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
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, info};

use crate::WxMessage;

/// API 服务共享状态
struct AppState {
    /// 最近消息缓存
    recent_messages: RwLock<Vec<WxMessage>>,
    /// 广播通道: 向所有 WS 客户端推送
    ws_broadcast: broadcast::Sender<WxMessage>,
}

/// 启动 API 服务
pub async fn run(mut msg_rx: mpsc::Receiver<WxMessage>) -> anyhow::Result<()> {
    info!("🌐 API 服务启动中...");

    let (ws_tx, _) = broadcast::channel::<WxMessage>(128);

    let state = Arc::new(AppState {
        recent_messages: RwLock::new(Vec::new()),
        ws_broadcast: ws_tx.clone(),
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

async fn status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "running",
        "version": "0.1.0",
        "engine": "rust + zbus + atspi-rs + uinput"
    }))
}

async fn get_messages(State(state): State<Arc<AppState>>) -> Json<Vec<WxMessage>> {
    let cache = state.recent_messages.read().await;
    Json(cache.clone())
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    text: String,
}

#[derive(Serialize)]
struct SendResponse {
    success: bool,
    message: String,
}

async fn send_message(Json(req): Json<SendRequest>) -> Json<SendResponse> {
    // TODO Phase 4: 使用 AT-SPI2 导航 + uinput 输入
    info!("📤 发送请求: [{}] → {}", req.to, req.text);

    Json(SendResponse {
        success: false,
        message: "TODO: uinput 发送尚未实现".to_string(),
    })
}

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
