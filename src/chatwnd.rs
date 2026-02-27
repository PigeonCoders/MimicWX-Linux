//! 独立聊天窗口 (ChatWnd)
//!
//! 借鉴 wxauto 的 ChatWnd 设计：每个独立弹出的聊天窗口拥有自己的
//! AT-SPI2 节点引用，可以独立读取消息和发送，互不干扰。
//!
//! 使用方式 (对应 wxauto):
//!   wxauto: wx.AddListenChat("张三") → 弹出独立窗口 → ChatWnd("张三")
//!   MimicWX: POST /listen {"who":"张三"} → 双击弹出 → ChatWnd 实例化

use anyhow::Result;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::atspi::{AtSpi, NodeRef};
use crate::input::InputEngine;
use crate::wechat::{ChatMessage, ms, parse_message_item, is_structural_role};

// =====================================================================
// ChatWnd — 独立聊天窗口
// =====================================================================

pub struct ChatWnd {
    /// 聊天对象名称
    pub who: String,
    /// AT-SPI2 引用
    atspi: Arc<AtSpi>,
    /// 该窗口的 AT-SPI2 根节点 (frame)
    pub window_node: NodeRef,
    /// 缓存的输入框节点 (DFS初始化时找到, 后续发送复用)
    edit_box_node: Option<NodeRef>,
    /// 缓存的消息列表节点 (DFS初始化时找到, 后续监听复用)
    msg_list_node: Option<NodeRef>,
    /// 已读消息计数 (last_count 追踪法)
    last_count: i32,
    /// 是否自动保存图片
    pub save_pic: bool,
    /// 是否自动保存文件
    pub save_file: bool,
}

impl ChatWnd {
    /// 创建独立聊天窗口实例
    ///
    /// `window_node` 应该是 AT-SPI2 树中该独立窗口的 frame 节点
    pub fn new(who: String, atspi: Arc<AtSpi>, window_node: NodeRef) -> Self {
        info!("📌 创建 ChatWnd: {who}");
        Self {
            who,
            atspi,
            window_node,
            edit_box_node: None,
            msg_list_node: None,
            last_count: 0,
            save_pic: false,
            save_file: false,
        }
    }

    /// 刷新窗口节点引用 (窗口可能被重新创建)
    pub fn update_window_node(&mut self, node: NodeRef) {
        self.window_node = node;
    }

    /// 检查独立窗口是否仍然存活
    /// 通过 AT-SPI2 bbox 是否返回有效值来判断
    pub async fn is_alive(&self) -> bool {
        if let Some(bbox) = self.atspi.bbox(&self.window_node).await {
            bbox.w > 0 && bbox.h > 0
        } else {
            false
        }
    }

    /// 初始化输入框缓存 (DFS 搜索, 只跑一次)
    ///
    /// 不限制结构性角色, 遍历所有子节点找 `entry`/`text`
    pub async fn init_edit_box(&mut self) {
        if self.edit_box_node.is_some() {
            return; // 已缓存
        }
        if let Some(node) = self.dfs_find_edit_box(&self.window_node.clone(), 0).await {
            info!("📌 [ChatWnd] 缓存输入框节点: {}", self.who);
            self.edit_box_node = Some(node);
        } else {
            info!("📌 [ChatWnd] 未找到输入框, 将使用偏移量方案: {}", self.who);
        }
    }

    /// DFS 搜索输入框 (不限制角色, 最大深度 15)
    fn dfs_find_edit_box<'a>(
        &'a self,
        node: &'a NodeRef,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<NodeRef>> + Send + 'a>> {
        Box::pin(async move {
            if depth > 15 { return None; }

            let count = self.atspi.child_count(node).await;
            for i in 0..count.min(30) {
                if let Some(child) = self.atspi.child_at(node, i).await {
                    let role = self.atspi.role(&child).await;
                    if role == "entry" || role == "text" {
                        return Some(child);
                    }
                    // 跳过消息列表 (list) 和滚动容器里的消息, 只搜索非消息区域
                    if role == "list" { continue; }
                    if let Some(found) = self.dfs_find_edit_box(&child, depth + 1).await {
                        return Some(found);
                    }
                }
            }
            None
        })
    }
    /// 初始化消息列表缓存 (DFS 搜索, 只跑一次)
    pub async fn init_msg_list(&mut self) {
        if self.msg_list_node.is_some() {
            return;
        }
        if let Some(node) = self.dfs_find_msg_list(&self.window_node.clone(), 0).await {
            info!("📌 [ChatWnd] 缓存消息列表节点: {}", self.who);
            self.msg_list_node = Some(node);
        } else {
            info!("📌 [ChatWnd] 未找到消息列表: {}", self.who);
        }
    }

