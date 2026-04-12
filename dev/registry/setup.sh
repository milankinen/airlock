#!/usr/bin/env bash
# Generate htpasswd files and push a test image to both local registries.
# Usage: ./setup.sh [username] [password] [image]
#
# Defaults:
#   username: testuser
#   password: testpass / testpass2
#   image:    alpine:3
set -euo pipefail

REGISTRY=localhost:5005
REGISTRY2=localhost:5006
USERNAME=${1:-testuser}
PASSWORD=${2:-testpass}
PASSWORD2="${PASSWORD}2"
IMAGE=${3:-alpine:3}

cd "$(dirname "$0")"

echo "Generating auth/htpasswd for $USERNAME @ $REGISTRY (password: $PASSWORD)..."
docker run --rm httpd:2 htpasswd -Bbn "$USERNAME" "$PASSWORD" > auth/htpasswd
echo "  done"

echo "Generating auth/htpasswd2 for $USERNAME @ $REGISTRY2 (password: $PASSWORD2)..."
docker run --rm httpd:2 htpasswd -Bbn "$USERNAME" "$PASSWORD2" > auth/htpasswd2
echo "  done"

echo "Starting registries..."
docker compose up -d
echo "  done"

docker pull "$IMAGE"

echo "Pushing $IMAGE to $REGISTRY..."
docker tag "$IMAGE" "$REGISTRY/$IMAGE"
docker login "$REGISTRY" -u "$USERNAME" -p "$PASSWORD"
docker push "$REGISTRY/$IMAGE"

echo "Pushing $IMAGE to $REGISTRY2..."
docker tag "$IMAGE" "$REGISTRY2/$IMAGE"
docker login "$REGISTRY2" -u "$USERNAME" -p "$PASSWORD2"
docker push "$REGISTRY2/$IMAGE"

docker rmi "$REGISTRY2/$IMAGE"

echo ""
echo "Registries running:"
echo "  $REGISTRY  — $USERNAME / $PASSWORD"
echo "  $REGISTRY2 — $USERNAME / $PASSWORD2"
echo ""
echo "Use in ez.toml / ez.local.toml:"
echo "  [vm.image]"
echo "  name = \"$REGISTRY/$IMAGE\""
echo "  insecure = true"
