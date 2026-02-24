//! HTTP API 服务
//!
//! 提供 REST + WebSocket 接口:
//! - GET  /status        — 服务状态
//! - GET  /contacts      — 联系人列表 (数据库)
//! - GET  /sessions      — 会话列表 (优先数据库)
//! - GET  /messages      — 当前聊天全部消息
//! - GET  /messages/new  — 增量新消息 (优先数据库)
//! - POST /send          — 发送消息 (AT-SPI)
//! - POST /chat          — 切换聊天 (AT-SPI)
//! - POST /listen        — 添加监听 (弹出独立窗口)
//! - DELETE /listen      — 移除监听
//! - GET  /listen        — 监听列表
//! - GET  /listen/messages — 所有监听窗口的新消息
//! - GET  /debug/tree    — AT-SPI2 控件树
//! - GET  /ws            — WebSocket 实时推送

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::info;

use crate::atspi::AtSpi;
use crate::db::DbManager;
use crate::input::InputEngine;
use crate::wechat::WeChat;

// =====================================================================
// 共享状态
// =====================================================================

pub struct AppState {
    pub wechat: Arc<WeChat>,
    pub atspi: Arc<AtSpi>,
    pub engine: Mutex<Option<InputEngine>>,
    pub tx: broadcast::Sender<String>,
    /// 数据库管理器 (密钥获取成功时可用)
    pub db: Option<Arc<DbManager>>,
}

// =====================================================================
// 路由
// =====================================================================

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // 基础
        .route("/status", get(get_status))
        .route("/contacts", get(get_contacts))
        .route("/messages", get(get_messages))
        .route("/messages/new", get(get_new_messages))
        .route("/send", post(send_message))
        // 会话管理
        .route("/sessions", get(get_sessions))
        .route("/chat", post(chat_with))
        // 监听管理
        .route("/listen", get(get_listen_list))
        .route("/listen", post(add_listen))
        .route("/listen", delete(remove_listen))
        .route("/listen/messages", get(get_listen_messages))
        // 调试
        .route("/debug/tree", get(get_tree))
        .route("/debug/sessions", get(get_session_tree))
        // WebSocket
        .route("/ws", get(ws_handler))
        .with_state(state)
}

// =====================================================================
// 请求/响应类型
// =====================================================================

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    version: String,
    listen_count: usize,
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    text: String,
}

#[derive(Serialize)]
struct SendResponse {
    sent: bool,
    verified: bool,
    message: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    who: String,
}

#[derive(Serialize)]
struct ChatResponse {
    success: bool,
    chat_name: Option<String>,
}

#[derive(Deserialize)]
struct ListenRequest {
    who: String,
}

#[derive(Serialize)]
struct ListenResponse {
    success: bool,
    message: String,
}

// =====================================================================
// Handlers
// =====================================================================

async fn get_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let status = state.wechat.check_status().await;
    let listen_count = state.wechat.get_listen_list().await.len();
    Json(StatusResponse {
        status: status.to_string(),
        version: "0.3.0".into(),
        listen_count,
    })
}

/// 联系人列表 (从数据库)
async fn get_contacts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(db) = &state.db {
        let contacts = db.get_contacts().await;
        Json(serde_json::json!({ "contacts": contacts }))
    } else {
        Json(serde_json::json!({ "error": "数据库不可用" }))
    }
}

async fn get_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let msgs = state.wechat.get_all_messages().await;
    Json(msgs)
}

async fn get_new_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 优先使用数据库
    if let Some(db) = &state.db {
        match db.get_new_messages().await {
            Ok(msgs) => return Json(serde_json::to_value(msgs).unwrap_or_default()),
            Err(e) => {
                tracing::warn!("数据库消息查询失败, fallback AT-SPI: {}", e);
            }
        }
    }
    // Fallback: AT-SPI
    let msgs = state.wechat.get_new_messages().await;
    Json(serde_json::to_value(msgs).unwrap_or_default())
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendRequest>,
) -> Json<SendResponse> {
    let mut guard = state.engine.lock().await;
    let engine = match guard.as_mut() {
        Some(e) => e,
        None => return Json(SendResponse {
            sent: false, verified: false,
            message: "X11 输入引擎不可用, 无法发送消息".into(),
        }),
    };
    match state.wechat.send_message(engine, &req.to, &req.text).await {
        Ok((sent, verified, message)) => {
            let msg_json = serde_json::json!({
                "type": "sent",
                "to": req.to,
                "text": req.text,
                "verified": verified,
            });
            let _ = state.tx.send(msg_json.to_string());
            Json(SendResponse { sent, verified, message })
        }
        Err(e) => Json(SendResponse {
            sent: false,
            verified: false,
            message: format!("发送失败: {e}"),
        }),
    }
}

