#!/bin/bash
set -e

DIR=${1:-/tmp/ceph}
OBJECTSTORE=${OBJECTSTORE:-memstore}

pkill -9 ceph-mon || true
pkill -9 ceph-osd || true
pkill -9 ceph-mgr || true
rm -rf "${DIR:?}"/*

LOG_DIR="${DIR}/log"
MON_DATA="${DIR}/mon"
OSD_DATA="${DIR}/osd"
MGR_DATA="${DIR}/mgr"
RUN_DIR="${DIR}/run"

mkdir -p "${LOG_DIR}" "${MON_DATA}" "${OSD_DATA}" "${MGR_DATA}" "${RUN_DIR}"

MON_NAME="a"
MGR_NAME="x"
FSID="$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid)"
export CEPH_CONF="${DIR}/ceph.conf"

cat > "${CEPH_CONF}" <<CONF
[global]
fsid = ${FSID}
osd crush chooseleaf type = 0
run dir = ${RUN_DIR}
auth cluster required = none
auth service required = none
auth client required = none
osd pool default size = 1
mon host = 127.0.0.1

[mon.${MON_NAME}]
log file = ${LOG_DIR}/mon.log
mon data = ${MON_DATA}
mon data avail crit = 0
mon addr = 127.0.0.1:6789
mon allow pool delete = true

[osd.0]
log file = ${LOG_DIR}/osd.log
osd data = ${OSD_DATA}
osd journal = ${OSD_DATA}.journal
osd journal size = 100
osd objectstore = ${OBJECTSTORE}
osd class load list = *
osd class default list = *

[mgr.${MGR_NAME}]
log file = ${LOG_DIR}/mgr.log
CONF

echo "Starting Ceph MON..."
ceph-mon --id "${MON_NAME}" --mkfs --keyring /dev/null
touch "${MON_DATA}/keyring"
ceph-mon --id "${MON_NAME}"

echo "Starting Ceph OSD (${OBJECTSTORE})..."
OSD_ID=$(ceph osd create)
ceph osd crush add "osd.${OSD_ID}" 1 root=default
ceph-osd --id "${OSD_ID}" --mkjournal --mkfs
ceph-osd --id "${OSD_ID}"

echo "Starting Ceph MGR..."
ceph-mgr --id "${MGR_NAME}"

ceph auth get-or-create client.admin mon 'allow *' osd 'allow *' mgr 'allow *' -o "${DIR}/keyring"

touch "${DIR}/.ready"
echo "Ceph micro cluster is ready."

if [ "${FOREGROUND:-0}" = "1" ]; then
    tail -f "${LOG_DIR}/osd.log" "${LOG_DIR}/mon.log"
fi
