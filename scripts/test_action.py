#!/usr/bin/env python3
"""测试 AT-SPI2 Action 接口：尝试点击聊天列表项打开聊天窗口"""
import gi, time, sys
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

def find_node(node, role, name, depth=0):
    if depth > 20: return None
    try:
        r = node.get_role_name()
        n = node.get_name() or ""
        if r == role and name in n: return node
        for i in range(min(node.get_child_count(), 20)):
            c = node.get_child_at_index(i)
            if c:
                f = find_node(c, role, name, depth+1)
                if f: return f
    except: pass
    return None

desktop = Atspi.get_desktop(0)
app = None
for i in range(desktop.get_child_count()):
    a = desktop.get_child_at_index(i)
    if a and "wechat" == (a.get_name() or "").lower():
        app = a
        break

if not app:
    print("ERROR: wechat app not found")
    sys.exit(1)

# 找到 Chats 列表
chats = find_node(app, "list", "Chats")
if not chats:
    print("ERROR: Chats list not found")
    sys.exit(1)

print(f"Chats list: {chats.get_child_count()} items")

# 取第一个聊天项（NIUNIU）
item = chats.get_child_at_index(0)
item_name = item.get_name() or ""
print(f"\n目标: [{item.get_role_name()}] name='{item_name[:60]}...'")

# 检查 Action 接口
try:
    ai = item.get_action_iface()
    if ai:
        n_actions = ai.get_n_actions()
        print(f"\n可用 Actions ({n_actions} 个):")
        for j in range(n_actions):
            name = ai.get_action_name(j)
            desc = ai.get_action_description(j)
            kb = ai.get_key_binding(j)
            print(f"  [{j}] name='{name}' desc='{desc}' keybinding='{kb}'")
    else:
        print("\n❌ 没有 Action 接口")
except Exception as e:
    print(f"\n❌ Action 接口错误: {e}")

# 检查其他接口
print(f"\n其他接口:")
try:
    si = item.get_selection_iface()
    print(f"  Selection: {'有' if si else '无'}")
except: print("  Selection: 无")

try:
    ci = item.get_component_iface()
    if ci:
        rect = ci.get_extents(Atspi.CoordType.SCREEN)
        print(f"  Component: 有 (位置: x={rect.x}, y={rect.y}, w={rect.width}, h={rect.height})")
    else:
        print(f"  Component: 无")
except Exception as e:
    print(f"  Component: 错误 {e}")

# 检查 Messages 列表 (点击前)
msgs = find_node(app, "list", "Messages")
if msgs:
    print(f"\n点击前 Messages: {msgs.get_child_count()} items")
else:
    print(f"\n点击前 Messages: not found")

# 尝试执行 Action
print("\n--- 尝试执行 Action ---")
try:
    ai = item.get_action_iface()
    if ai and ai.get_n_actions() > 0:
        action_name = ai.get_action_name(0)
        print(f"执行 Action[0]: '{action_name}'...")
        result = ai.do_action(0)
        print(f"结果: {result}")
        
        # 等待 UI 响应
        time.sleep(2)
        
        # 再检查 Messages 列表
        msgs2 = find_node(app, "list", "Messages")
        if msgs2:
            count = msgs2.get_child_count()
            print(f"\n点击后 Messages: {count} items")
            for j in range(min(count, 5)):
                c = msgs2.get_child_at_index(j)
                if c:
                    n = c.get_name() or ""
                    print(f"  [{j}] len={len(n)} | {repr(n[:80])}")
        else:
            print(f"\n点击后 Messages: still not found")
    else:
        print("没有可执行的 Action")
except Exception as e:
    print(f"执行失败: {e}")
