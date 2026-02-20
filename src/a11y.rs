//! AT-SPI2 无障碍树监听器 — 主消息检测通道
//!
//! 策略: 通过 atspi-rs 订阅事件 + 3 秒定时轮询后备。
//! 定向搜索 `[list] name='Chats'` 和 `[list] name='Messages'` 节点，
//! 首次搜索后缓存 NodeRef，后续轮询直接读取子项 (<100ms)。

use anyhow::Result;
use atspi::AccessibilityConnection;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::zvariant::OwnedObjectPath;

use crate::WxMessage;

// =====================================================================
// 常量
// =====================================================================

/// AT-SPI2 Accessible 接口名
const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";

/// D-Bus 单次调用超时 (防止阻塞)
const DBUS_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// 整体扫描超时
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 轮询间隔
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// DFS 搜索最大深度 (Chats 节点约在 depth 12)
const MAX_SEARCH_DEPTH: u32 = 18;

/// 等待微信登录的检测间隔
const LOGIN_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

// =====================================================================
// 类型定义
// =====================================================================

/// AT-SPI2 节点引用 (bus_name + object_path)
#[derive(Debug, Clone)]
struct NodeRef {
    bus: String,
    path: OwnedObjectPath,
}

/// 缓存已找到的 AT-SPI2 关键节点，避免重复 DFS 搜索
#[derive(Default, Clone)]
struct CachedNodes {
    /// `[list] name='Chats'` — 聊天联系人列表
    chats_list: Option<NodeRef>,
    /// `[list] name='Messages'` — 当前打开的聊天消息列表
    messages_list: Option<NodeRef>,
}

/// 扫描结果: 消息内容 + 更新后的缓存
struct ScanResult {
    messages: Vec<String>,
    cached: CachedNodes,
}

/// 微信状态
#[derive(Debug, PartialEq)]
enum WeChatStatus {
    /// 微信进程未找到
    NotRunning,
    /// 微信已启动但未登录 (登录界面)
    LoginScreen,
    /// 微信已登录 (有 Chats 列表)
    LoggedIn,
}

// =====================================================================
// 主入口: 事件循环
// =====================================================================

/// 启动 AT-SPI2 事件监听器
pub async fn run(tx: mpsc::Sender<WxMessage>) -> Result<()> {
    info!("📡 AT-SPI2 监听器启动中...");

    let a11y = AccessibilityConnection::new().await?;
    info!("✅ AT-SPI2 连接建立");

    // 订阅相关事件类型
    a11y.register_event::<atspi::events::object::ChildrenChangedEvent>().await?;
    a11y.register_event::<atspi::events::object::TextChangedEvent>().await?;
    a11y.register_event::<atspi::events::object::StateChangedEvent>().await?;
    a11y.register_event::<atspi::events::object::PropertyChangeEvent>().await?;
    a11y.register_event::<atspi::events::window::ActivateEvent>().await?;
    info!("✅ AT-SPI2 监听器就绪");

    // === 阶段 1: 等待微信登录 ===
    let mut cached_nodes = wait_for_wechat_login(a11y.connection()).await;

    // === 阶段 2: 初始扫描 ===
    let initial_result = scan_wechat_messages(a11y.connection(), &cached_nodes).await;
    let initial_messages = initial_result.messages;
    cached_nodes = initial_result.cached;
    info!("📋 初始消息数: {}", initial_messages.len());
    for msg in &initial_messages {
        info!("  初始: {msg}");
    }

    // 事件循环
    let mut last_messages = initial_messages;
    let event_stream = a11y.event_stream();
    tokio::pin!(event_stream);

    let mut last_scan_time = std::time::Instant::now();
    let mut poll_timer = tokio::time::interval(POLL_INTERVAL);
    poll_timer.tick().await; // 消耗第一个 tick

    loop {
        let should_scan = tokio::select! {
            event_result = event_stream.next() => {
                match event_result {
                    None => {
                        warn!("AT-SPI2 事件流结束");
                        break;
                    }
                    Some(Err(e)) => {
                        debug!("事件错误: {e}");
                        false
                    }
                    Some(Ok(event)) => classify_event(&event),
                }
            }
            _ = poll_timer.tick() => true,
        };

        if !should_scan {
            continue;
        }

        // 去重: 距上次扫描不足 POLL_INTERVAL 则跳过
        let now = std::time::Instant::now();
        if now.duration_since(last_scan_time) < POLL_INTERVAL {
            continue;
        }
        last_scan_time = now;

        // 执行扫描 (带整体超时)
        let scan_result = match tokio::time::timeout(
            SCAN_TIMEOUT,
            scan_wechat_messages(a11y.connection(), &cached_nodes),
        ).await {
            Ok(result) => result,
            Err(_) => {
                warn!("⏰ 扫描超时 ({SCAN_TIMEOUT:?}), 保留缓存");
                // 不清除缓存！超时通常是 DFS 搜索慢，缓存的 NodeRef 可能仍有效
                continue;
            }
        };

        cached_nodes = scan_result.cached;
        let current_messages = scan_result.messages;

        if current_messages.is_empty() {
            continue;
        }

        // 检测新增消息
        let new_msgs = diff_messages(&last_messages, &current_messages);
        if !new_msgs.is_empty() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            for msg_text in &new_msgs {
                let (sender, text) = parse_message(msg_text);
                info!("📨 新消息: {msg_text}");

                if tx.send(WxMessage { sender, text, timestamp, source: "atspi".into() })
                    .await.is_err()
                {
                    return Ok(());
                }
            }
        }

        last_messages = current_messages;
    }

    warn!("AT-SPI2 事件流结束");
    Ok(())
}

