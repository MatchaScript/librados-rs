#!/bin/bash
set -e

CONTAINER_NAME=${CONTAINER_NAME:-ceph-rust-test}
IMAGE=${IMAGE:-quay.io/ceph/ceph:v20}
OBJECTSTORE=${OBJECTSTORE:-memstore}
DIR=${DIR:-/tmp/ceph}
HACK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

podman rm -f "${CONTAINER_NAME}" 2>/dev/null || true
rm -rf "${DIR}"
mkdir -p "${DIR}"

echo "Starting Ceph test container..."
podman run -d --name "${CONTAINER_NAME}" \
    --net=host \
    --privileged \
    -v "${DIR}:${DIR}:z" \
    -v "${HACK_DIR}/micro-osd.sh:/hack/micro-osd.sh:ro" \
    -e FOREGROUND=1 \
    -e "OBJECTSTORE=${OBJECTSTORE}" \
    --entrypoint bash \
    "${IMAGE}" \
    /hack/micro-osd.sh "${DIR}"

echo "Waiting for Ceph micro cluster to become ready..."
for _ in {1..60}; do
    if [ -f "${DIR}/.ready" ]; then
        break
    fi
    sleep 1
done

if [ ! -f "${DIR}/.ready" ]; then
    echo "Timeout waiting for Ceph micro cluster to become ready"
    podman logs "${CONTAINER_NAME}"
    exit 1
fi

podman exec "${CONTAINER_NAME}" ceph -c "${DIR}/ceph.conf" -s
