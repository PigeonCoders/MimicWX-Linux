//! MimicWX-Linux: 微信自动化框架
//!
//! 架构:
//! - atspi: AT-SPI2 底层原语 (D-Bus 通信) — 仅用于发送消息
//! - wechat: 微信业务逻辑 (控件查找、消息发送/验证、会话管理)
//! - chatwnd: 独立聊天窗口 (借鉴 wxauto ChatWnd)
//! - input: X11 XTEST 输入注入
//! - db: 数据库监听 (SQLCipher 解密 + inotify WAL 监听)
//! - api: HTTP/WebSocket API

mod atspi;
mod api;
mod chatwnd;
mod db;
mod input;
mod wechat;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

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

    info!("🚀 MimicWX-Linux v0.3.0 启动中...");

    // ① AT-SPI2 连接 (仍用于发送消息, 带重试)
    let atspi = loop {
        match atspi::AtSpi::connect().await {
            Ok(a) => {
                info!("✅ AT-SPI2 连接就绪");
                break Arc::new(a);
            }
            Err(e) => {
                info!("⚠️ AT-SPI2 连接失败: {}, 5秒后重试...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    // ② X11 XTEST 输入引擎 (仅发送消息需要, 非必须)
    let engine = match input::InputEngine::new() {
        Ok(e) => {
            info!("✅ X11 XTEST 输入引擎就绪");
            Some(e)
        }
        Err(e) => {
            info!("⚠️ X11 输入引擎不可用 (发送消息功能受限): {}", e);
            None
        }
    };

    // ③ WeChat 实例化 (AT-SPI 部分, 用于发送)
    let wechat = Arc::new(wechat::WeChat::new(atspi.clone()));

    // ④ 等待微信就绪
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
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            _ => {
                break;
            }
        }
    }

    // ⑤ 读取 GDB 提取的数据库密钥 + 初始化 DbManager
    let key_path = "/tmp/wechat_key.txt";
    for i in 0..10 {
        if std::path::Path::new(key_path).exists() {
            break;
        }
        if i == 0 {
            info!("🔑 等待 GDB 提取密钥...");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let db_manager: Option<Arc<db::DbManager>> = match std::fs::read_to_string(key_path) {
        Ok(key) => {
            let key = key.trim().to_string();
            if key.len() == 64 {
                info!("🔑 数据库密钥已获取 ({}...{})", &key[..8], &key[56..]);
                wechat.set_cipher_key(key.clone()).await;

                // 查找数据库目录
                let db_dir = find_db_dir();
                match db_dir {
                    Some(dir) => {
                        match db::DbManager::new(key, dir) {
                            Ok(mgr) => {
                                let mgr = Arc::new(mgr);
                                // 加载联系人
                                if let Err(e) = mgr.refresh_contacts().await {
                                    info!("⚠️ 联系人加载失败 (可能尚无数据): {}", e);
                                }
                                // 标记已有消息为已读
                                if let Err(e) = mgr.mark_all_read().await {
                                    info!("⚠️ 标记已读失败: {}", e);
                                }
                                Some(mgr)
                            }
                            Err(e) => {
                                info!("⚠️ DbManager 初始化失败: {}", e);
                                None
                            }
                        }
                    }
                    None => {
                        info!("⚠️ 未找到微信数据库目录, 数据库监听不可用");
                        None
                    }
                }
            } else {
                info!("⚠️ 密钥文件格式异常 (长度: {}), 跳过", key.len());
                None
            }
        }
        Err(_) => {
            info!("⚠️ 未找到密钥文件, 数据库解密功能不可用");
            None
        }
    };

    // ⑥ 广播通道 (WebSocket)
    let (tx, _) = tokio::sync::broadcast::channel::<String>(128);

    // ⑦ API 服务
    let state = Arc::new(api::AppState {
        wechat: wechat.clone(),
        atspi: atspi.clone(),
        engine: Mutex::new(engine),
        tx: tx.clone(),
        db: db_manager.clone(),
    });

    let app = api::build_router(state.clone());
    let addr = "0.0.0.0:8899";
    info!("🌐 API 服务启动: http://{addr}");
    info!("📡 WebSocket: ws://{addr}/ws");
    info!("📌 端点: /status, /contacts, /sessions, /messages/new, /send, /chat, /listen, /ws");

    // ⑧ 后台数据库消息监听任务
    if let Some(db) = db_manager {
        let listen_tx = tx.clone();

        // 启动 WAL inotify 监听
        let mut wal_rx = db.spawn_wal_watcher();

        tokio::spawn(async move {
            info!("👂 数据库消息监听启动 (inotify 驱动)");

            // 去抖动: WAL 可能短时间内触发多次事件
            let debounce = std::time::Duration::from_millis(500);

            loop {
                // 等待 WAL 变化通知
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    wal_rx.recv(),
                ).await {
                    Ok(Some(())) => {
                        // 去抖: 吃掉短时间内的后续事件
                        tokio::time::sleep(debounce).await;
                        while wal_rx.try_recv().is_ok() {}
                    }
                    Ok(None) => {
                        info!("❌ WAL 监听通道关闭");
                        break;
                    }
                    Err(_) => {
                        // 30s 超时也执行一次轮询 (fallback)
                    }
                }

                // 拉取新消息
                match db.get_new_messages().await {
                    Ok(msgs) => {
                        for m in &msgs {
                            let json = serde_json::json!({
                                "type": "db_message",
                                "chat": m.chat,
                                "chat_display": m.chat_display_name,
                                "talker": m.talker,
                                "talker_display": m.talker_display_name,
                                "content": m.content,
                                "msg_type": m.msg_type,
                                "create_time": m.create_time,
                                "local_id": m.local_id,
                            });
                            let _ = listen_tx.send(json.to_string());
                        }
                    }
                    Err(e) => {
                        tracing::debug!("📭 消息查询: {}", e);
                    }
                }
            }
        });
    } else {
        // Fallback: AT-SPI 轮询 (无数据库密钥时)
        let listen_wechat = wechat.clone();
        let listen_tx = tx.clone();
        tokio::spawn(async move {
            info!("👂 后台监听 (AT-SPI fallback 模式)");
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
    }

    // ⑨ 启动 HTTP 服务
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// 查找微信数据库目录
///
/// WeChat Linux 数据库路径 (实际):
/// ~/Documents/xwechat_files/wxid_xxx/db_storage
/// 当存在多个 wxid 时 (换账号), 选择最近修改的目录
fn find_db_dir() -> Option<PathBuf> {
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    // 搜索 /home/*/Documents/xwechat_files/*/db_storage
    for home_base in &["/home/wechat", &dirs_or_home().to_string_lossy().to_string()] {
        let xwechat_dir = PathBuf::from(home_base).join("Documents/xwechat_files");
        if let Ok(entries) = std::fs::read_dir(&xwechat_dir) {
            for entry in entries.flatten() {
                let db_storage = entry.path().join("db_storage");
                if db_storage.exists() {
                    // 用 message 子目录的修改时间来判断活跃账号
                    let msg_dir = db_storage.join("message");
                    let mtime = msg_dir.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    debug!("📂 候选: {} (mtime={:?})", db_storage.display(), mtime);
                    candidates.push((db_storage, mtime));
                }
            }
        }
    }

    // Fallback: 搜索所有 /home/*/Documents/xwechat_files/*/db_storage
    if candidates.is_empty() {
        if let Ok(homes) = std::fs::read_dir("/home") {
            for home in homes.flatten() {
                let xwechat_dir = home.path().join("Documents/xwechat_files");
                if let Ok(entries) = std::fs::read_dir(&xwechat_dir) {
                    for entry in entries.flatten() {
                        let db_storage = entry.path().join("db_storage");
                        if db_storage.exists() {
                            let msg_dir = db_storage.join("message");
                            let mtime = msg_dir.metadata()
                                .and_then(|m| m.modified())
                                .unwrap_or(std::time::UNIX_EPOCH);
                            candidates.push((db_storage, mtime));
                        }
                    }
                }
            }
        }
    }

    // 选择最新修改的目录 (活跃账号)
    if !candidates.is_empty() {
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        let chosen = &candidates[0].0;
        if candidates.len() > 1 {
            info!("📂 发现 {} 个账号目录, 选择最新的: {}", candidates.len(), chosen.display());
        } else {
            info!("📂 数据库目录: {}", chosen.display());
        }
        return Some(chosen.clone());
    }

    // 也尝试旧路径格式
    let old_path = PathBuf::from("/home/wechat/.local/share/weixin/data/db_storage");
    if old_path.exists() {
        info!("📂 数据库目录 (旧格式): {}", old_path.display());
        return Some(old_path);
    }

    None
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}