/// 判断 AT-SPI2 事件是否需要触发扫描
fn classify_event(event: &atspi::Event) -> bool {
    use atspi::Event;
    let kind = match event {
        Event::Object(obj) => match obj {
            atspi::events::ObjectEvents::ChildrenChanged(_) => "ChildrenChanged",
            atspi::events::ObjectEvents::TextChanged(_) => "TextChanged",
            atspi::events::ObjectEvents::StateChanged(_) => "StateChanged",
            atspi::events::ObjectEvents::PropertyChange(_) => "PropertyChange",
            _ => return false,
        },
        Event::Window(_) => "Window",
        _ => return false,
    };
    info!("🔔 AT-SPI2 事件: {kind}");
    true
}

// =====================================================================
// 微信状态检测
// =====================================================================

/// 检测微信当前状态
async fn check_wechat_status(conn: &zbus::Connection) -> (WeChatStatus, Option<CachedNodes>) {
    let registry = NodeRef {
        bus: "org.a11y.atspi.Registry".to_string(),
        path: "/org/a11y/atspi/accessible/root".try_into().unwrap(),
    };

    let app_count = get_child_count(conn, &registry).await;
    if app_count == 0 {
        return (WeChatStatus::NotRunning, None);
    }

    // 查找微信应用
    let mut wechat_node: Option<NodeRef> = None;
    for i in 0..app_count {
        let Some(app_node) = get_child_at_index(conn, &registry, i).await else { continue };
        let app_name = get_name(conn, &app_node).await;
        if is_wechat_app(&app_name) {
            wechat_node = Some(app_node);
            break;
        }
    }

    let Some(wechat) = wechat_node else {
        return (WeChatStatus::NotRunning, None);
    };

    // 尝试查找 Chats 列表 — 有就是已登录
    let chats = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        find_node(conn, &wechat, "list", "Chats"),
    ).await;

    match chats {
        Ok(Some(node)) => {
            let mut cached = CachedNodes::default();
            cached.chats_list = Some(node);
            (WeChatStatus::LoggedIn, Some(cached))
        }
        _ => (WeChatStatus::LoginScreen, None),
    }
}

/// 等待微信登录完成，返回初始缓存
async fn wait_for_wechat_login(conn: &zbus::Connection) -> CachedNodes {
    let mut last_status = WeChatStatus::NotRunning;

    loop {
        let (status, cached) = check_wechat_status(conn).await;

        if status != last_status {
            match &status {
                WeChatStatus::NotRunning => {
                    warn!("❌ 微信未启动, 等待微信进程...");
                }
                WeChatStatus::LoginScreen => {
                    info!("📱 微信已启动, 等待扫码登录...");
                    info!("   请打开 VNC (http://localhost:6080/vnc.html) 扫码登录");
                }
                WeChatStatus::LoggedIn => {
                    info!("✅ 微信已登录!");
                }
            }
            last_status = status;
        }

        if last_status == WeChatStatus::LoggedIn {
            return cached.unwrap_or_default();
        }

        tokio::time::sleep(LOGIN_CHECK_INTERVAL).await;
    }
}

// =====================================================================
// 消息扫描 (定向搜索 + 缓存策略)
// =====================================================================

