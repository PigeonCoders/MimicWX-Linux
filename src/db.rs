//! 数据库监听模块
//!
//! 通过 SQLCipher 解密 + fanotify 监听 WAL 文件变化，实现:
//! - 联系人查询 (contact.db)
//! - 会话列表 (session.db)
//! - 增量消息获取 (message_0.db)
//!
//! 替代原有 AT-SPI2 轮询方案，完全非侵入。
//!
//! v0.4.0 优化: fanotify + PID 过滤替代 inotify (消除自循环冷却期),
//!             持久化 message_0.db 连接 (消除每次 PBKDF2 开销).
//!
//! 设计: rusqlite::Connection 是 !Send, 不能跨 .await 持有。
//! 策略: 所有 DB 操作在 spawn_blocking 中完成, 异步方法只操作缓存。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

// =====================================================================
// FFI: sqlite3_key (WCDB 密钥传递方式)
// =====================================================================

extern "C" {
    /// WCDB 使用 sqlite3_key() C API 传递 raw key (非 PRAGMA key).
    /// SQLCipher 会对这个 key 做 PBKDF2 派生.
    fn sqlite3_key(
        db: *mut std::ffi::c_void,
        key: *const u8,
        key_len: std::ffi::c_int,
    ) -> std::ffi::c_int;
}

// =====================================================================
// 类型定义
// =====================================================================

/// 联系人信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContactInfo {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    /// 优先显示名: remark > nick_name > username
    pub display_name: String,
}

/// 会话信息 (来自数据库)
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbSessionInfo {
    pub username: String,
    pub display_name: String,
    pub unread_count: i32,
    pub summary: String,
    pub last_timestamp: i64,
    pub last_msg_sender: String,
}

/// 数据库消息
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbMessage {
    pub local_id: i64,
    pub server_id: i64,
    pub create_time: i64,
    pub content: String,
    pub msg_type: i32,
    /// 发言人 wxid (群聊中有意义)
    pub talker: String,
    /// 发言人显示名 (通过联系人缓存解析)
    pub talker_display_name: String,
    /// 所属会话
    pub chat: String,
    /// 所属会话显示名
    pub chat_display_name: String,
}

/// 原始消息 (同步查询返回, 后续异步填充显示名)
struct RawMsg {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    content: String,
    msg_type: i32,
    talker: String,
    chat: String,
}

// =====================================================================
// DbManager — 核心结构
// =====================================================================

pub struct DbManager {
    /// 32 字节原始密钥
    key_bytes: Vec<u8>,
    /// 数据库存储目录 (如 /home/wechat/.local/share/weixin/db_storage/)
    db_dir: PathBuf,
    /// 联系人缓存: username → ContactInfo
    contacts: Mutex<HashMap<String, ContactInfo>>,
    /// 高水位线: ChatMsg 表名 → 最大 local_id
    watermarks: Mutex<HashMap<String, i64>>,
    /// 持久化 message_0.db 连接 (避免每次查询重做 PBKDF2 ~500ms)
    msg_conn: std::sync::Mutex<Option<Connection>>,
}

impl DbManager {
    /// 创建 DbManager
    pub fn new(key_hex: String, db_dir: PathBuf) -> Result<Self> {
        let key_bytes = hex_to_bytes(&key_hex)
            .context("密钥 hex 格式错误")?;
        anyhow::ensure!(key_bytes.len() == 32, "密钥长度必须为 32 字节, 实际: {}", key_bytes.len());

        info!("📦 DbManager 初始化: db_dir={}", db_dir.display());

        // 尝试建立持久化 message_0.db 连接
        let msg_conn = match Self::open_db(&key_bytes, &db_dir, "message/message_0.db") {
            Ok(conn) => {
                info!("🔗 message_0.db 持久连接已建立");
                Some(conn)
            }
            Err(e) => {
                info!("⚠️ message_0.db 暂不可用 (将在首次查询时重试): {}", e);
                None
            }
        };

        Ok(Self {
            key_bytes,
            db_dir,
            contacts: Mutex::new(HashMap::new()),
            watermarks: Mutex::new(HashMap::new()),
            msg_conn: std::sync::Mutex::new(msg_conn),
        })
    }

