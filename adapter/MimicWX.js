/**
 * MimicWX-Linux 适配器
 *
 * 通过 MimicWX REST API + WebSocket 连接微信。
 * MimicWX API 地址可通过环境变量 MIMICWX_URL 配置，默认 http://localhost:8899
 */
import cfg from "../../lib/config/config.js"
import fetch from "node-fetch"
import WebSocket from "ws"
import fs from "node:fs/promises"
import path from "node:path"

const MIMICWX_URL = process.env.MIMICWX_URL || "http://localhost:8899"
const MIMICWX_TOKEN = process.env.MIMICWX_TOKEN || ""
const MIMICWX_WS = MIMICWX_URL.replace(/^http/, "ws") + "/ws" + (MIMICWX_TOKEN ? `?token=${encodeURIComponent(MIMICWX_TOKEN)}` : "")
const RECONNECT_INTERVAL = 5000

/** 构建带认证的 headers */
function authHeaders(extra = {}) {
  const h = { ...extra }
  if (MIMICWX_TOKEN) h["Authorization"] = `Bearer ${MIMICWX_TOKEN}`
  return h
}

Bot.adapter.push(
  new (class MimicWXAdapter {
    id = "MimicWX"
    name = "MimicWX-Linux"
    ws = null
    self_id = "MimicWX"
    connected = false

    /**
     * 将后端 parsed 结构转为 Yunzai 消息段 + 显示文本
     * parsed: { type: "Text"|"Image"|"Voice"|..., data: {...} }
     */
    parseMsgContent(parsed, rawContent) {
      if (!parsed || !parsed.type) {
        return { segments: [{ type: "text", text: rawContent || "" }], display: rawContent || "" }
      }
      switch (parsed.type) {
        case "Text":
          return { segments: [{ type: "text", text: parsed.data?.text || rawContent }], display: parsed.data?.text || rawContent }
        case "Image":
          return { segments: [{ type: "text", text: "[图片]" }], display: "[图片]" }
        case "Voice": {
          const ms = parsed.data?.duration_ms
          const label = ms >= 1000 ? `[语音 ${Math.floor(ms / 1000)}s]` : ms > 0 ? `[语音 ${ms}ms]` : "[语音]"
          return { segments: [{ type: "text", text: label }], display: label }
        }
        case "Video":
          return { segments: [{ type: "text", text: "[视频]" }], display: "[视频]" }
        case "Emoji":
          return { segments: [{ type: "text", text: "[表情]" }], display: "[表情]" }
        case "App": {
          const t = parsed.data?.title || parsed.data?.desc || "链接"
          const label = `[链接] ${t}`
          return { segments: [{ type: "text", text: label }], display: label }
        }
        case "System":
          return { segments: [{ type: "text", text: parsed.data?.text || rawContent }], display: parsed.data?.text || rawContent }
        default:
          return { segments: [{ type: "text", text: rawContent || "" }], display: rawContent || "" }
      }
    }

    // =========================================================
    // 适配器加载入口
    // =========================================================
    async load() {
      Bot.makeLog("info", `MimicWX 适配器加载中... API: ${MIMICWX_URL}`, "MimicWX")
      this.connectWs()
    }

    // =========================================================
    // WebSocket 连接管理
    // =========================================================
    connectWs() {
      try {
        this.ws = new WebSocket(MIMICWX_WS)
      } catch (err) {
        Bot.makeLog("warn", `WebSocket 创建失败: ${err.message}`, "MimicWX")
        setTimeout(() => this.connectWs(), RECONNECT_INTERVAL)
        return
      }

      this.ws.on("open", async () => {
        Bot.makeLog("mark", `WebSocket 已连接: ${MIMICWX_WS}`, "MimicWX")
        await this.initBot()
      })

      this.ws.on("message", (raw) => {
        try {
          const data = JSON.parse(raw.toString())
          this.onMessage(data)
        } catch (err) {
          Bot.makeLog("error", ["消息解析失败", raw.toString(), err], "MimicWX")
        }
      })

      this.ws.on("close", () => {
        Bot.makeLog("warn", "WebSocket 断开，5秒后重连...", "MimicWX")
        this.connected = false
        setTimeout(() => this.connectWs(), RECONNECT_INTERVAL)
      })

      this.ws.on("error", (err) => {
        Bot.makeLog("error", `WebSocket 错误: ${err.message}`, "MimicWX")
      })
    }

    // =========================================================
    // Bot 初始化
    // =========================================================
    async initBot() {
      let status
      try {
        const res = await fetch(`${MIMICWX_URL}/status`, { headers: authHeaders() })
        status = await res.json()
      } catch {
        Bot.makeLog("warn", "获取状态失败，10秒后重试...", "MimicWX")
        setTimeout(() => this.initBot(), 10000)
        return
      }

      if (status.status !== "已登录") {
        Bot.makeLog("info", `微信状态: ${status.status}，10秒后重试...`, "MimicWX")
        setTimeout(() => this.initBot(), 10000)
        return
      }

      this.self_id = "MimicWX"
      this.connected = true

      // 获取联系人
      let contacts = []
      try {
        const res = await fetch(`${MIMICWX_URL}/contacts`, { headers: authHeaders() })
        const data = await res.json()
        contacts = data.contacts || []
      } catch (err) {
        Bot.makeLog("warn", `获取联系人失败: ${err.message}`, "MimicWX")
      }

      // 构建好友和群列表
      const fl = new Map()
      const gl = new Map()
      for (const c of contacts) {
        const id = c.username || c.wxid || c.display_name
        const info = {
          user_id: id,
          nickname: c.display_name || c.username,
          user_name: c.username,
          remark: c.remark_name || "",
          bot_id: this.self_id,
        }
        if (id && id.includes("@chatroom")) {
          gl.set(id, { group_id: id, group_name: info.nickname, bot_id: this.self_id })
        } else if (id) {
          fl.set(id, info)
        }
      }

      // 注册 Bot
      Bot[this.self_id] = {
        adapter: this,
        fl,
        gl,
        gml: new Map(),
        stat: { start_time: Date.now() / 1000 },

        info: { user_id: this.self_id, user_name: "MimicWX" },
        get uin() { return this.info.user_id },
        get nickname() { return this.info.user_name },

        pickFriend: this.pickFriend.bind(this),
        get pickUser() { return this.pickFriend },
        pickGroup: this.pickGroup.bind(this),
        pickMember: (gid, uid) => this.pickFriend(uid),

        getFriendArray: () => [...fl.values()],
        getFriendList: () => [...fl.keys()],
        getFriendMap: () => fl,

        getGroupArray: () => [...gl.values()],
        getGroupList: () => [...gl.keys()],
        getGroupMap: () => gl,
        getGroupMemberMap: () => new Map(),

        sendApi: () => { throw new Error("MimicWX 不支持 sendApi") },
        version: { id: this.id, name: this.name, version: status.version || "0.4.0" },
      }

      if (!Bot.uin.includes(this.self_id)) Bot.uin.push(this.self_id)

      Bot.makeLog("mark", `${this.name} v${status.version} 已连接 (${fl.size}好友, ${gl.size}群)`, this.self_id)
      Bot.em(`connect.${this.self_id}`, { self_id: this.self_id, bot: Bot[this.self_id] })
    }

    // =========================================================
    // 消息接收处理
    // =========================================================
    onMessage(data) {
      if (!this.connected) return

      // db_message: 数据库新消息
      if (data.type === "db_message") {
        if (data.is_self) return

        const isGroup = data.chat && data.chat.includes("@chatroom")
        const user_id = data.talker || data.chat
        const group_id = isGroup ? data.chat : undefined
        const bot = Bot[this.self_id]

        // 动态更新联系人/群映射 (确保 pickGroup/pickFriend 能找到正确的显示名)
        if (bot) {
          if (isGroup && group_id && data.chat_display) {
            bot.gl.set(group_id, {
              group_id, group_name: data.chat_display, bot_id: this.self_id,
            })
          }
          if (user_id && !user_id.includes("@chatroom")) {
            bot.fl.set(user_id, {
              user_id, nickname: data.talker_display || user_id,
              user_name: user_id, bot_id: this.self_id,
            })
          }
        }

        const { segments, display } = this.parseMsgContent(data.parsed, data.content)

        const e = {
          self_id: this.self_id,
          bot,
          post_type: "message",
          message_type: isGroup ? "group" : "private",
          user_id,
          group_id,
          group_name: isGroup ? data.chat_display : undefined,
          sender: {
            user_id,
            nickname: data.talker_display || data.talker,
            card: data.talker_display,
          },
          message: segments,
          raw_message: display,
          time: data.create_time || Math.floor(Date.now() / 1000),
          message_id: `mimicwx_${data.local_id || Date.now()}`,
        }

        const replyTo = isGroup ? data.chat_display : (data.talker_display || data.talker)
        e.reply = (msg, quote) => this.sendMsgSmart(replyTo, msg)

        if (isGroup) {
          Bot.makeLog("info", `群消息 [${data.chat_display}] ${data.talker_display}: ${display}`,
            `${this.self_id} <= ${group_id}`, true)
        } else {
          Bot.makeLog("info", `私聊消息 ${data.talker_display}: ${display}`,
            `${this.self_id} <= ${user_id}`, true)
        }

        Bot.em(`message.${e.message_type}`, e)
      }

      // listen_message: AT-SPI 监听窗口消息
      if (data.type === "listen_message") {
        const e = {
          self_id: this.self_id,
          bot: Bot[this.self_id],
          post_type: "message",
          message_type: "private",
          user_id: data.sender || data.from,
          sender: {
            user_id: data.sender || data.from,
            nickname: data.sender || data.from,
          },
          message: [{ type: "text", text: data.content }],
          raw_message: data.content,
          time: Math.floor(Date.now() / 1000),
          message_id: `mimicwx_listen_${Date.now()}`,
        }

        e.reply = (msg) => this.sendMsgSmart(data.from, msg)

        Bot.makeLog("info", `监听消息 [${data.from}] ${data.sender}: ${data.content}`,
          this.self_id)

        Bot.em("message.private", e)
      }

      // sent: 发送确认
      if (data.type === "sent") {
        Bot.makeLog("debug", `消息发送确认: ${data.to} verified=${data.verified}`, this.self_id)
      }
    }

    // =========================================================
    // 消息发送 (智能分段: 文本 + 图片)
    // =========================================================

    /**
     * 智能发送: 将 Yunzai 消息段拆分为文本和图片，分别发送
     */
    async sendMsgSmart(to, msg) {
      if (typeof msg === "string") return this.sendText(to, msg)
      if (!Array.isArray(msg)) msg = [msg]

      const textParts = []
      const imageTasks = []

      for (const seg of msg) {
        if (typeof seg === "string") {
          textParts.push(seg)
        } else if (seg.type === "text") {
          textParts.push(seg.text || seg.data?.text || "")
        } else if (seg.type === "at") {
          const uid = seg.qq || seg.data?.qq || ""
          // 从好友列表解析 wxid → 显示名
          const info = Bot[this.self_id]?.fl?.get(uid)
          const name = info?.nickname || info?.user_name || uid
          textParts.push(`@${name}\u2005`)
        } else if (seg.type === "image") {
          // 图片段: 收集后单独发送
          const file = seg.file || seg.url || seg.data?.file || seg.data?.url
          if (file) imageTasks.push(file)
        } else if (seg.type === "record" || seg.type === "video") {
          textParts.push("[不支持的媒体]")
        } else if (seg.type === "face") {
          textParts.push("[表情]")
        } else if (seg.type === "reply" || seg.type === "button") {
          // 跳过不支持的段
        } else if (seg.type === "node") {
          // 合并转发: 递归发送每个节点
          if (Array.isArray(seg.data)) {
            for (const node of seg.data) {
              await this.sendMsgSmart(to, node.message || node.content || node)
            }
          }
        } else {
          textParts.push(Bot.String(seg))
        }
      }

      // 先发文本
      const text = textParts.join("").trim()
      if (text) await this.sendText(to, text)

      // 再发图片 (逐张)
      for (const file of imageTasks) {
        await this.sendImage(to, file)
      }
    }

    /** 发送纯文本 */
    async sendText(to, text) {
      try {
        const res = await fetch(`${MIMICWX_URL}/send`, {
          method: "POST",
          headers: authHeaders({ "Content-Type": "application/json" }),
          body: JSON.stringify({ to, text }),
        })
        const result = await res.json()
        if (!result.sent) {
          Bot.makeLog("warn", `文本发送失败: ${result.message || result.error}`, `${this.self_id} => ${to}`)
        }
        return result
      } catch (err) {
        Bot.makeLog("error", `文本发送错误: ${err.message}`, `${this.self_id} => ${to}`)
      }
    }

    /** 发送图片 (支持 URL / 本地路径 / base64 / Buffer) */
    async sendImage(to, file) {
      try {
        let base64Data, fileName = "image.png"

        // Buffer 对象: 直接转 base64
        if (Buffer.isBuffer(file)) {
          base64Data = file.toString("base64")
        } else if (typeof file !== "string") {
          // 非字符串非 Buffer: 尝试转字符串
          Bot.makeLog("debug", `图片类型: ${typeof file} ${file?.constructor?.name}`, this.self_id)
          if (file?.buffer && Buffer.isBuffer(file.buffer)) {
            base64Data = file.buffer.toString("base64")
          } else {
            file = String(file)
          }
        }

        if (!base64Data && typeof file === "string") {
          if (file.startsWith("base64://")) {
            base64Data = file.slice(9)
          } else if (file.startsWith("http://") || file.startsWith("https://")) {
            Bot.makeLog("info", `下载图片: ${file.slice(0, 80)}...`, this.self_id)
            const res = await fetch(file)
            const buf = Buffer.from(await res.arrayBuffer())
            base64Data = buf.toString("base64")

            const urlPath = new URL(file).pathname
            const ext = path.extname(urlPath)
            if (ext) fileName = `image${ext}`
          } else if (file.startsWith("file://")) {
            const filePath = file.slice(7)
            const buf = await fs.readFile(filePath)
            base64Data = buf.toString("base64")
            fileName = path.basename(filePath)
          } else {
            try {
              const buf = await fs.readFile(file)
              base64Data = buf.toString("base64")
              fileName = path.basename(file)
            } catch {
              Bot.makeLog("warn", `无法读取图片: ${file}`, this.self_id)
              return
            }
          }
        }

        Bot.makeLog("info", `发送图片: ${fileName} (${Math.round(base64Data.length * 3 / 4 / 1024)}KB)`,
          `${this.self_id} => ${to}`, true)

        const res = await fetch(`${MIMICWX_URL}/send_image`, {
          method: "POST",
          headers: authHeaders({ "Content-Type": "application/json" }),
          body: JSON.stringify({ to, file: base64Data, name: fileName }),
        })
        const result = await res.json()
        if (!result.sent) {
          Bot.makeLog("warn", `图片发送失败: ${result.message || result.error}`, `${this.self_id} => ${to}`)
        }
        return result
      } catch (err) {
        Bot.makeLog("error", `图片发送错误: ${err.message}`, `${this.self_id} => ${to}`)
      }
    }

    // =========================================================
    // pickFriend / pickGroup (Yunzai 标准接口)
    // =========================================================
    pickFriend(user_id) {
      const info = Bot[this.self_id]?.fl?.get(user_id) || { user_id, nickname: user_id }
      return {
        ...info,
        user_id,
        sendMsg: (msg) => {
          const to = info.nickname || user_id
          Bot.makeLog("info", `发送好友消息`, `${this.self_id} => ${to}`, true)
          return this.sendMsgSmart(to, msg)
        },
        sendFile: (file, name) => {
          return this.sendImage(info.nickname || user_id, file)
        },
        getInfo: () => info,
        getAvatarUrl: () => null,
      }
    }

    pickGroup(group_id) {
      const info = Bot[this.self_id]?.gl?.get(group_id) || { group_id, group_name: group_id }
      return {
        ...info,
        group_id,
        sendMsg: (msg) => {
          const to = info.group_name || group_id
          Bot.makeLog("info", `发送群消息`, `${this.self_id} => ${to}`, true)
          return this.sendMsgSmart(to, msg)
        },
        sendFile: (file, name) => {
          return this.sendImage(info.group_name || group_id, file)
        },
        getInfo: () => info,
        getAvatarUrl: () => null,
        getMemberArray: () => [],
        getMemberList: () => [],
        getMemberMap: () => new Map(),
        pickMember: (uid) => this.pickFriend(uid),
      }
    }
  })(),
)