/// 扫描微信聊天列表和消息列表
///
/// 快速路径: 使用缓存的 NodeRef 直接读取子项
/// 慢速路径: 首次 / 缓存失效时, DFS 搜索整棵树
async fn scan_wechat_messages(conn: &zbus::Connection, cache: &CachedNodes) -> ScanResult {
    let mut messages = Vec::new();
    let mut new_cache = cache.clone();

    // --- 快速路径: 缓存命中 ---

    if let Some(ref chats_node) = cache.chats_list {
        let items = collect_list_item_names(conn, chats_node).await;
        if !items.is_empty() {
            debug!("📋 [缓存] Chats: {} 项", items.len());
            push_unique(&mut messages, &items);
        } else {
            info!("📋 缓存失效, 将重新搜索");
            new_cache.chats_list = None;
        }
    }

    if let Some(ref msgs_node) = cache.messages_list {
        let items = collect_list_item_names(conn, msgs_node).await;
        if !items.is_empty() {
            debug!("💬 [缓存] Messages: {} 项", items.len());
            push_unique(&mut messages, &items);
        }
    }

    // 缓存命中且有数据 → 直接返回
    if !messages.is_empty() && new_cache.chats_list.is_some() {
        return ScanResult { messages, cached: new_cache };
    }

    // --- 慢速路径: 完整搜索 ---

    let registry = NodeRef {
        bus: "org.a11y.atspi.Registry".to_string(),
        path: "/org/a11y/atspi/accessible/root".try_into().unwrap(),
    };

    let app_count = get_child_count(conn, &registry).await;
    info!("🔍 AT-SPI2 Registry: {app_count} 个应用");

    for i in 0..app_count {
        let Some(app_node) = get_child_at_index(conn, &registry, i).await else { continue };
        let app_name = get_name(conn, &app_node).await;

        if !is_wechat_app(&app_name) {
            continue;
        }
        info!("🔍 扫描: {app_name} (bus: {})", app_node.bus);

        // 搜索 Chats 列表
        if new_cache.chats_list.is_none() {
            if let Some(node) = find_node(conn, &app_node, "list", "Chats").await {
                let items = collect_list_item_names(conn, &node).await;
                info!("📋 Chats: {} 项", items.len());
                new_cache.chats_list = Some(node);
                push_unique(&mut messages, &items);
            }
        }

        // 搜索 Messages 列表
        if new_cache.messages_list.is_none() {
            if let Some(node) = find_node(conn, &app_node, "list", "Messages").await {
                let items = collect_list_item_names(conn, &node).await;
                new_cache.messages_list = Some(node);
                if !items.is_empty() {
                    info!("💬 Messages: {} 项", items.len());
                    push_unique(&mut messages, &items);
                }
            }
        }

        // 搜索新消息提醒按钮
        if let Some(btn) = find_node(conn, &app_node, "push button", "new message").await {
            let name = get_name(conn, &btn).await;
            if !name.is_empty() {
                info!("🔔 {name}");
            }
        }
    }

    ScanResult { messages, cached: new_cache }
}

// =====================================================================
// D-Bus 底层调用 (所有调用带 500ms 超时)
// =====================================================================

/// D-Bus call_method 的超时包装
async fn call_with_timeout(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
    iface: Option<&str>,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType + Sync),
) -> Option<zbus::Message> {
    match tokio::time::timeout(
        DBUS_CALL_TIMEOUT,
        conn.call_method(Some(bus), path, iface, method, body),
    ).await {
        Ok(Ok(reply)) => Some(reply),
        Ok(Err(e)) => { debug!("D-Bus {method}: {e}"); None }
        Err(_) => { debug!("D-Bus {method}: 超时"); None }
    }
}

/// 获取节点子元素数量
async fn get_child_count(conn: &zbus::Connection, node: &NodeRef) -> i32 {
    let reply = match call_with_timeout(
        conn, &node.bus, node.path.as_str(),
        Some("org.freedesktop.DBus.Properties"), "Get",
        &(IFACE_ACCESSIBLE, "ChildCount"),
    ).await {
        Some(r) => r,
        None => return 0,
    };
    let val: zbus::zvariant::OwnedValue = match reply.body().deserialize() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if let Ok(c) = <i32>::try_from(&val) { return c; }
    if let Ok(c) = <u32>::try_from(&val) { return c as i32; }
    0
}

