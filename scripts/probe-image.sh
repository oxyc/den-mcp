#!/usr/bin/env bash
# Start an image and prove it reports the release it was built for.
#
#   scripts/probe-image.sh ghcr.io/oxyc/den-mcp@sha256:… 0.9.1
#
# den-mcp has no /version route, so the version is read from /metrics (mcp_build_info), which the probe unlocks with
# a throwaway token. ATLAS_URL points nowhere: the process starts and answers /health without atlas.
set -euo pipefail

image="${1:?usage: probe-image.sh <image> <version>}"
version="${2:?usage: probe-image.sh <image> <version>}"

docker pull --quiet "$image" >/dev/null
id=$(docker run --rm -d -p 127.0.0.1:18096:8096 \
    -e ATLAS_URL=http://127.0.0.1:9 -e PUBLIC_ORIGIN=https://probe.invalid -e METRICS_TOKEN=probe "$image")
trap 'docker rm -f "$id" >/dev/null 2>&1 || true' EXIT

metrics=""
for _ in $(seq 1 20); do
    metrics=$(curl -fsS -H 'Authorization: Bearer probe' http://127.0.0.1:18096/metrics 2>/dev/null) && break
    sleep 1
done
grep -qF "mcp_build_info{version=\"$version\"} 1" <<<"$metrics" || {
    echo "::error::image does not report version $version: $(grep -F mcp_build_info <<<"$metrics" || echo 'no /metrics answer')"
    docker logs "$id" >&2 || true
    exit 1
}
echo "image reports $version"