    // =================================================================
    // 数据库连接 (同步, 在 spawn_blocking 中调用)
    // =================================================================

    /// 打开加密数据库 (只读模式)
    fn open_db(key_bytes: &[u8], db_dir: &Path, db_name: &str) -> Result<Connection> {
        let path = db_dir.join(db_name);
        anyhow::ensure!(path.exists(), "数据库不存在: {}", path.display());

        // WAL 模式下必须用 READ_WRITE 才能读到 WAL 中未 checkpoint 的新数据
        // 配合 PRAGMA query_only=ON 防止意外写入
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).with_context(|| format!("打开数据库失败: {}", path.display()))?;

        // 通过 FFI 调用 sqlite3_key() 传递 raw key
        let rc = unsafe {
            let handle = conn.handle();
            sqlite3_key(
                handle as *mut std::ffi::c_void,
                key_bytes.as_ptr(),
                key_bytes.len() as std::ffi::c_int,
            )
        };
        anyhow::ensure!(rc == 0, "sqlite3_key() 失败, rc={}", rc);

        conn.execute_batch("PRAGMA cipher_compatibility = 4;")?;
        // 安全防护: 不触发 checkpoint, 不写入数据
        conn.execute_batch("PRAGMA wal_autocheckpoint = 0;")?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        // 防御性: 遇到写锁时等待最多 5 秒, 而非直接报错
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;

        // 验证解密成功
        let count: i32 = conn.query_row(
            "SELECT count(*) FROM sqlite_master", [], |row| row.get(0),
        ).with_context(|| format!("数据库解密验证失败: {}", db_name))?;

