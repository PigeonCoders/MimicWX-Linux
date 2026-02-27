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

/// 结构化消息内容 (按 msg_type 解析)
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MsgContent {
    /// 纯文本 (msg_type=1)
    Text { text: String },
    /// 图片 (msg_type=3)
    Image { path: Option<String> },
    /// 语音 (msg_type=34)
    Voice { duration_ms: Option<u32> },
    /// 视频 (msg_type=43)
    Video { thumb_path: Option<String> },
    /// 表情包 (msg_type=47)
    Emoji { url: Option<String> },
    /// 链接/文件/小程序 (msg_type=49)
    App { title: Option<String>, desc: Option<String>, url: Option<String>, app_type: Option<i32> },
    /// 系统消息 (msg_type=10000/10002)
    System { text: String },
    /// 未知类型
    Unknown { raw: String, msg_type: i64 },
}

impl MsgContent {
    /// 消息类型的简短描述 (用于日志)
    pub fn type_label(&self) -> &'static str {
        match self {
            Self::Text { .. } => "文本",
            Self::Image { .. } => "图片",
            Self::Voice { .. } => "语音",
            Self::Video { .. } => "视频",
            Self::Emoji { .. } => "表情",
            Self::App { .. } => "链接",
            Self::System { .. } => "系统",
            Self::Unknown { .. } => "未知",
        }
    }

    /// 日志预览文本
    pub fn preview(&self, max_len: usize) -> String {
        let text = match self {
            Self::Text { text } => text.clone(),
            Self::Image { .. } => "[图片]".into(),
            Self::Voice { duration_ms, .. } => {
                match duration_ms {
                    Some(ms) if *ms >= 1000 => format!("[语音 {}s]", ms / 1000),
                    Some(ms) if *ms > 0 => format!("[语音 {ms}ms]"),
                    _ => "[语音]".into(),
                }
            }
            Self::Video { .. } => "[视频]".into(),
            Self::Emoji { url, .. } => format!("[表情] {}", url.as_deref().unwrap_or("")),
            Self::App { title, desc, app_type, .. } => {
                let t = title.as_deref().unwrap_or("");
                let d = desc.as_deref().unwrap_or("");
                // 子类型 + 标题后缀推断
                let label = match app_type.unwrap_or(0) {
                    3 => "音乐",
                    6 => "文件",
                    19 => "转发",
                    33 | 36 => "小程序",
                    42 => "名片",
                    2000 => "转账",
                    2001 => "红包",
                    _ => {
                        // 子类型提取失败时, 用标题后缀推断文件
                        let tl = t.to_lowercase();
                        if tl.ends_with(".pdf") || tl.ends_with(".doc") || tl.ends_with(".docx")
                            || tl.ends_with(".xls") || tl.ends_with(".xlsx") || tl.ends_with(".ppt")
                            || tl.ends_with(".pptx") || tl.ends_with(".zip") || tl.ends_with(".rar")
                            || tl.ends_with(".7z") || tl.ends_with(".txt") || tl.ends_with(".csv")
                            || tl.ends_with(".apk") || tl.ends_with(".exe") || tl.ends_with(".dmg")
                        {
                            "文件"
                        } else {
                            "链接"
                        }
                    }
                };
                if !t.is_empty() { format!("[{label}] {t}") }
                else if !d.is_empty() { format!("[{label}] {d}") }
                else { format!("[{label}]") }
            }
            Self::System { text } => format!("[系统] {text}"),
            Self::Unknown { msg_type, .. } => format!("[type={msg_type}]"),
        };
        if text.len() > max_len {
            format!("{}...", &text[..text.floor_char_boundary(max_len)])
        } else {
            text
        }
    }
}

/// 数据库消息
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbMessage {
    pub local_id: i64,
    pub server_id: i64,
    pub create_time: i64,
    /// 原始 content 字符串 (向后兼容)
    pub content: String,
    /// 结构化解析结果
    pub parsed: MsgContent,
    pub msg_type: i64,
    /// 发言人 wxid (群聊中有意义)
    pub talker: String,
    /// 发言人显示名 (通过联系人缓存解析)
    pub talker_display_name: String,
    /// 所属会话
    pub chat: String,
    /// 所属会话显示名
    pub chat_display_name: String,
    /// 是否为自己发送的消息
    pub is_self: bool,
}

/// 原始消息 (同步查询返回, 后续异步填充显示名)
struct RawMsg {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    content: String,
    msg_type: i64,
    talker: String,
    chat: String,
    status: i64,
}