    /// DFS 搜索消息列表 (找 role=list 且 name 包含 消息/Messages)
    fn dfs_find_msg_list<'a>(
        &'a self,
        node: &'a NodeRef,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<NodeRef>> + Send + 'a>> {
        Box::pin(async move {
            if depth > 15 { return None; }

            let count = self.atspi.child_count(node).await;
            for i in 0..count.min(30) {
                if let Some(child) = self.atspi.child_at(node, i).await {
                    let role = self.atspi.role(&child).await;
                    let name = self.atspi.name(&child).await;

                    if role == "list" && (name.contains("消息") || name.contains("Messages") || name.contains("Message")) {
                        return Some(child);
                    }
                    // 跳过其他 list (非消息列表)
                    if role == "list" { continue; }
                    if let Some(found) = self.dfs_find_msg_list(&child, depth + 1).await {
                        return Some(found);
                    }
                }
            }
            None
        })
    }
    // =================================================================
    // 消息列表
    // =================================================================

    /// 在此独立窗口中查找消息列表
    pub async fn find_message_list(&self) -> Option<NodeRef> {
        self.find_list_in_window(&["消息", "Messages"]).await
    }

    /// 在此独立窗口中查找输入框
    pub async fn find_edit_box(&self) -> Option<NodeRef> {
        self.find_by_role_in_window("entry").await
            .or(self.find_by_role_in_window("text").await)
    }