        trace!("🔓 {} 解密成功, {} 个表", db_name, count);
        Ok(conn)
    }

    /// 确保 message_0.db 持久连接可用 (如首次不可用则重建)
    fn ensure_msg_conn(&self) -> Result<std::sync::MutexGuard<'_, Option<Connection>>> {
        let mut guard = self.msg_conn.lock().map_err(|e| anyhow::anyhow!("msg_conn lock poisoned: {}", e))?;
        if guard.is_none() {
            info!("🔗 重建 message_0.db 持久连接...");
            *guard = Some(Self::open_db(&self.key_bytes, &self.db_dir, "message/message_0.db")?);
        }
        Ok(guard)
    }

    // =================================================================
    // 联系人
    // =================================================================

    /// 加载/刷新联系人缓存 (spawn_blocking 中执行 DB 查询)
    pub async fn refresh_contacts(&self) -> Result<usize> {
        let key = self.key_bytes.clone();
        let dir = self.db_dir.clone();

        let contacts = tokio::task::spawn_blocking(move || -> Result<Vec<ContactInfo>> {
            let conn = Self::open_db(&key, &dir, "contact/contact.db")?;
            let mut stmt = conn.prepare(
                "SELECT username, nick_name, remark, alias FROM contact"
            )?;
            let result: Vec<ContactInfo> = stmt.query_map([], |row| {
                let username: String = row.get(0)?;
                let nick_name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                let remark: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
                let alias: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                let display_name = if !remark.is_empty() {
                    remark.clone()
                } else if !nick_name.is_empty() {
                    nick_name.clone()
                } else {
                    username.clone()
                };
                Ok(ContactInfo { username, nick_name, remark, alias, display_name })
            })?.filter_map(|r| r.ok()).collect();
            Ok(result)
        }).await??;

        let count = contacts.len();
        let mut cache = self.contacts.lock().await;
        cache.clear();
        for c in contacts {
            cache.insert(c.username.clone(), c);
        }
        info!("👥 联系人缓存: {} 条", count);
        Ok(count)
    }

    /// 获取联系人列表
    pub async fn get_contacts(&self) -> Vec<ContactInfo> {
        self.contacts.lock().await.values().cloned().collect()
    }

    /// 通过 username 获取显示名
    async fn resolve_name(&self, username: &str) -> String {
        self.contacts.lock().await
            .get(username)
            .map(|c| c.display_name.clone())
            .unwrap_or_else(|| username.to_string())
    }

    // =================================================================
    // 会话
    // =================================================================

    /// 获取会话列表
    pub async fn get_sessions(&self) -> Result<Vec<DbSessionInfo>> {
        let key = self.key_bytes.clone();
        let dir = self.db_dir.clone();

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, i32, String, i64, String)>> {
            let conn = Self::open_db(&key, &dir, "session/session.db")?;
            let mut stmt = conn.prepare(
                "SELECT username, unread_count, summary, last_timestamp, last_msg_sender \
                 FROM SessionTable ORDER BY sort_timestamp DESC"
            )?;
            let result = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i32>>(1)?.unwrap_or(0),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            })?.filter_map(|r| r.ok()).collect();
            Ok(result)
        }).await??;

        // 异步填充显示名
        let mut sessions = Vec::with_capacity(rows.len());
        for (username, unread_count, summary, last_timestamp, last_msg_sender) in rows {
            let display_name = self.resolve_name(&username).await;
            sessions.push(DbSessionInfo {
                username, display_name, unread_count, summary, last_timestamp, last_msg_sender,
            });
        }
        Ok(sessions)
    }

    // =================================================================
    // 增量消息
    // =================================================================

    /// 获取新消息 (复用持久连接)
    pub async fn get_new_messages(&self) -> Result<Vec<DbMessage>> {
        let current_watermarks = self.watermarks.lock().await.clone();

        // 获取持久连接并在 spawn_blocking 中执行同步查询
        let conn_guard = self.ensure_msg_conn()?;
        let conn_ptr = conn_guard.as_ref().unwrap() as *const Connection as usize;
        // SAFETY: Connection 在 std::sync::Mutex 中受保护, spawn_blocking 中独占使用
        // 我们持有 conn_guard 直到 spawn_blocking 完成
        let (raw_msgs, new_watermarks) = {
            let result = tokio::task::spawn_blocking(move || -> Result<(Vec<RawMsg>, HashMap<String, i64>)> {
                let conn = unsafe { &*(conn_ptr as *const Connection) };

            // 查找消息表

            // 查找消息表: ChatMsg_xxx 或 MSG_xxx 或 Chat_xxx
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND \
                 (name LIKE 'ChatMsg_%' OR name LIKE 'MSG_%' OR name LIKE 'Chat_%')"
            )?;
            let tables: Vec<String> = stmt.query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok()).collect();

            if tables.is_empty() {
                return Ok((vec![], current_watermarks));
            }

            let mut all_msgs = Vec::new();
            let mut wm = current_watermarks;

            for table in &tables {
                // 查询实际列名
                let pragma_sql = format!("PRAGMA table_info({})", table);
                let mut pragma_stmt = conn.prepare(&pragma_sql)?;
                let columns: Vec<String> = pragma_stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok()).collect();
                // 列名仅在首次发现或出错时打印

                // 实际列名 (Linux WeChat WCDB):
                // local_id, server_id, local_type, sort_seq, real_sender_id,
                // create_time, message_content, compress_content, WCDB_CT_message_content
                let id_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("local_id") || c.eq_ignore_ascii_case("localId")
                        || c.eq_ignore_ascii_case("rowid")
                }).cloned().unwrap_or_else(|| "rowid".to_string());

                let time_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("create_time") || c.eq_ignore_ascii_case("createTime")
                }).cloned();

                let content_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("message_content")
                        || c.eq_ignore_ascii_case("content")
                        || c.eq_ignore_ascii_case("msgContent")
                        || c.eq_ignore_ascii_case("compress_content")
                }).cloned();

                let type_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("local_type")
                        || c.eq_ignore_ascii_case("type")
                        || c.eq_ignore_ascii_case("msgType")
                }).cloned();

                let talker_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("real_sender_id")
                        || c.eq_ignore_ascii_case("talker")
                        || c.eq_ignore_ascii_case("talkerId")
                }).cloned();

                let svr_col = columns.iter().find(|c| {
                    c.eq_ignore_ascii_case("server_id") || c.eq_ignore_ascii_case("svrid")
                        || c.eq_ignore_ascii_case("msgSvrId")
                }).cloned();

                if content_col.is_none() {
                    warn!("⚠️ {} 无可识别的内容列, 列: {:?}", table, columns);
                    continue;
                }

                let time_sel = time_col.as_deref().unwrap_or("0");
                let content_sel = content_col.as_deref().unwrap();
                let type_sel = type_col.as_deref().unwrap_or("0");
                let talker_sel = talker_col.as_deref().unwrap_or("''");
                let svr_sel = svr_col.as_deref().unwrap_or("0");
                
                let last_id = wm.get(table).copied().unwrap_or(0);

                let sql = format!(
                    "SELECT {id}, {svr}, {time}, {content}, {typ}, {talker} \
                     FROM [{tbl}] WHERE {id} > ?1 ORDER BY {id} ASC",
                    id = id_col, svr = svr_sel, time = time_sel,
                    content = content_sel, typ = type_sel, talker = talker_sel,
                    tbl = table,
                );

                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(e) => { warn!("⚠️ 查询 {} 失败: {}", table, e); continue; }
                };
                let msgs: Vec<(i64, i64, i64, String, i32, String)> = match stmt
                    .query_map([last_id], |row| {
                        let local_id: i64 = row.get(0)?;
                        let svr_id: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
                        let ts: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                        
                        // message_content 可能是 TEXT 或 BLOB (WCDB压缩)
                        let content = match row.get::<_, Option<String>>(3) {
                            Ok(s) => s.unwrap_or_default(),
                            Err(_) => {
                                // BLOB fallback: 尝试读取 bytes 转 lossy UTF-8
                                match row.get::<_, Option<Vec<u8>>>(3) {
                                    Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).to_string(),
                                    _ => String::new(),
                                }
                            }
                        };
                        
                        let msg_type: i32 = row.get::<_, Option<i32>>(4)?.unwrap_or(0);
                        
                        // real_sender_id 也可能是 BLOB
                        let sender = match row.get::<_, Option<String>>(5) {
                            Ok(s) => s.unwrap_or_default(),
                            Err(_) => match row.get::<_, Option<Vec<u8>>>(5) {
                                Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).to_string(),
                                _ => String::new(),
                            }
                        };
                        
                        Ok((local_id, svr_id, ts, content, msg_type, sender))
                    }) {
                    Ok(rows) => rows.filter_map(|r| match r {
                        Ok(v) => Some(v),
                        Err(e) => { warn!("⚠️ 行解析失败: {}", e); None }
                    }).collect(),
                    Err(e) => { warn!("⚠️ query_map {} 失败: {}", table, e); continue; }
                };
                // 仅在有新消息时打印
                if !msgs.is_empty() {
                    debug!("📬 {} 查询到 {} 条新消息 (id>{}, 最新={})",
                        table, msgs.len(), last_id,
                        msgs.last().map(|m| m.0).unwrap_or(0));
                }

                if !msgs.is_empty() {
                    // 解析会话标识
                    let chat = resolve_chat_from_table(table, &conn);
                    let mut max_id = last_id;
                    for (local_id, server_id, create_time, content, msg_type, talker) in msgs {
                        all_msgs.push(RawMsg {
                            local_id, server_id, create_time, content, msg_type,
                            talker, chat: chat.clone(),
                        });
                        if local_id > max_id { max_id = local_id; }
                    }
                    wm.insert(table.clone(), max_id);
                }
            }

                Ok((all_msgs, wm))
            }).await??;
            result
        };
        drop(conn_guard); // 释放连接锁

        // 更新高水位线
        if !raw_msgs.is_empty() {
            *self.watermarks.lock().await = new_watermarks;
        }

        // 异步填充显示名
        let mut result = Vec::with_capacity(raw_msgs.len());
        for m in raw_msgs {
            // 私聊中 real_sender_id 为空, 用 chat (对方 wxid) 作为 talker
            let talker = if m.talker.is_empty() && !m.chat.contains("@chatroom") {
                m.chat.clone()
            } else {
                m.talker
            };
            let talker_display = self.resolve_name(&talker).await;
            let chat_display = self.resolve_name(&m.chat).await;
            result.push(DbMessage {
                local_id: m.local_id,
                server_id: m.server_id,
                create_time: m.create_time,
                content: m.content,
                msg_type: m.msg_type,
                talker,
                talker_display_name: talker_display,
                chat: m.chat,
                chat_display_name: chat_display,
            });
        }

        for m in &result {
            let preview = if m.content.len() > 40 {
                format!("{}...", &m.content[..m.content.floor_char_boundary(40)])
            } else {
                m.content.clone()
            };
            // 灰色 wxid: \x1b[90m ... \x1b[0m
            let gray_id = format!("\x1b[90m({})\x1b[0m", m.talker);
            if m.chat.contains("@chatroom") {
                // 群聊: 📨 [群名] 发送人(wxid): 内容
                info!("📨 [{}] {}{}: {}",
                    m.chat_display_name, m.talker_display_name, gray_id, preview);
            } else {
                // 私聊: 📨 发送人(wxid): 内容
                info!("📨 {}{}: {}",
                    m.talker_display_name, gray_id, preview);
            }
        }
        Ok(result)
    }

    /// 标记所有已有消息为已读 (复用持久连接)
    pub async fn mark_all_read(&self) -> Result<()> {
        let conn_guard = self.ensure_msg_conn()?;
        let conn_ptr = conn_guard.as_ref().unwrap() as *const Connection as usize;

        let wm = {
            let result = tokio::task::spawn_blocking(move || -> Result<HashMap<String, i64>> {
                let conn = unsafe { &*(conn_ptr as *const Connection) };
                let mut stmt = conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND \
                     (name LIKE 'ChatMsg_%' OR name LIKE 'MSG_%' OR name LIKE 'Chat_%' OR name LIKE 'Msg_%')"
                )?;
                let tables: Vec<String> = stmt.query_map([], |row| row.get(0))?
                    .filter_map(|r| r.ok()).collect();

                let mut watermarks = HashMap::new();
                for table in &tables {
                    let pragma = format!("PRAGMA table_info({})", table);
                    let mut ps = conn.prepare(&pragma)?;
                    let cols: Vec<String> = ps.query_map([], |r| r.get::<_, String>(1))?
                        .filter_map(|r| r.ok()).collect();
                    let id_col = cols.iter().find(|c| {
                        c.eq_ignore_ascii_case("local_id") || c.eq_ignore_ascii_case("localId")
                    }).cloned().unwrap_or_else(|| "rowid".to_string());

                    let sql = format!("SELECT MAX({}) FROM [{}]", id_col, table);
                    if let Ok(max_id) = conn.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0)) {
                        if let Some(id) = max_id {
                            watermarks.insert(table.clone(), id);
                        }
                    }
                }
                info!("✅ 已标记 {} 个消息表为已读", tables.len());
                Ok(watermarks)
            }).await??;
            result
        };
        drop(conn_guard);

        *self.watermarks.lock().await = wm;
        Ok(())
    }

    // =================================================================
    // WAL fanotify 监听 (PID 过滤)
    // =================================================================

    /// 启动 WAL 文件监听 (fanotify + PID 过滤, 在独立线程运行)
    pub fn spawn_wal_watcher(self: &Arc<Self>) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel::<()>(32);
        let db_dir = self.db_dir.clone();

        std::thread::spawn(move || {
            if let Err(e) = wal_watch_loop(&db_dir, tx) {
                error!("❌ WAL 监听退出: {}", e);
            }
        });

        info!("👁️ WAL 文件监听已启动 (fanotify PID 过滤)");
        rx
    }
}

