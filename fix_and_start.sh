#!/bin/bash
set -e

echo "1/4: Stopping container..."
docker compose -f /mnt/d/WeChat/MimicWX-Linux/docker-compose.yml down 2>/dev/null || true

echo "2/4: Patching image with new start.sh..."
docker rm -f tmp-patch 2>/dev/null || true
docker run -d --name tmp-patch mimicwx-linux-mimicwx:latest sleep 60
docker cp /mnt/d/WeChat/MimicWX-Linux/docker/start.sh tmp-patch:/usr/local/bin/start.sh
docker exec tmp-patch sed -i 's/\r$//' /usr/local/bin/start.sh
docker exec tmp-patch chmod +x /usr/local/bin/start.sh

echo "3/4: Committing patched image..."
docker stop tmp-patch
docker commit --change 'CMD ["/usr/local/bin/start.sh"]' tmp-patch mimicwx-linux-mimicwx:latest
docker rm tmp-patch

echo "4/4: Starting container..."
docker compose -f /mnt/d/WeChat/MimicWX-Linux/docker-compose.yml up -d

echo "ALL DONE"