// =====================================================================
// DbManager — 核心结构
// =====================================================================

/// 消息表结构元数据缓存 (避免每次查询重新执行 PRAGMA table_info)
#[derive(Debug, Clone)]
struct TableMeta {
    /// 表名
    table: String,
    /// 预编译的 SELECT SQL
    select_sql: String,
    /// ID 列名 (local_id / rowid)
    id_col: String,
}

pub struct DbManager {
    /// 32 字节原始密钥
    key_bytes: Vec<u8>,
    /// 数据库存储目录 (如 /home/wechat/.local/share/weixin/db_storage/)
    db_dir: PathBuf,
    /// 当前登录账号的 wxid (从 db_dir 路径提取, 用于判断自发消息)
    self_wxid: String,
    /// 当前账号的显示名 (从联系人库查询, 默认 "我")
    self_display_name: tokio::sync::RwLock<String>,
    /// 联系人缓存: username → ContactInfo
    contacts: Mutex<HashMap<String, ContactInfo>>,
    /// 高水位线: "db_name::表名" → 最大 local_id (多数据库区分)
    watermarks: Mutex<HashMap<String, i64>>,
    /// 持久化 message_N.db 连接池 (避免每次查询重做 PBKDF2 ~500ms)
    /// key = 相对路径 (如 "message/message_0.db")
    msg_conns: std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<Connection>>>>,
    /// 持久化 contact.db 连接 (避免每次重做 PBKDF2)
    contact_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// 持久化 session.db 连接
    session_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// 消息表结构元数据缓存: "db_name::table_name" → TableMeta
    /// 表的列结构在运行期间不变, 但微信可能动态创建新表
    table_meta_cache: std::sync::Mutex<HashMap<String, TableMeta>>,
}

