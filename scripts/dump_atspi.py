#!/usr/bin/env python3
"""Targeted dump of WeChat's Chats list from AT-SPI2 tree.
Searches for [list] name='Chats' and dumps its children deeply.
"""

import gi
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi


def find_node(node, target_role=None, target_name=None, max_depth=20, depth=0):
    """BFS-like search for a specific node."""
    if depth > max_depth:
        return None
    try:
        role = node.get_role_name()
        name = node.get_name() or ""
    except:
        return None

    if target_role and target_name:
        if role == target_role and target_name in name:
            return node

    try:
        count = node.get_child_count()
    except:
        return None

    for i in range(min(count, 20)):
        try:
            child = node.get_child_at_index(i)
            if child:
                result = find_node(child, target_role, target_name, max_depth, depth + 1)
                if result:
                    return result
        except:
            pass
    return None


def dump_node(node, depth=0, max_depth=6):
    if depth > max_depth:
        return

    indent = "  " * depth
    try:
        role = node.get_role_name()
        name = node.get_name() or ""
        name_len = len(name)
    except:
        return

    text_content = ""
    try:
        ti = node.get_text_iface()
        if ti:
            cc = ti.get_character_count()
            if 0 < cc < 2000:
                text_content = ti.get_text(0, min(cc, 300))
                if text_content:
                    text_content = repr(text_content)
    except:
        pass

    line = f"{indent}[{role}] (len={name_len})"
    if name:
        line += f" name={repr(name)}"
    if text_content:
        line += f" TEXT={text_content}"

    try:
        child_count = node.get_child_count()
        if child_count > 0:
            line += f" ch={child_count}"
    except:
        child_count = 0

    print(line)

    for i in range(min(child_count, 20)):
        try:
            child = node.get_child_at_index(i)
            if child:
                dump_node(child, depth + 1, max_depth)
        except Exception as e:
            print(f"{indent}  [ERR] {e}")


def main():
    desktop = Atspi.get_desktop(0)
    app_count = desktop.get_child_count()

    for i in range(app_count):
        try:
            app = desktop.get_child_at_index(i)
            if not app:
                continue
            app_name = app.get_name() or ""
            if "wechat" not in app_name.lower():
                continue

            print(f"\n=== Searching in: {app_name} ===")

            # Find Chats list
            chats_node = find_node(app, "list", "Chats")
            if chats_node:
                name = chats_node.get_name()
                count = chats_node.get_child_count()
                print(f"\n📋 Found [list] name='{name}' children={count}")
                print("--- Dumping Chats list (depth=6) ---")
                dump_node(chats_node, depth=0, max_depth=6)
            else:
                print("  ❌ [list] name='Chats' not found")

            # Find Messages list
            msgs_node = find_node(app, "list", "Messages")
            if msgs_node:
                name = msgs_node.get_name()
                count = msgs_node.get_child_count()
                print(f"\n📨 Found [list] name='{name}' children={count}")
                print("--- Dumping Messages list (depth=6) ---")
                dump_node(msgs_node, depth=0, max_depth=6)
            else:
                print("  ℹ️ [list] name='Messages' not found")

            # Find '3 new message(s)' button
            new_msg_btn = find_node(app, "push button", "new message")
            if new_msg_btn:
                name = new_msg_btn.get_name()
                print(f"\n🔔 Found button: '{name}'")

        except Exception as e:
            print(f"Error: {e}")


if __name__ == "__main__":
    main()
