//! AT-SPI2 底层原语
//!
//! 封装 zbus D-Bus 调用，提供节点遍历、属性读取、坐标获取等能力。
//! 所有 D-Bus 调用带 500ms 超时保护。
//!
//! 连接策略 (按优先级):
//! 1. 通过 session bus 上的 org.a11y.Bus 接口获取 AT-SPI2 bus 地址
//! 2. 使用 AT_SPI_BUS_ADDRESS 环境变量
//! 3. 标准 AccessibilityConnection (自动发现)
//! 4. 扫描 ~/.cache/at-spi/ 下所有 bus socket
//!
//! 支持运行时重连: 当检测到 Registry 为空时可调用 reconnect() 重新发现。

use anyhow::Result;
use serde::Serialize;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// =====================================================================
// 常量
// =====================================================================

const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const IFACE_COMPONENT: &str = "org.a11y.atspi.Component";
const IFACE_TEXT: &str = "org.a11y.atspi.Text";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

// =====================================================================
// 类型
// =====================================================================

/// AT-SPI2 节点引用 (bus_name + object_path)
#[derive(Debug, Clone)]
pub struct NodeRef {
    pub bus: String,
    pub path: OwnedObjectPath,
}

/// 控件坐标 (屏幕像素)
#[derive(Debug, Clone, Copy)]
pub struct BBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl BBox {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// 调试用：树节点信息
#[derive(Serialize)]
pub struct TreeNode {
    pub depth: u32,
    pub role: String,
    pub name: String,
    pub children: i32,
}

// =====================================================================
// AtSpi — 核心结构
// =====================================================================

pub struct AtSpi {
    conn: RwLock<zbus::Connection>,
}

impl AtSpi {
    /// 建立 AT-SPI2 连接
    ///
    /// 策略：
    /// 1. 通过 session bus 上 org.a11y.Bus 获取 AT-SPI2 bus 地址 (最可靠)
    /// 2. 使用 AT_SPI_BUS_ADDRESS 环境变量
    /// 3. 标准 AccessibilityConnection
    /// 4. 扫描 ~/.cache/at-spi/ 下 bus socket
    pub async fn connect() -> Result<Self> {
        // 尝试多种方式获取连接
        if let Some(instance) = Self::try_connect_all().await {
            return Ok(instance);
        }

        // 最终回退: 标准连接 (可能后续 WeChat 启动后会注册上来)
        let a11y = atspi::AccessibilityConnection::new().await?;
        let conn = a11y.connection().clone();
        info!("🔗 AT-SPI2 连接就绪 (标准发现, 等待应用注册)");
        Ok(Self { conn: RwLock::new(conn) })
    }

    /// 尝试所有连接方式，返回第一个有应用注册的连接
    async fn try_connect_all() -> Option<Self> {
        // 方法1: 通过 session bus 上 org.a11y.Bus 发现
        if let Some(instance) = Self::connect_via_a11y_bus().await {
            return Some(instance);
        }

        // 方法2: 使用 AT_SPI_BUS_ADDRESS 环境变量
        if let Ok(addr) = std::env::var("AT_SPI_BUS_ADDRESS") {
            if !addr.is_empty() {
                debug!("尝试 AT_SPI_BUS_ADDRESS: {addr}");
                if let Some(instance) = Self::connect_to_address(&addr).await {
                    info!("🔗 AT-SPI2 连接就绪 (AT_SPI_BUS_ADDRESS)");
                    return Some(instance);
                }
            }
        }

        // 方法3: 标准 AccessibilityConnection
        if let Ok(a11y) = atspi::AccessibilityConnection::new().await {
            let conn = a11y.connection().clone();
            let instance = Self { conn: RwLock::new(conn) };
            if let Some(root) = Self::registry() {
                let count = instance.child_count(&root).await;
                if count > 1 {
                    info!("🔗 AT-SPI2 连接就绪 (标准发现, {count} 个应用)");
                    return Some(instance);
                }
                debug!("标准连接只有 {count} 个子节点");
            }
        }

        // 方法4: 扫描 socket 文件
        if let Some(instance) = Self::scan_bus_sockets().await {
            info!("🔗 AT-SPI2 连接就绪 (扫描发现)");
            return Some(instance);
        }

        None
    }