impl DbManager {
    /// 创建 DbManager
    pub fn new(key_hex: String, db_dir: PathBuf) -> Result<Self> {
        let key_bytes = hex_to_bytes(&key_hex)
            .context("密钥 hex 格式错误")?;
        anyhow::ensure!(key_bytes.len() == 32, "密钥长度必须为 32 字节, 实际: {}", key_bytes.len());

        info!("📦 DbManager 初始化: db_dir={}", db_dir.display());

        // 从 db_dir 路径提取自己的 wxid
        // 路径格式: .../wxid_xxx_c024/db_storage
        let self_wxid = db_dir.components()
            .filter_map(|c| c.as_os_str().to_str())
            .find(|s| s.starts_with("wxid_"))
            .map(|s| {
                // 取 wxid_xxx 部分 (去掉 _c024 后缀)
                if let Some(pos) = s.rfind("_c") {
                    let suffix = &s[pos+2..];
                    if suffix.chars().all(|c| c.is_ascii_digit()) {
                        return s[..pos].to_string();
                    }
                }
                s.to_string()
            })
            .unwrap_or_default();
        if !self_wxid.is_empty() {
            info!("👤 当前账号: {}", self_wxid);
        }

        // 自动发现并连接所有 message_N.db
        let mut conns = HashMap::new();
        let msg_dir = db_dir.join("message");
        if msg_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&msg_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if is_message_db(&name) {
                        let rel_path = format!("message/{}", name);
                        match Self::open_db(&key_bytes, &db_dir, &rel_path) {
                            Ok(conn) => {
                                info!("🔗 {} 持久连接已建立", name);
                                conns.insert(rel_path, Arc::new(std::sync::Mutex::new(conn)));
                            }
                            Err(e) => {
                                info!("⚠️ {} 暂不可用 (将在查询时重试): {}", name, e);
                            }
                        }
                    }
                }
            }
        }
        if conns.is_empty() {
            warn!("⚠️ 未发现可用的 message 数据库 (将在首次查询时重试)");
        } else {
            info!("📂 已连接 {} 个消息数据库", conns.len());
        }

        Ok(Self {
            key_bytes,
            db_dir,
            self_wxid,
            self_display_name: tokio::sync::RwLock::new("我".to_string()),
            contacts: Mutex::new(HashMap::new()),
            watermarks: Mutex::new(HashMap::new()),
            msg_conns: std::sync::Mutex::new(conns),
            contact_conn: Arc::new(std::sync::Mutex::new(None)),
            session_conn: Arc::new(std::sync::Mutex::new(None)),
            table_meta_cache: std::sync::Mutex::new(HashMap::new()),
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

    /// 确保至少有一个 message 数据库连接可用 (如为空则重新扫描)
    fn ensure_msg_conns(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Arc<std::sync::Mutex<Connection>>>>> {
        let mut guard = self.msg_conns.lock().map_err(|e| anyhow::anyhow!("msg_conns lock poisoned: {}", e))?;
        if guard.is_empty() {
            info!("🔗 重新扫描 message 数据库...");
            let msg_dir = self.db_dir.join("message");
            if msg_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&msg_dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if is_message_db(&name) {
                            let rel_path = format!("message/{}", name);
                            if !guard.contains_key(&rel_path) {
                                if let Ok(conn) = Self::open_db(&self.key_bytes, &self.db_dir, &rel_path) {
                                    info!("🔗 {} 持久连接已建立", name);
                                    guard.insert(rel_path, Arc::new(std::sync::Mutex::new(conn)));
                                }
                            }
                        }
                    }
                }
            }
            anyhow::ensure!(!guard.is_empty(), "无可用的 message 数据库");
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
        let conn_mutex = Arc::clone(&self.contact_conn);

        let contacts = tokio::task::spawn_blocking(move || -> Result<Vec<ContactInfo>> {
            // 复用或创建持久连接
            let mut guard = conn_mutex.lock().map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(&key, &dir, "contact/contact.db")?);
                info!("🔗 contact.db 持久连接已建立");
            }
            let conn = guard.as_ref().unwrap();
            let mut stmt = conn.prepare(
                "SELECT username, nick_name, remark, alias FROM contact"
            )?;
            // WCDB 压缩可能导致 TEXT 列实际存储为 BLOB (Zstd),
            // 必须用 BLOB 回退读取, 否则部分行 (包括 chatroom) 会被丢弃
            let result: Vec<ContactInfo> = stmt.query_map([], |row| {
                let username = wcdb_get_text(row, 0);
                if username.is_empty() {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let nick_name = wcdb_get_text(row, 1);
                let remark = wcdb_get_text(row, 2);
                let alias = wcdb_get_text(row, 3);
                let display_name = if !remark.is_empty() {
                    remark.clone()
                } else if !nick_name.is_empty() {
                    nick_name.clone()
                } else {
                    username.clone()
                };
                Ok(ContactInfo { username, nick_name, remark, alias, display_name })
            })?.filter_map(|r| match r {
                Ok(c) => Some(c),
                Err(e) => { warn!("⚠️ 联系人行读取失败: {}", e); None }
            }).collect();
            Ok(result)
        }).await??;

        let count = contacts.len();
        let mut cache = self.contacts.lock().await;
        cache.clear();
        for c in contacts {
            cache.insert(c.username.clone(), c);
        }
        info!("👥 联系人缓存: {} 条", count);

        // 从 chat_room 表补充群名 (contact 表可能不含 @chatroom 条目)
        // chat_room 表结构: id INTEGER, username TEXT, owner TEXT, ext_buffer BLOB
        // 群名不在 chat_room 中, 需要 JOIN contact 表的 nick_name
        {
            let conn_mutex2 = Arc::clone(&self.contact_conn);
            let chatrooms = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
                let guard = conn_mutex2.lock().map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
                if let Some(conn) = guard.as_ref() {
                    let mut result = Vec::new();
                    // chat_room 表列出所有群聊 ID, JOIN contact 表获取群名
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT cr.username, c.nick_name FROM chat_room cr \
                         LEFT JOIN contact c ON cr.username = c.username \
                         WHERE cr.username IS NOT NULL"
                    ) {
                        let rows: Vec<(String, String)> = stmt.query_map([], |row| {
                            let id = wcdb_get_text(row, 0);
                            let name = wcdb_get_text(row, 1);
                            Ok((id, name))
                        }).ok()
                        .map(|iter| iter.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default();

                        for (id, name) in rows {
                            if !id.is_empty() && !name.is_empty() {
                                info!("👥 chat_room 补充: {} → {}", id, name);
                                result.push((id, name));
                            }
                        }
                    }
                    Ok(result)
                } else {
                    Ok(vec![])
                }
            }).await.unwrap_or_else(|_| Ok(vec![])).unwrap_or_default();

            let mut added = 0usize;
            for (chatroom_id, nick_name) in chatrooms {
                if !cache.contains_key(&chatroom_id) {
                    cache.insert(chatroom_id.clone(), ContactInfo {
                        username: chatroom_id,
                        nick_name: nick_name.clone(),
                        remark: String::new(),
                        alias: String::new(),
                        display_name: nick_name,
                    });
                    added += 1;
                }
            }
            if added > 0 {
                info!("👥 群聊名称补充: {} 条", added);
            }
        }

        // 尝试解析当前账号的显示名
        if !self.self_wxid.is_empty() {
            if let Some(c) = cache.get(&self.self_wxid) {
                let name = c.display_name.clone();
                info!("👤 当前账号昵称: {} ({})", name, self.self_wxid);
                *self.self_display_name.write().await = name;
            }
        }

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
        let conn_mutex = Arc::clone(&self.session_conn);

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, i32, String, i64, String)>> {
            // 复用或创建持久连接
            let mut guard = conn_mutex.lock().map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(Self::open_db(&key, &dir, "session/session.db")?);
                info!("🔗 session.db 持久连接已建立");
            }
            let conn = guard.as_ref().unwrap();
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

    /// 获取新消息 (遍历所有 message_N.db 持久连接)
    pub async fn get_new_messages(&self) -> Result<Vec<DbMessage>> {
        let current_watermarks = self.watermarks.lock().await.clone();

        // 克隆 Arc 引用传入 spawn_blocking (安全, 无 unsafe)
        let conn_arcs: Vec<(String, Arc<std::sync::Mutex<Connection>>)> = {
            let conns_guard = self.ensure_msg_conns()?;
            conns_guard.iter()
                .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
                .collect()
        };

        // 获取表结构缓存: key = "db_name::table_name" → TableMeta
        // 每次都查表列表 (1 条 SQL, 很快), 但只对新出现的表执行 PRAGMA
        let cached_meta: HashMap<String, TableMeta> = {
            self.table_meta_cache.lock()
                .map(|g| g.clone())
                .unwrap_or_default()
        };

        let (raw_msgs, new_watermarks, updated_meta) = tokio::task::spawn_blocking(move || -> Result<(Vec<RawMsg>, HashMap<String, i64>, HashMap<String, TableMeta>)> {
            let mut all_msgs = Vec::new();
            let mut wm = current_watermarks;
            let mut name2id_cache: HashMap<String, String> = HashMap::new();
            let mut meta_cache = cached_meta;

            for (db_name, conn_arc) in &conn_arcs {
                let conn = conn_arc.lock().map_err(|e| anyhow::anyhow!("conn lock: {}", e))?;
                let db_prefix = db_name.trim_start_matches("message/").trim_end_matches(".db");

                // 每次都查表列表 (微信可能动态创建新表)
                let tables = discover_msg_tables(&conn);
                if tables.is_empty() { continue; }

                // 对每个表: 查缓存 → 有则复用, 无则 PRAGMA 构建
                let mut table_metas = Vec::new();
                for table in &tables {
                    let cache_key = format!("{}::{}", db_name, table);
                    if let Some(cached) = meta_cache.get(&cache_key) {
                        table_metas.push(cached.clone());
                    } else {
                        // 新表: PRAGMA 获取列结构
                        if let Some(meta) = build_single_table_meta(&conn, table) {
                            info!("📋 {} 新增表结构缓存: {}", db_name, table);
                            meta_cache.insert(cache_key, meta.clone());
                            table_metas.push(meta);
                        }
                    }
                }

                for meta in &table_metas {
                    let wm_key = format!("{}::{}", db_prefix, meta.table);
                    let last_id = wm.get(&wm_key).copied().unwrap_or(0);

                    let mut stmt = match conn.prepare(&meta.select_sql) {
                        Ok(s) => s,
                        Err(e) => { warn!("⚠️ 查询 {} ({}) 失败: {}", meta.table, db_name, e); continue; }
                    };
                    let msgs: Vec<(i64, i64, i64, String, i64, String, i64)> = match stmt
                        .query_map([last_id], |row| {
                            let local_id: i64 = row.get(0)?;
                            let svr_id: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
                            let ts: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                            
                            // message_content: 先尝试读为文本，失败则读 BLOB + Zstd 解压
                            let content = match row.get::<_, Option<String>>(3) {
                                Ok(s) => s.unwrap_or_default(),
                                Err(_) => {
                                    // BLOB: 可能是 WCDB Zstd 压缩
                                    match row.get::<_, Option<Vec<u8>>>(3) {
                                        Ok(Some(bytes)) => decompress_wcdb_content(&bytes),
                                        _ => String::new(),
                                    }
                                }
                            };
                            
                            let msg_type: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
                            
                            let sender = match row.get::<_, Option<String>>(5) {
                                Ok(s) => s.unwrap_or_default(),
                                Err(_) => match row.get::<_, Option<Vec<u8>>>(5) {
                                    Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).to_string(),
                                    _ => String::new(),
                                }
                            };

                            let status: i64 = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
                            
                            Ok((local_id, svr_id, ts, content, msg_type, sender, status))
                        }) {
                        Ok(rows) => rows.filter_map(|r| match r {
                            Ok(v) => Some(v),
                            Err(e) => { warn!("⚠️ 行解析失败: {}", e); None }
                        }).collect(),
                        Err(e) => { warn!("⚠️ query_map {} ({}) 失败: {}", meta.table, db_name, e); continue; }
                    };

                    if !msgs.is_empty() {
                        let chat = resolve_chat_from_table(&meta.table, &conn, &mut name2id_cache);
                        let mut max_id = last_id;
                        for (local_id, server_id, create_time, content, msg_type, talker, status) in msgs {
                            all_msgs.push(RawMsg {
                                local_id, server_id, create_time, content, msg_type,
                                talker, chat: chat.clone(), status,
                            });
                            if local_id > max_id { max_id = local_id; }
                        }
                        wm.insert(wm_key.clone(), max_id);
                    }
                }
            }

            Ok((all_msgs, wm, meta_cache))
        }).await??;

        // 回写表结构缓存 (首次构建后不再变化)
        if let Ok(mut cache) = self.table_meta_cache.lock() {
            for (k, v) in updated_meta {
                cache.entry(k).or_insert(v);
            }
        }

        // 更新高水位线
        if !raw_msgs.is_empty() {
            *self.watermarks.lock().await = new_watermarks;
        }

        // 异步填充显示名 (批量: 一次锁定联系人缓存, 避免 N×2 次锁竞争)
        let contacts_cache = self.contacts.lock().await;
        let self_display = self.self_display_name.read().await.clone();
        let resolve = |username: &str| -> String {
            contacts_cache
                .get(username)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| username.to_string())
        };

        let mut result = Vec::with_capacity(raw_msgs.len());
        for m in raw_msgs {
            let mut talker = m.talker;
            let mut content = m.content;

            // 群聊中 real_sender_id 可能为空, 此时发送人 wxid 嵌入在消息内容中
            // 格式: "wxid_xxx:\n实际消息" 或 "wxid_xxx:\r\n实际消息"
            if talker.is_empty() && m.chat.contains("@chatroom") {
                if let Some(pos) = content.find(":\n") {
                    let prefix = &content[..pos];
                    // 验证前缀看起来像 wxid (不含空格和特殊字符)
                    if !prefix.is_empty() && !prefix.contains(' ') && prefix.len() < 50 {
                        talker = prefix.to_string();
                        content = content[pos + 2..].to_string(); // 跳过 ":\n"
                    }
                }
            }

            // 判断是否为自己发送的消息 (基于 status 位掩码)
            // status bit 1 (0x02): 1=收到的消息, 0=自己发的消息
            let is_self = (m.status & 0x02) == 0;

            // talker 为空时填充: 自发用 self_wxid, 私聊收到用 chat(对方)
            if talker.is_empty() {
                if is_self {
                    talker = self.self_wxid.clone();
                } else if !m.chat.contains("@chatroom") {
                    talker = m.chat.clone();
                }
            }

            let talker_display = if is_self {
                self_display.clone()
            } else {
                resolve(&talker)
            };
            let chat_display = resolve(&m.chat);
            // 非文本消息: 输出原始 content 前 200 字符用于调试 XML 解析
            let base_type = (m.msg_type & 0xFFFF) as i32;
            if base_type != 1 {
                let raw_preview = if content.len() > 200 {
                    format!("{}...", &content[..content.floor_char_boundary(200)])
                } else {
                    content.clone()
                };
                debug!("🔍 msg_type={} (base={}) raw: {}", m.msg_type, base_type, raw_preview);
            }
            let parsed = parse_msg_content(m.msg_type, &content);
            result.push(DbMessage {
                local_id: m.local_id,
                server_id: m.server_id,
                create_time: m.create_time,
                content,
                parsed,
                msg_type: m.msg_type,
                talker,
                talker_display_name: talker_display,
                chat: m.chat,
                chat_display_name: chat_display,
                is_self,
            });
        }
        drop(contacts_cache); // 显式释放锁

        for m in &result {
            let preview = m.parsed.preview(40);
            let icon = if m.is_self { "📤 →" } else { "📨" };
            if m.chat.contains("@chatroom") {
                info!("{icon} [{}] {}({}): {}",
                    m.chat_display_name, m.talker_display_name, m.talker, preview);
            } else {
                info!("{icon} {}({}): {}",
                    m.chat_display_name, m.talker, preview);
            }
        }
        Ok(result)
    }

    /// 标记所有已有消息为已读 (复用持久连接)
    pub async fn mark_all_read(&self) -> Result<()> {
        // 克隆 Arc 引用传入 spawn_blocking
        let conn_arcs: Vec<(String, Arc<std::sync::Mutex<Connection>>)> = {
            let conns_guard = self.ensure_msg_conns()?;
            conns_guard.iter()
                .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
                .collect()
        };

        let wm = tokio::task::spawn_blocking(move || -> Result<HashMap<String, i64>> {
            let mut watermarks = HashMap::new();
            let mut total_tables = 0;

            for (db_name, conn_arc) in &conn_arcs {
                let conn = conn_arc.lock().map_err(|e| anyhow::anyhow!("conn lock: {}", e))?;
                let db_prefix = db_name.trim_start_matches("message/").trim_end_matches(".db");

                let mut stmt = match conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND \
                     (name LIKE 'ChatMsg_%' OR name LIKE 'MSG_%' OR name LIKE 'Chat_%')"
                ) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let tables: Vec<String> = stmt.query_map([], |row| row.get(0))?
                    .filter_map(|r| r.ok()).collect();

                for table in &tables {
                    let wm_key = format!("{}::{}", db_prefix, table);
                    let pragma = format!("PRAGMA table_info({})", table);
                    let mut ps = match conn.prepare(&pragma) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let cols: Vec<String> = ps.query_map([], |r| r.get::<_, String>(1))?
                        .filter_map(|r| r.ok()).collect();
                    let id_col = cols.iter().find(|c| {
                        c.eq_ignore_ascii_case("local_id") || c.eq_ignore_ascii_case("localId")
                    }).cloned().unwrap_or_else(|| "rowid".to_string());

                    let sql = format!("SELECT MAX({}) FROM [{}]", id_col, table);
                    if let Ok(max_id) = conn.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0)) {
                        if let Some(id) = max_id {
                            watermarks.insert(wm_key, id);
                        }
                    }
                }
                total_tables += tables.len();
            }
            info!("✅ 已标记 {} 个消息表为已读 (跨 {} 个数据库)", total_tables, conn_arcs.len());
            Ok(watermarks)
        }).await??;

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
/// Msg_<hash> -> MD5(Name2Id.user_name) == hash (使用缓存 O(1) 查找)
fn resolve_chat_from_table(table_name: &str, conn: &Connection, cache: &mut HashMap<String, String>) -> String {
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
    if let Some(hash) = table_name.strip_prefix("Msg_")
        .or_else(|| table_name.strip_prefix("MSG_"))
        .or_else(|| table_name.strip_prefix("Chat_"))
    {
        // 懒加载: 首次查找时构建 MD5 hash → username 缓存
        if cache.is_empty() {
            if let Ok(mut stmt) = conn.prepare("SELECT user_name FROM Name2Id") {
                if let Ok(names) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                    for name in names.flatten() {
                        let name_hash = format!("{:x}", md5::compute(name.as_bytes()));
                        cache.insert(name_hash, name);
                    }
                }
            }
            debug!("📦 Name2Id 缓存已构建: {} 条", cache.len());
        }

        // O(1) 查找
        if let Some(name) = cache.get(hash) {
            debug!("✅ Msg hash={} -> user_name={}", hash, name);
            return name.clone();
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

    // 初始化 fanotify (通知模式, 阻塞读取)
    let fan = Fanotify::new_blocking(FanotifyMode::NOTIF)
        .with_context(|| "fanotify 初始化失败")?;

    // 使用 FAN_MARK_MOUNT (挂载点级别标记) 而非 add_path (Inode 级标记)
    // 原因: add_path 对目录的 Inode 标记只监听目录自身的修改,
    //       不会报告目录内子文件(WAL/SHM)的 FAN_MODIFY 事件,
    //       除非额外附加 FAN_EVENT_ON_CHILD 标志.
    //       add_mountpoint 使用 FAN_MARK_MOUNT, 覆盖整个挂载点上的所有文件,
    //       包括子目录和嵌套文件, 无需 FAN_EVENT_ON_CHILD.
    fan.add_mountpoint(FanEvent::Modify.into(), &msg_dir)
        .with_context(|| format!("fanotify add_mountpoint 失败: {}", msg_dir.display()))?;

    info!("👁️ 开始监听 WAL: {} (fanotify FAN_MARK_MOUNT, 无冷却期)", wal_path.display());

    let msg_dir_prefix = msg_dir.to_string_lossy().to_string();

    loop {
        let events = fan.read_event();
        // 注: Event.fd 由 fanotify-rs 的 Drop trait 自动关闭, 无需手动 close

        let mut has_external_modify = false;
        for event in events {
            // 核心 PID 过滤: 丢弃自身进程触发的事件
            if event.pid == self_pid {
                continue;
            }

            // 路径过滤: 只关心 message/ 目录下的文件 (忽略挂载点其他文件)
            if !event.path.starts_with(&msg_dir_prefix) {
                continue;
            }

            // 外部进程修改了消息数据库文件 → 触发消息检查
            trace!("📝 外部 MODIFY (pid={}): {}", event.pid, event.path);
            has_external_modify = true;
        }

        if has_external_modify {
            // 直接通知, 无需冷却期!
            let _ = tx.try_send(());
        }
    }
}

// =====================================================================
// 消息内容解析
// =====================================================================

/// WCDB Zstd BLOB 解压: 检测 Zstd magic 0x28B52FFD, 解压后返回 UTF-8 字符串
fn decompress_wcdb_content(blob: &[u8]) -> String {
    // Zstd magic: 0xFD2FB528 (little-endian) = bytes [0x28, 0xB5, 0x2F, 0xFD]
    if blob.len() >= 4 && blob[0] == 0x28 && blob[1] == 0xB5 && blob[2] == 0x2F && blob[3] == 0xFD {
        match zstd::decode_all(blob) {
            Ok(data) => return String::from_utf8_lossy(&data).to_string(),
            Err(e) => warn!("⚠️ Zstd 解压失败: {}", e),
        }
    }
    // 非 Zstd: 直接 lossy UTF-8
    String::from_utf8_lossy(blob).to_string()
}

/// WCDB 兼容读取: 先尝试 TEXT, 失败则 BLOB + Zstd 解压
/// (WCDB 压缩可能导致 TEXT 列实际存储为 BLOB)
fn wcdb_get_text(row: &rusqlite::Row, idx: usize) -> String {
    match row.get::<_, Option<String>>(idx) {
        Ok(s) => s.unwrap_or_default(),
        Err(_) => match row.get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(bytes)) => decompress_wcdb_content(&bytes),
            _ => String::new(),
        },
    }
}

