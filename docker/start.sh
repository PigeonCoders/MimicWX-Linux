#!/bin/bash
# MimicWX-Linux 容器启动脚本
# 启动顺序: D-Bus → VNC → AT-SPI2 → WeChat → GDB密钥提取 → noVNC → MimicWX

# ============================================================
# 0) 系统服务 (root)
# ============================================================
mkdir -p /run/dbus
dbus-daemon --system --fork 2>/dev/null || true

# 允许 ptrace (GDB 密钥提取需要)
echo 0 > /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || true

# 修复权限
chmod 666 /dev/uinput 2>/dev/null || true
chown -R wechat:wechat /home/wechat/.xwechat 2>/dev/null || true
chown -R wechat:wechat /home/wechat/mimicwx-linux 2>/dev/null || true
mkdir -p /home/wechat/.xwechat/crashinfo/attachments
chown -R wechat:wechat /home/wechat/.xwechat

# VNC 密码
su - wechat -c '
  mkdir -p ~/.vnc
  echo "mimicwx" | vncpasswd -f > ~/.vnc/passwd
  chmod 600 ~/.vnc/passwd
'

# ============================================================
# GDB 密钥提取监视器 (root 后台)
# 等待 WeChat PID 文件出现后自动 attach 提取密钥
# ============================================================
if [ ! -f /tmp/wechat_key.txt ]; then
  (
    echo "[GDB] 密钥提取监视器启动, 等待 WeChat PID..."
    for _i in $(seq 1 90); do
      [ -f /tmp/wechat.pid ] && break
      sleep 1
    done
    if [ -f /tmp/wechat.pid ]; then
      WECHAT_PID=$(cat /tmp/wechat.pid)
      echo "[GDB] 检测到 WeChat (PID: $WECHAT_PID), 开始提取密钥..."
      # 等 WeChat 加载 so 库
      sleep 5
      gdb -batch -nx -p "$WECHAT_PID" -x /usr/local/bin/extract_key.py \
        > /tmp/gdb_extract.log 2>&1
      echo "[GDB] 密钥提取完成, 详见 /tmp/gdb_extract.log"
    else
      echo "[GDB] ❌ 超时: 未找到 WeChat PID"
    fi
  ) &
fi

# ============================================================
# 1-8) 用户空间服务 (wechat 用户)
# ============================================================
su - wechat << 'USEREOF'
  export LANG=zh_CN.UTF-8
  export LANGUAGE=zh_CN:zh
  export LC_ALL=zh_CN.UTF-8

  # 1) D-Bus session
  eval $(dbus-launch --sh-syntax)
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  export QT_ACCESSIBILITY=1

  # 2) VNC + XFCE 桌面
  vncserver :1 -geometry 1280x720 -depth 24 -localhost no 2>/dev/null
  export DISPLAY=:1
  sleep 3

  # 3) 清理 XFCE 自启的 AT-SPI2 (避免 bus 冲突)
  for _r in 1 2 3; do
    pkill -9 -f at-spi-bus-launcher 2>/dev/null || true
    pkill -9 -f at-spi2-registryd 2>/dev/null || true
    sleep 0.5
  done
  rm -f ~/.cache/at-spi/bus_1 ~/.cache/at-spi/bus 2>/dev/null || true
  sleep 1

  # 4) 启动唯一的 AT-SPI2 bus
  /usr/libexec/at-spi-bus-launcher &
  sleep 2

  # 5) 获取 AT-SPI2 bus 地址
  A11Y_ADDR=$(dbus-send --session --dest=org.a11y.Bus --print-reply \
    /org/a11y/bus org.a11y.Bus.GetAddress 2>/dev/null \
    | grep string | sed 's/.*"\(.*\)"/\1/')
  if [ -n "$A11Y_ADDR" ]; then
    export AT_SPI_BUS_ADDRESS="$A11Y_ADDR"
    echo "AT-SPI2 bus: $A11Y_ADDR"
  else
    echo "WARN: AT-SPI2 bus address not found"
  fi

  # 保存环境变量 (供 docker exec 使用)
  cat > ~/.dbus_env << ENVEOF
export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS
export DISPLAY=$DISPLAY
export LANG=zh_CN.UTF-8
export LANGUAGE=zh_CN:zh
export LC_ALL=zh_CN.UTF-8
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
export QT_ACCESSIBILITY=1
ENVEOF
  [ -n "$AT_SPI_BUS_ADDRESS" ] && echo "export AT_SPI_BUS_ADDRESS=$AT_SPI_BUS_ADDRESS" >> ~/.dbus_env

  # 6) 启动微信 (写 PID 供 GDB 使用)
  wechat --no-sandbox --disable-gpu 2>/dev/null &
  echo $! > /tmp/wechat.pid
  sleep 12

  # 7) noVNC
  websockify --web /usr/share/novnc 6080 localhost:5901 &

  # 8) MimicWX
  RUST_LOG=mimicwx=info /usr/local/bin/mimicwx > /tmp/mimicwx.log 2>&1 &

  echo "=============================="
  echo "MimicWX-Linux Ready!"
  echo "noVNC: http://localhost:6080/vnc.html"
  echo "API:   http://localhost:8899"
  echo "=============================="
USEREOF

# ============================================================
# 容器保活 (PID 1 直接持有, 不受子进程崩溃影响)
# ============================================================
echo "[start.sh] 容器启动完成, 进入保活循环"
exec tail -f /dev/null