// =====================================================================
// 同步辅助函数
// =====================================================================

/// 从消息表名解析会话 username
/// ChatMsg_<rowid> -> Name2Id.user_name WHERE rowid = <id>
/// Msg_<hash> -> MD5(Name2Id.user_name) == hash
fn resolve_chat_from_table(table_name: &str, conn: &Connection) -> String {
    // 尝试 ChatMsg_<数字> 格式 -> 按 rowid 查找
    if let Some(suffix) = table_name.strip_prefix("ChatMsg_") {
        if let Ok(id) = suffix.parse::<i64>() {
            let sql = "SELECT user_name FROM Name2Id WHERE rowid = ?1";
            if let Ok(name) = conn.query_row(sql, [id], |row| row.get::<_, String>(0)) {
                debug!("✅ ChatMsg rowid={} -> {}", id, name);
                return name;
            }
        }
    }

    // 尝试 Msg_<hash> / MSG_<hash> / Chat_<hash> 格式
    // WCDB 用 MD5(user_name) 作为消息表后缀
    if let Some(hash) = table_name.strip_prefix("Msg_")
        .or_else(|| table_name.strip_prefix("MSG_"))
        .or_else(|| table_name.strip_prefix("Chat_"))
    {
        // 遍历 Name2Id 所有 user_name，计算 MD5 匹配
        if let Ok(mut stmt) = conn.prepare("SELECT user_name FROM Name2Id") {
            if let Ok(names) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for name in names.flatten() {
                    let name_hash = format!("{:x}", md5::compute(name.as_bytes()));
                    if name_hash == hash {
                        debug!("✅ Msg hash={} -> user_name={}", hash, name);
                        return name;
                    }
                }
            }
        }
        debug!("⚠️ hash={} 未在 Name2Id 中找到匹配", hash);
    }

    debug!("⚠️ 无法解析会话名: {}", table_name);
    table_name.to_string()
}