/// 查询 sqlite_master 获取消息表列表 (每次调用, 发现新表)
fn discover_msg_tables(conn: &Connection) -> Vec<String> {
    match conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND \
         (name LIKE 'ChatMsg_%' OR name LIKE 'MSG_%' OR name LIKE 'Chat_%')"
    ) {
        Ok(mut stmt) => {
            stmt.query_map([], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    }
}

/// 对单个消息表执行 PRAGMA table_info → 构建 TableMeta (仅新表调用一次)
fn build_single_table_meta(conn: &Connection, table: &str) -> Option<TableMeta> {
    let pragma_sql = format!("PRAGMA table_info({})", table);
    let mut pragma_stmt = conn.prepare(&pragma_sql).ok()?;
    let columns: Vec<String> = pragma_stmt
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?
        .filter_map(|r| r.ok()).collect();

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

    let content_sel = content_col.as_deref()?;
    let time_sel = time_col.as_deref().unwrap_or("0");
    let type_sel = type_col.as_deref().unwrap_or("0");
    let talker_sel = talker_col.as_deref().unwrap_or("''");
    let svr_sel = svr_col.as_deref().unwrap_or("0");

    let status_col = columns.iter().find(|c| {
        c.eq_ignore_ascii_case("status")
    }).cloned();
    let status_sel = status_col.as_deref().unwrap_or("0");

    let select_sql = format!(
        "SELECT {id}, {svr}, {time}, {content}, {typ}, {talker}, {status} \
         FROM [{tbl}] WHERE {id} > ?1 ORDER BY {id} ASC",
        id = id_col, svr = svr_sel, time = time_sel,
        content = content_sel, typ = type_sel, talker = talker_sel,
        status = status_sel, tbl = table,
    );

    Some(TableMeta {
        table: table.to_string(),
        select_sql,
        id_col,
    })
}

/// 根据 msg_type 解析原始 content 为结构化 MsgContent
/// content 已经过 Zstd 解压 (如果需要), 应为 XML 或纯文本
fn parse_msg_content(msg_type: i64, content: &str) -> MsgContent {
    // 微信 msg_type 高位是标志位 (如 0x600000021), 实际类型在低 16 位
    let base_type = (msg_type & 0xFFFF) as i32;
    match base_type {
        1 => MsgContent::Text { text: content.to_string() },
        3 => parse_image(content),
        34 => parse_voice(content),
        42 => parse_contact_card(content),
        43 => parse_video(content),
        47 => parse_emoji(content),
        49 => parse_app(content),
        10000 | 10002 => MsgContent::System { text: content.to_string() },
        _ => MsgContent::Unknown { raw: content.to_string(), msg_type },
    }
}

/// 图片消息: 从 XML 中提取 CDN URL
fn parse_image(content: &str) -> MsgContent {
    let path = extract_xml_attr(content, "img", "cdnmidimgurl")
        .or_else(|| extract_xml_attr(content, "img", "cdnbigimgurl"));
    MsgContent::Image { path }
}

/// 语音消息: 尝试多种属性名提取时长
fn parse_voice(content: &str) -> MsgContent {
    let duration_ms = extract_xml_attr(content, "voicemsg", "voicelength")
        .or_else(|| extract_xml_attr(content, "voicemsg", "voicelen"))
        .or_else(|| extract_xml_attr(content, "voicemsg", "length"))
        .and_then(|v| v.parse::<u32>().ok());
    MsgContent::Voice { duration_ms }
}

/// 名片消息 (msg_type=42): 提取昵称和 wxid
fn parse_contact_card(content: &str) -> MsgContent {
    let nickname = extract_xml_attr(content, "msg", "nickname")
        .or_else(|| extract_xml_attr(content, "msg", "smallheadimgurl"));
    let username = extract_xml_attr(content, "msg", "username");
    let title = nickname.or(username);
    MsgContent::App {
        title,
        desc: Some("名片".to_string()),
        url: None,
        app_type: Some(42),
    }
}

/// 视频消息: 提取 cdnthumburl
fn parse_video(content: &str) -> MsgContent {
    let thumb_path = extract_xml_attr(content, "videomsg", "cdnthumburl");
    MsgContent::Video { thumb_path }
}

/// 表情消息: 提取 cdnurl
fn parse_emoji(content: &str) -> MsgContent {
    let url = extract_xml_attr(content, "emoji", "cdnurl");
    MsgContent::Emoji { url }
}

/// 链接/文件/小程序消息 (msg_type=49): 解析 appmsg XML
/// app_type 子类型: 3=音乐, 4=链接, 5=链接, 6=文件, 19=转发, 33/36=小程序, 2000=转账, 2001=红包
fn parse_app(content: &str) -> MsgContent {
    let title = extract_xml_text(content, "title");
    let desc = extract_xml_text(content, "des");
    let url = extract_xml_text(content, "url");
    let app_type = extract_xml_text(content, "type")
        .and_then(|t| t.parse::<i32>().ok());
    MsgContent::App {
        title, desc, url, app_type,
    }
}

/// 从 XML 中提取指定元素的属性值 (如 <img cdnmidimgurl="..."/>)
fn extract_xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == attr.as_bytes() {
                            return String::from_utf8(a.value.to_vec()).ok();
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// 从 XML 中提取指定元素的文本内容 (如 <title>标题</title>)
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_tag = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = true;
                }
            }
            Ok(Event::Text(ref e)) if in_tag => {
                return e.unescape().ok().map(|s| s.to_string());
            }
            Ok(Event::CData(ref e)) if in_tag => {
                return String::from_utf8(e.to_vec()).ok();
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

// =====================================================================
// 工具函数
// =====================================================================

/// 判断文件名是否为 message_N.db 格式 (N 是数字)
/// 排除 message_fts.db, message_resource.db 等辅助数据库
fn is_message_db(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("message_") {
        if let Some(num_part) = rest.strip_suffix(".db") {
            return !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

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