    /// 通过 session bus 上 org.a11y.Bus 接口获取 AT-SPI2 bus 地址
    async fn connect_via_a11y_bus() -> Option<Self> {
        debug!("尝试通过 org.a11y.Bus 发现 AT-SPI2 bus...");

        // 先连接 session bus
        let session = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                debug!("session bus 连接失败: {e}");
                return None;
            }
        };

        // 调用 org.a11y.Bus.GetAddress()
        let reply = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            session.call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.a11y.Bus"),
                "GetAddress",
                &(),
            ),
        ).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                debug!("org.a11y.Bus.GetAddress 调用失败: {e}");
                return None;
            }
            Err(_) => {
                debug!("org.a11y.Bus.GetAddress 超时");
                return None;
            }
        };

        let addr: String = reply.body().deserialize().ok()?;
        if addr.is_empty() {
            debug!("org.a11y.Bus 返回空地址");
            return None;
        }

        info!("发现 AT-SPI2 bus 地址: {addr}");
        Self::connect_to_address(&addr).await
    }

    /// 连接到指定地址的 AT-SPI2 bus，并验证是否有应用注册
    async fn connect_to_address(addr: &str) -> Option<Self> {
        // 解析地址中的 socket 路径
        let socket_path = if addr.starts_with("unix:path=") {
            let path_part = addr.strip_prefix("unix:path=")?;
            // 去掉逗号后的部分 (如 ,guid=xxx)
            path_part.split(',').next()?.to_string()
        } else {
            debug!("  不支持的地址格式: {addr}");
            return None;
        };

        debug!("  连接 socket: {socket_path}");

        let stream = match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                debug!("  socket 连接失败: {e}");
                return None;
            }
        };

        let conn = match zbus::connection::Builder::unix_stream(stream)
            .build()
            .await
        {
            Ok(c) => c,
            Err(e) => {
                debug!("  zbus 连接失败: {e}");
                return None;
            }
        };

        let instance = Self { conn: RwLock::new(conn) };
        if let Some(root) = Self::registry() {
            let count = instance.child_count(&root).await;
            debug!("  bus {socket_path} 有 {count} 个子节点");
            if count > 0 {
                info!("🔗 找到有效 AT-SPI2 bus: {socket_path} ({count} 个应用)");
                return Some(instance);
            }
        }

        // 即使 0 个子节点也返回连接 (可能应用尚未注册)
        debug!("  bus {socket_path} 暂无应用，但保留连接");
        Some(instance)
    }

    /// 运行时重连: 重新发现 AT-SPI2 bus 并更新连接
    ///
    /// 当 Registry 持续返回 0 个子节点时调用此方法。
    pub async fn reconnect(&self) -> bool {
        info!("🔄 尝试重新发现 AT-SPI2 bus...");

        // 尝试通过 org.a11y.Bus 获取最新地址
        if let Some(new_conn) = Self::connect_via_a11y_bus().await {
            let new_inner = new_conn.conn.read().await.clone();
            // 验证新连接有应用
            if let Some(root) = Self::registry() {
                let tmp = Self { conn: RwLock::new(new_inner.clone()) };
                let count = tmp.child_count(&root).await;
                if count > 0 {
                    let mut conn = self.conn.write().await;
                    *conn = new_inner;
                    info!("🔄 重连成功 (org.a11y.Bus, {count} 个应用)");
                    return true;
                }
            }
        }

        // 扫描 socket
        if let Some(new_conn) = Self::scan_bus_sockets().await {
            let new_inner = new_conn.conn.read().await.clone();
            let mut conn = self.conn.write().await;
            *conn = new_inner;
            info!("🔄 重连成功 (socket 扫描)");
            return true;
        }

        debug!("🔄 重连未发现新的有效 bus");
        false
    }

    /// 扫描 ~/.cache/at-spi/ 下的所有 bus socket 文件
    async fn scan_bus_sockets() -> Option<Self> {
        use std::os::unix::fs::FileTypeExt;

        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/wechat".into());
        let bus_dir = std::path::PathBuf::from(&home).join(".cache/at-spi");

        let entries = std::fs::read_dir(&bus_dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();

            // 只处理 socket 文件
            if let Ok(meta) = std::fs::metadata(&path) {
                if !meta.file_type().is_socket() {
                    continue;
                }
            } else {
                continue;
            }

            let path_str = path.to_string_lossy().to_string();
            debug!("尝试 AT-SPI2 bus: {path_str}");

            // 用 tokio UnixStream 连接
            let stream = match tokio::net::UnixStream::connect(&path).await {
                Ok(s) => s,
                Err(e) => {
                    debug!("  连接失败: {e}");
                    continue;
                }
            };

            let conn = match zbus::connection::Builder::unix_stream(stream)
                .build()
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    debug!("  zbus 连接失败: {e}");
                    continue;
                }
            };

            let instance = Self { conn: RwLock::new(conn) };
            if let Some(root) = Self::registry() {
                let count = instance.child_count(&root).await;
                if count > 1 {
                    info!("🔗 找到有效 AT-SPI2 bus: {path_str} ({count} 个应用)");
                    return Some(instance);
                }
                debug!("  bus {path_str} 只有 {count} 个子节点, 跳过");
            }
        }
        None
    }

    /// AT-SPI2 Registry 根节点
    pub fn registry() -> Option<NodeRef> {
        Some(NodeRef {
            bus: "org.a11y.atspi.Registry".into(),
            path: "/org/a11y/atspi/accessible/root".try_into().ok()?,
        })
    }

    // =================================================================
    // 属性读取
    // =================================================================

    pub async fn child_count(&self, node: &NodeRef) -> i32 {
        let reply = self.call(
            &node.bus, node.path.as_str(), Some(PROPS), "Get",
            &(IFACE_ACCESSIBLE, "ChildCount"),
        ).await;
        reply.and_then(|r| {
            let v: OwnedValue = r.body().deserialize().ok()?;
            i32::try_from(&v).ok()
                .or_else(|| u32::try_from(&v).ok().map(|n| n as i32))
        }).unwrap_or(0)
    }

    pub async fn child_at(&self, node: &NodeRef, idx: i32) -> Option<NodeRef> {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_ACCESSIBLE), "GetChildAtIndex", &(idx,),
        ).await?;
        let (bus, path): (String, OwnedObjectPath) = reply.body().deserialize().ok()?;
        Some(NodeRef { bus, path })
    }

    pub async fn name(&self, node: &NodeRef) -> String {
        let reply = self.call(
            &node.bus, node.path.as_str(), Some(PROPS), "Get",
            &(IFACE_ACCESSIBLE, "Name"),
        ).await;
        reply.and_then(|r| {
            let v: OwnedValue = r.body().deserialize().ok()?;
            String::try_from(v).ok()
        }).unwrap_or_default()
    }

    pub async fn role(&self, node: &NodeRef) -> String {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_ACCESSIBLE), "GetRoleName", &(),
        ).await;
        reply.and_then(|r| r.body().deserialize::<String>().ok())
            .unwrap_or_default()
    }

    pub async fn bbox(&self, node: &NodeRef) -> Option<BBox> {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_COMPONENT), "GetExtents", &(0u32,),
        ).await?;
        let (x, y, w, h): (i32, i32, i32, i32) = reply.body().deserialize().ok()?;
        Some(BBox { x, y, w, h })
    }

    pub async fn text(&self, node: &NodeRef) -> Option<String> {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_TEXT), "GetText", &(0i32, -1i32),
        ).await?;
        reply.body().deserialize::<String>().ok()
    }

    /// 读取 Description 属性
    pub async fn description(&self, node: &NodeRef) -> String {
        let reply = self.call(
            &node.bus, node.path.as_str(), Some(PROPS), "Get",
            &(IFACE_ACCESSIBLE, "Description"),
        ).await;
        reply.and_then(|r| {
            let v: OwnedValue = r.body().deserialize().ok()?;
            String::try_from(v).ok()
        }).unwrap_or_default()
    }

    /// 获取 Parent 节点
    pub async fn parent(&self, node: &NodeRef) -> Option<NodeRef> {
        let reply = self.call(
            &node.bus, node.path.as_str(), Some(PROPS), "Get",
            &(IFACE_ACCESSIBLE, "Parent"),
        ).await?;
        let v: OwnedValue = reply.body().deserialize().ok()?;
        let (bus, path): (String, OwnedObjectPath) = zbus::zvariant::Value::try_from(v)
            .ok()
            .and_then(|v| v.downcast().ok())?;
        Some(NodeRef { bus, path })
    }

    /// 获取节点状态位集合 (AT-SPI2 StateSet)
    /// 返回 64 位状态标志 (两个 u32 合并)
    pub async fn get_states(&self, node: &NodeRef) -> u64 {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_ACCESSIBLE), "GetState", &(),
        ).await;
        reply.and_then(|r| {
            let states: Vec<u32> = r.body().deserialize().ok()?;
            if states.len() >= 2 {
                Some((states[1] as u64) << 32 | states[0] as u64)
            } else if states.len() == 1 {
                Some(states[0] as u64)
            } else {
                None
            }
        }).unwrap_or(0)
    }

    /// 检查节点是否处于 SELECTED 状态 (AT-SPI2 STATE_SELECTED = bit 25)
    pub async fn is_selected(&self, node: &NodeRef) -> bool {
        let states = self.get_states(node).await;
        states & (1 << 25) != 0
    }

    /// 强制聚焦节点 (将窗口提到前台)
    pub async fn grab_focus(&self, node: &NodeRef) -> bool {
        let reply = self.call(
            &node.bus, node.path.as_str(),
            Some(IFACE_COMPONENT), "GrabFocus", &(),
        ).await;
        reply.and_then(|r| r.body().deserialize::<bool>().ok()).unwrap_or(false)
    }

    // =================================================================
    // 通用搜索原语 (BFS / DFS)
    // =================================================================

    /// BFS 查找节点 (结构性角色穿透, 最大深度 20, 每层最多 20 子节点)
    ///
    /// `matcher(role, name) -> bool`: 返回 true 表示匹配
    pub async fn find_bfs(
        &self, root: &NodeRef,
        matcher: impl Fn(&str, &str) -> bool,
    ) -> Option<NodeRef> {
        self.find_bfs_limited(root, &matcher, 500).await
    }

    /// BFS 查找节点 — 带节点数量上限
    pub async fn find_bfs_limited(
        &self, root: &NodeRef,
        matcher: &impl Fn(&str, &str) -> bool,
        max_nodes: usize,
    ) -> Option<NodeRef> {
        let mut frontier = vec![root.clone()];
        let mut visited = 0usize;

        for _depth in 0..20 {
            if frontier.is_empty() { return None; }
            let mut next = Vec::new();

            for node in &frontier {
                let count = self.child_count(node).await;
                for i in 0..count.min(20) {
                    visited += 1;
                    if visited > max_nodes { return None; }
                    if let Some(child) = self.child_at(node, i).await {
                        let role = self.role(&child).await;
                        let name = self.name(&child).await;
                        if matcher(&role, &name) {
                            return Some(child);
                        }
                        if is_structural_role(&role) {
                            next.push(child);
                        }
                    }
                }
            }
            frontier = next;
        }
        None
    }

    /// DFS 查找节点 (递归, 可控制跳过/递归/匹配)
    ///
    /// `matcher(role, name) -> SearchAction`:
    /// - `Found` = 匹配, 返回此节点
    /// - `Recurse` = 不匹配, 但继续递归子节点
    /// - `Skip` = 不匹配, 跳过此子树
    pub fn find_dfs<'a>(
        &'a self, node: &'a NodeRef,
        matcher: &'a (dyn Fn(&str, &str) -> SearchAction + Send + Sync),
        depth: u32, max_depth: u32, max_children: i32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<NodeRef>> + Send + 'a>> {
        Box::pin(async move {
            if depth > max_depth { return None; }

            let count = self.child_count(node).await;
            for i in 0..count.min(max_children) {
                if let Some(child) = self.child_at(node, i).await {
                    let role = self.role(&child).await;
                    let name = self.name(&child).await;
                    match matcher(&role, &name) {
                        SearchAction::Found => return Some(child),
                        SearchAction::Recurse => {
                            if let Some(found) = self.find_dfs(
                                &child, matcher, depth + 1, max_depth, max_children,
                            ).await {
                                return Some(found);
                            }
                        }
                        SearchAction::Skip => {}
                    }
                }
            }
            None
        })
    }

    // =================================================================
    // 调试树导出
    // =================================================================

    /// 导出 AT-SPI2 树（调试用，限制 200 节点）
    pub async fn dump_tree(&self, root: &NodeRef, max_depth: u32) -> Vec<TreeNode> {
        let mut nodes = Vec::new();
        let mut count = 0u32;
        self.dump_dfs(root, 0, max_depth, &mut nodes, &mut count).await;
        nodes
    }

    fn dump_dfs<'a>(
        &'a self, node: &'a NodeRef, depth: u32, max_depth: u32,
        out: &'a mut Vec<TreeNode>, count: &'a mut u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if depth > max_depth || *count >= 200 { return; }
            *count += 1;

            let role = self.role(node).await;
            let name = self.name(node).await;
            let children = self.child_count(node).await;

            out.push(TreeNode { depth, role: role.clone(), name: name.clone(), children });

            // 消息列表不递归
            if role == "list" && (name.contains("消息") || name.contains("Messages")) {
                return;
            }

            for i in 0..children.min(20) {
                if *count >= 200 { return; }
                if let Some(child) = self.child_at(node, i).await {
                    self.dump_dfs(&child, depth + 1, max_depth, out, count).await;
                }
            }
        })
    }

    // =================================================================
    // D-Bus 底层调用 (带超时)
    // =================================================================

    async fn call(
        &self, bus: &str, path: &str,
        iface: Option<&str>, method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType + Sync),
    ) -> Option<zbus::Message> {
        let conn = self.conn.read().await;
        match tokio::time::timeout(
            CALL_TIMEOUT,
            conn.call_method(Some(bus), path, iface, method, body),
        ).await {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(e)) => { debug!("D-Bus {method}: {e}"); None }
            Err(_) => { debug!("D-Bus {method}: timeout"); None }
        }
    }
}

// =====================================================================
// 搜索辅助类型
// =====================================================================

/// DFS 搜索动作
pub enum SearchAction {
    /// 匹配成功, 返回此节点
    Found,
    /// 不匹配, 继续递归子节点
    Recurse,
    /// 不匹配, 跳过此子树
    Skip,
}

/// 结构性角色: BFS 搜索时应当穿透的容器节点
/// 统一定义, 避免多处硬编码不一致
pub fn is_structural_role(role: &str) -> bool {
    matches!(role,
        "filler" | "layered pane" | "panel" | "frame"
        | "scroll pane" | "viewport" | "section"
        | "split pane" | "splitter" | "page tab list"
        | "page tab" | "tool bar" | "" | "invalid"
    )
}
