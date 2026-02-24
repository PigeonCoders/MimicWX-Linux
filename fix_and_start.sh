#!/bin/bash
# Fix Docker image CMD and restart container
set -e

# Clean up temp containers
docker rm -f mimicwx-fix-cmd 2>/dev/null || true

# Create temp container from current image
docker create --name mimicwx-fix-cmd mimicwx-linux-mimicwx:latest /bin/true

# Commit with correct CMD
docker commit \
  --change 'CMD ["/usr/local/bin/start.sh"]' \
  mimicwx-fix-cmd \
  mimicwx-linux-mimicwx:latest

# Clean up
docker rm mimicwx-fix-cmd

echo "✅ Image CMD fixed"

# Start with docker compose
cd /mnt/d/WeChat/MimicWX-Linux
docker compose up -d

echo "✅ Container started"
