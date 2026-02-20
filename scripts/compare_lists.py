#!/usr/bin/env python3
"""对比 Chats 和 Messages 列表中的消息长度"""
import gi
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
for i in range(desktop.get_child_count()):
    app = desktop.get_child_at_index(i)
    if not app or "wechat" not in (app.get_name() or "").lower():
        continue

    chats = find_node(app, "list", "Chats")
    if chats:
        print("=== CHATS LIST ===")
        for j in range(chats.get_child_count()):
            c = chats.get_child_at_index(j)
            if c:
                n = c.get_name() or ""
                print(f"  [{j}] len={len(n)} | {repr(n)}")

    msgs = find_node(app, "list", "Messages")
    if msgs and msgs.get_child_count() > 0:
        print("\n=== MESSAGES LIST ===")
        for j in range(msgs.get_child_count()):
            c = msgs.get_child_at_index(j)
            if c:
                n = c.get_name() or ""
                print(f"  [{j}] len={len(n)} | {repr(n)}")
    else:
        print("\nMESSAGES LIST: empty / not found")