// =====================================================================
// WAL 监听 (fanotify PID 过滤, 在 std::thread 中运行)
// =====================================================================

fn wal_watch_loop(db_dir: &Path, tx: mpsc::Sender<()>) -> Result<()> {
    use fanotify::high_level::*;

    let self_pid = std::process::id() as i32;
    info!("🔍 fanotify PID 过滤: self_pid={}", self_pid);

    let msg_dir = db_dir.join("message");

    // 等待 message 目录创建 (轮询, 仅启动时执行一次)
    if !msg_dir.exists() {
        info!("⏳ 等待 message 目录创建: {}", msg_dir.display());
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if msg_dir.exists() {
                info!("📁 message 目录已创建");
                break;
            }
        }
    }

    // 等待 WAL 文件创建 (轮询)
    let wal_path = msg_dir.join("message_0.db-wal");
    if !wal_path.exists() {
        info!("⏳ 等待 WAL 文件: {}", wal_path.display());
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if wal_path.exists() {
                info!("📄 WAL 文件已创建");
                break;
            }
        }
    }

    // 初始化 fanotify (通知模式)
    let fan = Fanotify::new_with_blocking(FanotifyMode::NOTIF);

    // 监听 message 目录的 MODIFY 事件 (覆盖 .wal 和 .shm)
    fan.add_path(FanEvent::Modify, &msg_dir)
        .with_context(|| format!("fanotify add_path 失败: {}", msg_dir.display()))?;

    info!("👁️ 开始监听 WAL: {} (fanotify, 无冷却期)", wal_path.display());

    loop {
        let events = fan.read_event();

        let mut has_external_modify = false;
        for event in events {
            // 核心 PID 过滤: 丢弃自身进程触发的事件
            if event.pid == self_pid {
                trace!("🔇 忽略自身事件 (pid={}): {}", event.pid, event.path);
                continue;
            }

            // 只关注 message_0.db 相关文件的修改
            if event.path.contains("message_0.db") {
                debug!("📝 外部 WAL MODIFY (pid={}): {}", event.pid, event.path);
                has_external_modify = true;
            }
        }

        if has_external_modify {
            // 直接通知, 无需冷却期!
            let _ = tx.try_send(());
        }
    }
}

// =====================================================================
// 工具函数
// =====================================================================

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(hex.len() % 2 == 0, "hex 长度必须为偶数");
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("无效 hex 字符: {}", &hex[i..i + 2]))
        })
        .collect()
}