async fn get_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 优先使用数据库
    if let Some(db) = &state.db {
        match db.get_sessions().await {
            Ok(sessions) => return Json(serde_json::to_value(sessions).unwrap_or_default()),
            Err(e) => {
                tracing::warn!("数据库会话查询失败, fallback AT-SPI: {}", e);
            }
        }
    }
    // Fallback: AT-SPI
    let sessions = state.wechat.list_sessions().await;
    Json(serde_json::to_value(sessions).unwrap_or_default())
}

async fn chat_with(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Json<ChatResponse> {
    let mut guard = state.engine.lock().await;
    let engine = match guard.as_mut() {
        Some(e) => e,
        None => return Json(ChatResponse { success: false, chat_name: None }),
    };
    match state.wechat.chat_with(engine, &req.who).await {
        Ok(Some(name)) => Json(ChatResponse { success: true, chat_name: Some(name) }),
        Ok(None) => Json(ChatResponse { success: false, chat_name: None }),
        Err(_) => Json(ChatResponse { success: false, chat_name: None }),
    }
}

async fn add_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Json<ListenResponse> {
    let mut guard = state.engine.lock().await;
    let engine = match guard.as_mut() {
        Some(e) => e,
        None => return Json(ListenResponse {
            success: false,
            message: "X11 输入引擎不可用".into(),
        }),
    };
    match state.wechat.add_listen(engine, &req.who).await {
        Ok(true) => Json(ListenResponse {
            success: true,
            message: format!("已添加监听: {}", req.who),
        }),
        Ok(false) => Json(ListenResponse {
            success: false,
            message: format!("添加监听失败: {}", req.who),
        }),
        Err(e) => Json(ListenResponse {
            success: false,
            message: format!("添加监听错误: {e}"),
        }),
    }
}

async fn remove_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Json<ListenResponse> {
    let guard = state.engine.lock().await;
    let removed = if let Some(engine) = guard.as_ref() {
        state.wechat.remove_listen(engine, &req.who).await
    } else {
        false
    };
    Json(ListenResponse {
        success: removed,
        message: if removed {
            format!("已移除监听: {}", req.who)
        } else {
            format!("未找到监听: {}", req.who)
        },
    })
}

async fn get_listen_list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let list = state.wechat.get_listen_list().await;
    Json(list)
}

async fn get_listen_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let msgs = state.wechat.take_pending_messages().await;

    // 推送到 WebSocket
    for (who, new_msgs) in &msgs {
        for m in new_msgs {
            let msg_json = serde_json::json!({
                "type": "listen_message",
                "from": who,
                "msg_type": m.msg_type,
                "sender": m.sender,
                "content": m.content,
            });
            let _ = state.tx.send(msg_json.to_string());
        }
    }

    Json(msgs)
}

async fn get_tree(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let max_depth = params.get("depth")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(5)
        .min(15);
    if let Some(app) = state.wechat.find_app().await {
        let tree = state.atspi.dump_tree(&app, max_depth).await;
        Json(tree)
    } else {
        Json(vec![])
    }
}

/// 只 dump 会话容器的子树 (用于调试)
async fn get_session_tree(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(app) = state.wechat.find_app().await {
        if let Some(container) = state.wechat.find_session_list(&app).await {
            let tree = state.atspi.dump_tree(&container, 4).await;
            return Json(tree);
        }
    }
    Json(vec![])
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    info!("🔌 WebSocket 连接建立");

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    info!("🔌 WebSocket 连接断开");
}
