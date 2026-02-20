#!/usr/bin/env python3
"""监控微信 Chats 列表，每秒打印所有项的名称。
用于收集不同消息类型的格式样本。

用法: 在容器内运行此脚本，然后让别人发不同类型的消息:
  - 私聊文本
  - 群聊消息
  - 图片/文件/语音
  - 系统消息
  - 多条未读消息
"""

import gi, time
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi


def find_node(node, target_role, target_name, depth=0, max_depth=20):
    if depth > max_depth:
        return None
    try:
        role = node.get_role_name()
        name = node.get_name() or ""
        if role == target_role and target_name in name:
            return node
        for i in range(min(node.get_child_count(), 20)):
            child = node.get_child_at_index(i)
            if child:
                result = find_node(child, target_role, target_name, depth + 1, max_depth)
                if result:
                    return result
    except:
        pass
    return None


def main():
    print("🔍 正在查找微信 Chats 列表...")

    desktop = Atspi.get_desktop(0)
    chats_node = None

    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if not app:
            continue
        name = (app.get_name() or "").lower()
        if "wechat" not in name:
            continue
        chats_node = find_node(app, "list", "Chats")
        if chats_node:
            break

    if not chats_node:
        print("❌ 未找到 Chats 列表")
        return

    print(f"✅ 找到 Chats 列表\n")
    print("=" * 60)
    print("开始监控，请发送不同类型的消息...")
    print("Ctrl+C 退出")
    print("=" * 60)

    seen = set()
    round_num = 0

    while True:
        round_num += 1
        count = chats_node.get_child_count()
        items = []
        for i in range(count):
            try:
                child = chats_node.get_child_at_index(i)
                if child:
                    name = (child.get_name() or "").strip()
                    if name:
                        items.append(name)
            except:
                pass

        # 打印新出现的格式
        for item in items:
            if item not in seen:
                seen.add(item)
                print(f"\n[#{round_num}] 新格式:")
                print(f"  原始: {repr(item)}")
                print(f"  长度: {len(item)}")
                # 分析结构
                parts = item.rsplit(' ', 1)
                if len(parts) == 2 and ':' in parts[-1]:
                    print(f"  末尾时间: {parts[-1]}")
                if 'unread' in item:
                    print(f"  含未读标记")
                if '[' in item:
                    print(f"  含方括号标记")

        time.sleep(1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\n\n已停止监控")
