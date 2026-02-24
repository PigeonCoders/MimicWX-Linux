# MimicWX-Linux Docker 环境
# Ubuntu + XFCE桌面 + VNC + WeChat + AT-SPI2 + Rust

FROM ubuntu:22.04 AS base

ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=zh_CN.UTF-8
ENV LANGUAGE=zh_CN:zh
ENV LC_ALL=zh_CN.UTF-8

# ================================================================
# 基础包 + 桌面环境 + VNC
# ================================================================
RUN apt-get update && apt-get install -y \
    locales fonts-wqy-microhei fonts-wqy-zenhei fonts-noto-cjk \
    xfce4 xfce4-terminal dbus-x11 \
    tigervnc-standalone-server tigervnc-common \
    novnc websockify \
    at-spi2-core \
    xdotool xclip xautomation \
    wget curl sudo procps net-tools gpg \
    build-essential pkg-config \
    libdbus-1-dev libatspi2.0-dev libglib2.0-dev \
    gdb python3 \
    && rm -rf /var/lib/apt/lists/*

# 中文 locale
RUN locale-gen zh_CN.UTF-8

# ================================================================
# 安装微信 (官方 .deb 直接下载)
# ================================================================
RUN wget -q -O /tmp/wechat.deb \
    "https://dldir1v6.qq.com/weixin/Universal/Linux/WeChatLinux_x86_64.deb" && \
    apt-get update && dpkg -i /tmp/wechat.deb; \
    apt-get install -f -y && \
    rm -f /tmp/wechat.deb && rm -rf /var/lib/apt/lists/*

# 微信运行时依赖 (Qt/xcb)
RUN apt-get update && apt-get install -y \
    libxkbcommon-x11-0 libxcb-icccm4 libxcb-image0 libxcb-keysyms1 \
    libxcb-render-util0 libxcb-xinerama0 libxcb-shape0 libxcb-cursor0 \
    libxcb-xkb1 libxcb-randr0 libnss3 libatk1.0-0 libatk-bridge2.0-0 \
    libcups2 libdrm2 libgbm1 libasound2 libpango-1.0-0 \
    libcairo2 libatspi2.0-0 libgtk-3-0 \
    && rm -rf /var/lib/apt/lists/*

# ================================================================
# Rust 工具链
# ================================================================
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:$PATH"

# ================================================================
# 编译 MimicWX (利用 Docker cache)
# ================================================================
WORKDIR /build

# 先复制 Cargo 文件利用依赖缓存
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf src target/release/mimicwx target/release/deps/mimicwx-*

# 复制实际源码并编译
COPY src/ src/
RUN cargo build --release

# ================================================================
# 创建用户 + 部署
# ================================================================
RUN useradd -m -s /bin/bash -G sudo wechat && \
    echo "wechat:wechat" | chpasswd && \
    echo "wechat ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# 部署编译产物
RUN cp /build/target/release/mimicwx /usr/local/bin/mimicwx && \
    chmod +x /usr/local/bin/mimicwx

# ================================================================
# VNC 配置
# ================================================================
USER wechat
WORKDIR /home/wechat

RUN mkdir -p ~/.vnc && \
    echo "mimicwx" | vncpasswd -f > ~/.vnc/passwd && \
    chmod 600 ~/.vnc/passwd

RUN printf '#!/bin/bash\nunset SESSION_MANAGER\nunset DBUS_SESSION_BUS_ADDRESS\nexport XKL_XMODMAP_DISABLE=1\nexec startxfce4\n' > ~/.vnc/xstartup && \
    chmod +x ~/.vnc/xstartup

# ================================================================
# 启动脚本
# ================================================================
USER root
# D-Bus 策略: 允许 eavesdrop 用于通知拦截
COPY docker/dbus-mimicwx.conf /etc/dbus-1/session.d/mimicwx.conf
COPY docker/start.sh /usr/local/bin/start.sh
COPY docker/extract_key.py /usr/local/bin/extract_key.py
RUN sed -i 's/\r$//' /usr/local/bin/start.sh /usr/local/bin/extract_key.py && \
    chmod +x /usr/local/bin/start.sh

EXPOSE 5901 6080 8899
CMD ["/usr/local/bin/start.sh"]