    /// BFS 查找列表节点 (在窗口范围内)
    async fn find_list_in_window(&self, keywords: &[&str]) -> Option<NodeRef> {
        let mut frontier = vec![self.window_node.clone()];

        for depth in 0..20 {
            if frontier.is_empty() { return None; }
            let mut next_frontier = Vec::new();

            for node in &frontier {
                let count = self.atspi.child_count(node).await;
                for i in 0..count.min(20) {
                    if let Some(child) = self.atspi.child_at(node, i).await {
                        let role = self.atspi.role(&child).await;
                        let name = self.atspi.name(&child).await;

                        if role == "list" && keywords.iter().any(|k| name.contains(k)) {
                            debug!("[ChatWnd::find_list] FOUND [{role}] '{name}' at depth {depth}");
                            return Some(child);
                        }

                        if is_structural_role(&role) {
                            next_frontier.push(child);
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
        None
    }

    /// BFS 查找特定 role 的节点 (在窗口范围内)
    async fn find_by_role_in_window(&self, target_role: &str) -> Option<NodeRef> {
        let mut frontier = vec![self.window_node.clone()];

        for _depth in 0..20 {
            if frontier.is_empty() { return None; }
            let mut next_frontier = Vec::new();

            for node in &frontier {
                let count = self.atspi.child_count(node).await;
                for i in 0..count.min(20) {
                    if let Some(child) = self.atspi.child_at(node, i).await {
                        let role = self.atspi.role(&child).await;
                        if role == target_role {
                            return Some(child);
                        }
                        if is_structural_role(&role) {
                            next_frontier.push(child);
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
        None
    }

    // =================================================================
    // 消息读取
    // =================================================================

    /// 获取所有已加载的消息
    pub async fn get_all_messages(&self) -> Vec<ChatMessage> {
        // 优先使用缓存的消息列表节点
        let msg_list = if let Some(ref cached) = self.msg_list_node {
            cached.clone()
        } else {
            match self.find_message_list().await {
                Some(l) => l,
                None => {
                    debug!("[ChatWnd::get_all_messages] {} 未找到消息列表", self.who);
                    return Vec::new();
                }
            }
        };

        let count = self.atspi.child_count(&msg_list).await;
        let mut messages = Vec::new();

        for i in 0..count.min(100) {
            if let Some(child) = self.atspi.child_at(&msg_list, i).await {
                let msg = self.parse_message_item(&child, i).await;
                messages.push(msg);
            }
        }

        messages
    }

    /// 获取新消息 (last_count 追踪法: 只读取新增的消息)
    pub async fn get_new_messages(&mut self) -> Vec<ChatMessage> {
        // 获取消息列表节点
        let msg_list = if let Some(ref cached) = self.msg_list_node {
            cached.clone()
        } else {
            match self.find_message_list().await {
                Some(l) => l,
                None => return Vec::new(),
            }
        };

        let count = self.atspi.child_count(&msg_list).await;
        debug!("[ChatWnd::get_new_messages] {} count={} last_count={}", self.who, count, self.last_count);
        if count < self.last_count {
            // 消息列表变小了 (窗口重建/消息被清理), 重置
            debug!("[ChatWnd::get_new_messages] {} count 减少, 重置 last_count", self.who);
            self.last_count = count;
            return Vec::new();
        }
        if count == self.last_count {
            return Vec::new(); // 没有新消息
        }

        // 只读取 last_count..count 的新消息
        let mut new_msgs = Vec::new();
        for i in self.last_count..count.min(self.last_count + 50) {
            if let Some(child) = self.atspi.child_at(&msg_list, i).await {
                let msg = self.parse_message_item(&child, i).await;
                new_msgs.push(msg);
            }
        }

        self.last_count = count;
        new_msgs
    }

    /// 标记当前所有消息为已读
    pub async fn mark_all_read(&mut self) {
        let msg_list = if let Some(ref cached) = self.msg_list_node {
            cached.clone()
        } else {
            match self.find_message_list().await {
                Some(l) => l,
                None => {
                    debug!("[ChatWnd::mark_all_read] {} 未找到消息列表", self.who);
                    return;
                }
            }
        };

        let count = self.atspi.child_count(&msg_list).await;
        self.last_count = count;
        debug!("[ChatWnd::mark_all_read] {} 标记 {} 条消息为已读", self.who, count);
    }

    // =================================================================
    // 消息解析 (借鉴 wxauto _split)
    // =================================================================

    /// 解析单个消息项
    async fn parse_message_item(&self, item: &NodeRef, index: i32) -> ChatMessage {
        parse_message_item(&self.atspi, item, index).await
    }

    // =================================================================
    // 发送消息
    // =================================================================

    /// 在此独立窗口中发送消息
    ///
    /// 简化流程: 点击窗口聚焦 → 粘贴 → Enter
    /// (独立聊天窗口会自动聚焦输入框)
    pub async fn send_message(
        &self,
        engine: &mut InputEngine,
        text: &str,
    ) -> Result<(bool, bool, String)> {
        info!("📤 [ChatWnd] 发送: [{}] → {text}", self.who);

        // 1. 激活窗口并聚焦输入框
        self.activate_and_focus_input(engine).await?;

        // 2. 粘贴消息 (xclip + Ctrl+V)
        engine.paste_text(text).await?;
        tokio::time::sleep(ms(300)).await;

        // 3. Enter 发送
        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        // 4. 验证发送
        let verified = self.verify_sent(text).await;

        let msg = if verified { "消息已发送" } else { "消息已发送 (未验证)" };
        info!("✅ [ChatWnd] 完成: [{}] verified={verified}", self.who);
        Ok((true, verified, msg.into()))
    }

    /// 在此独立窗口中发送图片
    ///
    /// 流程: 激活窗口 → 点击输入框 → 粘贴图片 → Enter
    /// (图片不做文本验证)
    pub async fn send_image(
        &self,
        engine: &mut InputEngine,
        image_path: &str,
    ) -> Result<(bool, bool, String)> {
        info!("🖼️ [ChatWnd] 发送图片: [{}] → {image_path}", self.who);

        // 1. 激活窗口并聚焦输入框
        self.activate_and_focus_input(engine).await?;

        // 2. 粘贴图片
        engine.paste_image(image_path).await?;
        tokio::time::sleep(ms(500)).await;

        // 3. Enter 发送
        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        info!("✅ [ChatWnd] 图片发送完成: [{}]", self.who);
        Ok((true, false, "图片已发送 (独立窗口)".into()))
    }

    /// 激活独立窗口并聚焦输入框 (send_message/send_image 的公共前置步骤)
    async fn activate_and_focus_input(&self, engine: &mut InputEngine) -> Result<()> {
        // 1. 将独立窗口提到前台 (xdotool 按窗口标题激活)
        let activated = std::process::Command::new("xdotool")
            .args(["search", "--name", &self.who])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .and_then(|o| {
                let wids = String::from_utf8_lossy(&o.stdout);
                wids.lines().next().map(|id| id.trim().to_string())
            })
            .map(|wid| {
                let _ = std::process::Command::new("xdotool")
                    .args(["windowactivate", &wid])
                    .stderr(std::process::Stdio::null())
                    .status();
                true
            })
            .unwrap_or(false);
        if !activated {
            // 回退: 点击标题栏
            if let Some(bbox) = self.atspi.bbox(&self.window_node).await {
                let cx = bbox.x + bbox.w / 2;
                engine.click(cx, bbox.y + 30).await?;
            }
        }
        tokio::time::sleep(ms(300)).await;

        // 2. 点击输入框 (缓存的精确坐标, 或偏移量回退)
        if let Some(ref edit_node) = self.edit_box_node {
            // 精确方案: 用缓存节点的 bbox
            if let Some(eb) = self.atspi.bbox(edit_node).await {
                let (cx, cy) = eb.center();
                engine.click(cx, cy).await?;
                tokio::time::sleep(ms(200)).await;
            }
        } else {
            // 偏移量回退: 点击窗口底部输入区域
            if let Some(bbox) = self.atspi.bbox(&self.window_node).await {
                let cx = bbox.x + bbox.w / 2;
                engine.click(cx, bbox.y + bbox.h - 50).await?;
                tokio::time::sleep(ms(200)).await;
            }
        }

        Ok(())
    }

    /// 验证消息是否出现在消息列表末尾
    async fn verify_sent(&self, text: &str) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(ms(500)).await;
            }
            if let Some(msg_list) = self.find_message_list().await {
                let count = self.atspi.child_count(&msg_list).await;
                if count <= 0 { continue; }

                // 检查最后几条消息 (因为可能有系统消息插入)
                let check_range = 3.min(count);
                for i in (count - check_range)..count {
                    if let Some(child) = self.atspi.child_at(&msg_list, i).await {
                        let name = self.atspi.name(&child).await;
                        let trimmed = name.trim();
                        if trimmed.contains(text) || text.contains(trimmed) {
                            info!("✅ [ChatWnd] 验证成功 (attempt {attempt}, item {i})");
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}
