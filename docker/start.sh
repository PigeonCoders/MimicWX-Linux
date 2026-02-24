#!/bin/bash
set -e

# D-Bus system
mkdir -p /run/dbus
dbus-daemon --system --fork 2>/dev/null || true

# 允许 ptrace (GDB 密钥提取需要)
echo 0 > /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || true

# Fix permissions
chmod 666 /dev/uinput 2>/dev/null || echo "WARN: /dev/uinput not available"
chown -R wechat:wechat /home/wechat/.xwechat 2>/dev/null || true
chown -R wechat:wechat /home/wechat/mimicwx-linux 2>/dev/null || true
mkdir -p /home/wechat/.xwechat/crashinfo/attachments
chown -R wechat:wechat /home/wechat/.xwechat

# VNC passwd (tmpfs wipes /tmp, recreate)
su - wechat -c '
  mkdir -p ~/.vnc
  echo "mimicwx" | vncpasswd -f > ~/.vnc/passwd
  chmod 600 ~/.vnc/passwd
'

# 6.5) GDB 密钥提取 — 以 root 后台等待 WeChat PID, 然后 attach
if [ ! -f /tmp/wechat_key.txt ]; then
  (
    # 等待 WeChat PID 文件出现
    for i in $(seq 1 60); do
      [ -f /tmp/wechat.pid ] && break
      sleep 1
    done
    if [ -f /tmp/wechat.pid ]; then
      WECHAT_PID=$(cat /tmp/wechat.pid)
      echo "🔑 启动 GDB 密钥提取 (PID: $WECHAT_PID, 以 root 运行)..."
      gdb -batch -nx -p "$WECHAT_PID" -x /usr/local/bin/extract_key.py \
        > /tmp/gdb_extract.log 2>&1
      echo "🔑 GDB 密钥提取完成"
    else
      echo "🔑 ❌ 未找到 WeChat PID 文件, 跳过密钥提取"
    fi
  ) &
  echo "🔑 GDB 密钥提取监视器已在后台启动"
else
  echo "🔑 密钥文件已存在, 跳过 GDB 提取"
fi

su - wechat << 'EOF'
  # Locale (确保微信用中文)
  export LANG=zh_CN.UTF-8
  export LANGUAGE=zh_CN:zh
  export LC_ALL=zh_CN.UTF-8

  # D-Bus session
  eval $(dbus-launch --sh-syntax)
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  export QT_ACCESSIBILITY=1

  # 1) VNC + XFCE 桌面
  vncserver :1 -geometry 1280x720 -depth 24 -localhost no 2>/dev/null
  export DISPLAY=:1
  sleep 3

  # 2) 彻底清理 XFCE 自动启动的 AT-SPI2 进程
  #    XFCE 创建的和后续创建的 bus 路径不同会导致冲突
  #    多次杀以防 XFCE 桌面组件重新拉起
  for _kill_round in 1 2 3; do
    pkill -9 -f at-spi-bus-launcher 2>/dev/null || true
    pkill -9 -f at-spi2-registryd 2>/dev/null || true
    sleep 0.5
  done
  rm -f ~/.cache/at-spi/bus_1 ~/.cache/at-spi/bus 2>/dev/null || true
  sleep 1

  # 3) 手动启动唯一的 AT-SPI2 bus
  /usr/libexec/at-spi-bus-launcher &
  sleep 2

  # 4) 获取 AT-SPI2 bus 地址
  A11Y_ADDR=$(dbus-send --session --dest=org.a11y.Bus --print-reply \
    /org/a11y/bus org.a11y.Bus.GetAddress 2>/dev/null \
    | grep string | sed 's/.*"\(.*\)"/\1/')
  if [ -n "$A11Y_ADDR" ]; then
    export AT_SPI_BUS_ADDRESS="$A11Y_ADDR"
    echo "AT-SPI2 bus: $A11Y_ADDR"
  else
    echo "WARN: AT-SPI2 bus address not found"
  fi

  # 5) Save D-Bus env
  echo "export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" > ~/.dbus_env
  echo "export DISPLAY=$DISPLAY" >> ~/.dbus_env
  echo "export LANG=zh_CN.UTF-8" >> ~/.dbus_env
  echo "export LANGUAGE=zh_CN:zh" >> ~/.dbus_env
  echo "export LC_ALL=zh_CN.UTF-8" >> ~/.dbus_env
  echo "export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1" >> ~/.dbus_env
  echo "export QT_ACCESSIBILITY=1" >> ~/.dbus_env
  [ -n "$AT_SPI_BUS_ADDRESS" ] && echo "export AT_SPI_BUS_ADDRESS=$AT_SPI_BUS_ADDRESS" >> ~/.dbus_env

  # 6) WeChat (注册到唯一的 AT-SPI2 bus)
  wechat --no-sandbox --disable-gpu 2>/dev/null &
  echo $! > /tmp/wechat.pid
  sleep 12

  # 7) noVNC
  websockify --web /usr/share/novnc 6080 localhost:5901 &

  # 8) MimicWX (连接到同一条 AT-SPI2 bus)
  RUST_LOG=mimicwx=info /usr/local/bin/mimicwx > /tmp/mimicwx.log 2>&1 &

  echo "=============================="
  echo "MimicWX-Linux Ready!"
  echo "noVNC: http://localhost:6080/vnc.html"
  echo "API:   http://localhost:8899"
  echo "=============================="

  tail -f /dev/null
EOF
