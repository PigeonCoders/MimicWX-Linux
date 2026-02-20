#!/bin/bash
set -e

# D-Bus system
mkdir -p /run/dbus
dbus-daemon --system --fork 2>/dev/null || true

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

su - wechat << 'EOF'
  # VNC
  vncserver :1 -geometry 1280x720 -depth 24 -localhost no 2>/dev/null
  export DISPLAY=:1
  sleep 2

  # D-Bus session
  eval $(dbus-launch --sh-syntax)
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  export QT_ACCESSIBILITY=1

  # AT-SPI2
  /usr/libexec/at-spi2-registryd &
  sleep 1

  # Save D-Bus env for other shells
  echo "export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" > ~/.dbus_env
  echo "export DISPLAY=$DISPLAY" >> ~/.dbus_env
  echo "export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1" >> ~/.dbus_env
  echo "export QT_ACCESSIBILITY=1" >> ~/.dbus_env

  # WeChat
  wechat --no-sandbox --disable-gpu 2>/dev/null &

  # noVNC
  websockify --web /usr/share/novnc 6080 localhost:5901 &

  # MimicWX (Rust binary)
  /usr/local/bin/mimicwx &

  echo "=============================="
  echo "MimicWX-Linux Ready!"
  echo "noVNC: http://localhost:6080/vnc.html"
  echo "API:   http://localhost:8899"
  echo "=============================="
  echo "To connect from docker exec:"
  echo "  source ~/.dbus_env"

  tail -f /dev/null
EOF
