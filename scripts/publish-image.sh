#!/usr/bin/env bash
set -Eeuo pipefail

IMAGE="${IMAGE:-ghcr.io/sofyan48/ai48proxy}"
TAG="${1:-latest}"
PLATFORMS="${PLATFORMS:-linux/amd64,linux/arm64}"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || fail 'Docker is required.'
docker buildx version >/dev/null 2>&1 || fail 'Docker Buildx is required.'

printf 'Publishing %s:%s for %s\n' "$IMAGE" "$TAG" "$PLATFORMS"
printf 'Authenticate first with: docker login ghcr.io\n'

docker buildx build \
  --platform "$PLATFORMS" \
  --tag "$IMAGE:$TAG" \
  --push \
  --provenance=true \
  --sbom=true \
  .