/// 获取指定索引的子节点
async fn get_child_at_index(conn: &zbus::Connection, node: &NodeRef, idx: i32) -> Option<NodeRef> {
    let reply = call_with_timeout(
        conn, &node.bus, node.path.as_str(),
        Some(IFACE_ACCESSIBLE), "GetChildAtIndex", &(idx),
    ).await?;
    let (bus, path): (String, OwnedObjectPath) = reply.body().deserialize().ok()?;
    Some(NodeRef { bus, path })
}

/// 获取节点名称
async fn get_name(conn: &zbus::Connection, node: &NodeRef) -> String {
    let reply = match call_with_timeout(
        conn, &node.bus, node.path.as_str(),
        Some("org.freedesktop.DBus.Properties"), "Get",
        &(IFACE_ACCESSIBLE, "Name"),
    ).await {
        Some(r) => r,
        None => return String::new(),
    };
    let val: zbus::zvariant::OwnedValue = match reply.body().deserialize() {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    <String>::try_from(val).unwrap_or_default()
}

/// 获取节点角色名
async fn get_role_name(conn: &zbus::Connection, node: &NodeRef) -> String {
    let reply = match call_with_timeout(
        conn, &node.bus, node.path.as_str(),
        Some(IFACE_ACCESSIBLE), "GetRoleName", &(),
    ).await {
        Some(r) => r,
        None => return String::new(),
    };
    reply.body().deserialize::<String>().unwrap_or_default()
}

// =====================================================================
// 树搜索
// =====================================================================

/// DFS 搜索指定 (role, name) 的节点
async fn find_node(
    conn: &zbus::Connection,
    root: &NodeRef,
    target_role: &str,
    target_name: &str,
) -> Option<NodeRef> {
    find_node_recursive(conn, root, target_role, target_name, 0).await
}

async fn find_node_recursive(
    conn: &zbus::Connection,
    node: &NodeRef,
    target_role: &str,
    target_name: &str,
    depth: u32,
) -> Option<NodeRef> {
    if depth > MAX_SEARCH_DEPTH {
        return None;
    }

    let role = get_role_name(conn, node).await;
    let name = get_name(conn, node).await;

    if role == target_role && name.contains(target_name) {
        return Some(node.clone());
    }

    let count = get_child_count(conn, node).await;
    for i in 0..count.min(20) {
        if let Some(child) = get_child_at_index(conn, node, i).await {
            if let Some(found) = Box::pin(find_node_recursive(
                conn, &child, target_role, target_name, depth + 1,
            )).await {
                return Some(found);
            }
        }
    }
    None
}

/// 收集 list 节点的直接子项名称
async fn collect_list_item_names(conn: &zbus::Connection, list_node: &NodeRef) -> Vec<String> {
    let count = get_child_count(conn, list_node).await;
    let mut items = Vec::with_capacity(count.min(30) as usize);

    for i in 0..count.min(30) {
        if let Some(child) = get_child_at_index(conn, list_node, i).await {
            let name = get_name(conn, &child).await;
            let trimmed = name.trim().to_string();
            if trimmed.len() > 1 {
                items.push(trimmed);
            }
        }
    }
    items
}

// =====================================================================
// 辅助函数
// =====================================================================

/// 判断应用名是否属于微信
fn is_wechat_app(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("wechat") || lower.contains("weixin") || name.contains("微信")
}

/// 解析后的聊天列表项
#[derive(Debug, Clone)]
struct ChatItem {
    /// 联系人/群名
    sender: String,
    /// 消息预览 (文本内容或 [Photo] 等标记)
    preview: String,
    /// 消息类型: "text", "photo", "video", "audio", "namecard", "unknown"
    msg_type: String,
    /// 未读消息数
    unread: u32,
    /// 时间 (如 "11:47")
    time: String,
}

/// 比较新旧消息列表，返回有变化的项
///
/// 策略: 按联系人名比较，如果同一联系人的预览文本变了 → 有新消息
fn diff_messages(old: &[String], new: &[String]) -> Vec<String> {
    use std::collections::HashMap;

    // 解析旧列表: sender → preview
    let old_map: HashMap<String, String> = old.iter()
        .map(|raw| {
            let item = parse_chat_item(raw);
            (item.sender, item.preview)
        })
        .collect();

    let mut changed = Vec::new();
    for raw in new {
        let item = parse_chat_item(raw);
        if item.preview.is_empty() {
            continue; // 无未读，跳过
        }
        match old_map.get(&item.sender) {
            None => changed.push(raw.clone()),           // 新联系人
            Some(old_preview) if *old_preview != item.preview => {
                changed.push(raw.clone());               // 预览变了 = 新消息
            }
            _ => {}                                       // 没变化
        }
    }
    changed
}

/// 解析聊天列表项的原始字符串
///
/// 格式样本:
///   "NIUNIU 3 unread message(s) 测试1 10:52"
///   "NIUNIU 7 unread message(s) [Photo]  11:55"
///   "NIUNIU 6 unread message(s) [Audio] 1\" 11:47"
///   "NIUNIU 9 unread message(s) [Name Card] 自信音游Fu 11:58"
///   "File Transfer  "
fn parse_chat_item(raw: &str) -> ChatItem {
    let trimmed = raw.trim();

    // 无未读消息: 只有联系人名 + 尾部空格
    let unread_marker = " unread message(s) ";
    let Some(marker_pos) = trimmed.find(unread_marker) else {
        return ChatItem {
            sender: trimmed.to_string(),
            preview: String::new(),
            msg_type: "none".to_string(),
            unread: 0,
            time: String::new(),
        };
    };

    // 找到 "N unread message(s)" 的起始位置
    // marker_pos 指向 " unread..." 前面的空格位置
    // 往前找数字开头: "NIUNIU 3 unread..." → 找到 "3" 的位置
    let before_marker = &trimmed[..marker_pos];
    let (sender, unread) = match before_marker.rfind(' ') {
        Some(space_pos) => {
            let name = &before_marker[..space_pos];
            let num_str = &before_marker[space_pos + 1..];
            let n = num_str.parse::<u32>().unwrap_or(0);
            (name.to_string(), n)
        }
        None => (before_marker.to_string(), 0),
    };

    // 提取预览 + 时间: "测试1 10:52" 或 "[Photo]  11:55"
    let after_marker = &trimmed[marker_pos + unread_marker.len()..];

    // 时间在最后，格式 HH:MM (或 Yesterday 等)
    // 尝试从末尾提取时间
    let (preview, time) = extract_time(after_marker);

    // 判断消息类型
    let msg_type = classify_preview(&preview);

    ChatItem { sender, preview, msg_type, unread, time }
}

/// 从预览字符串末尾提取时间
fn extract_time(s: &str) -> (String, String) {
    let trimmed = s.trim();

    // 尝试匹配末尾的 HH:MM 格式
    if trimmed.len() >= 5 {
        let last5 = &trimmed[trimmed.len() - 5..];
        if last5.chars().nth(2) == Some(':')
            && last5[..2].chars().all(|c| c.is_ascii_digit())
            && last5[3..].chars().all(|c| c.is_ascii_digit())
        {
            let preview = trimmed[..trimmed.len() - 5].trim_end().to_string();
            return (preview, last5.to_string());
        }
    }

    // 没找到时间，整个作为预览
    (trimmed.to_string(), String::new())
}

/// 根据预览内容分类消息类型
fn classify_preview(preview: &str) -> String {
    if preview.starts_with("[Photo]") { return "photo".into(); }
    if preview.starts_with("[Video]") { return "video".into(); }
    if preview.starts_with("[Audio]") { return "audio".into(); }
    if preview.starts_with("[Name Card]") { return "namecard".into(); }
    if preview.starts_with("[Sticker]") { return "sticker".into(); }
    if preview.starts_with("[File]") { return "file".into(); }
    if preview.starts_with("[Link]") { return "link".into(); }
    if preview.starts_with("[Location]") { return "location".into(); }
    if preview.starts_with("[Mini Program]") { return "miniprogram".into(); }
    if preview.starts_with("[Red Packet]") { return "redpacket".into(); }
    if preview.starts_with('[') { return "other".into(); }
    "text".into()
}

/// 解析为 (sender, text) 用于 WxMessage 生成
fn parse_message(raw: &str) -> (String, String) {
    let item = parse_chat_item(raw);
    if item.preview.is_empty() {
        return (item.sender, String::new());
    }
    (item.sender, item.preview)
}

/// 去重追加字符串到 Vec
fn push_unique(target: &mut Vec<String>, items: &[String]) {
    for item in items {
        if !target.contains(item) {
            target.push(item.clone());
        }
    }
}

/// 查找微信进程 PID (通过 /proc)
fn find_wechat_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return pids };

    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };

        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if is_wechat_app(&exe.to_string_lossy()) {
                pids.push(pid);
                continue;
            }
        }
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            if is_wechat_app(comm.trim()) {
                pids.push(pid);
            }
        }
    }
    pids
}
