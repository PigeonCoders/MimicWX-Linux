#!/bin/bash
# Fix Docker image CMD and restart
set -e
docker rm -f tmp-fix 2>/dev/null || true
docker create --name tmp-fix mimicwx-linux-mimicwx:latest /bin/true
docker commit --change 'CMD ["/usr/local/bin/start.sh"]' tmp-fix mimicwx-linux-mimicwx:latest
docker rm tmp-fix
echo "Image CMD fixed"
cd /mnt/d/WeChat/MimicWX-Linux
docker compose up -d
echo "Container started"
