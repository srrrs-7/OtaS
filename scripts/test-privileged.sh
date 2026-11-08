#!/bin/sh
# 特権を足したテスト用のコンテナ（compose の dev-privileged）の中で、すべてのテストを動かす。
#
# - root として動き、名前空間・OverlayFS・Btrfs を使うテストも有効にする（OTAS_TEST_PRIVILEGED=1）
# - Btrfs は、/tmp に置いたイメージファイルを loop デバイスでマウントして使う
#   （mount -o loop は自動で切り離される設定になるので、コンテナが終われば何も残らない）
set -eu

if [ "$(id -u)" != 0 ]; then
    echo "test-privileged.sh: root で動かしてください" >&2
    exit 1
fi

IMG=/tmp/otas-btrfs.img
MNT=/mnt/otas-btrfs
cleanup() {
    umount "$MNT" 2>/dev/null || true
    rm -f "$IMG"
}
trap cleanup EXIT

truncate -s 512M "$IMG"
mkfs.btrfs -q "$IMG"
mkdir -p "$MNT"
mount -o loop "$IMG" "$MNT"

export OTAS_TEST_PRIVILEGED=1
export OTAS_TEST_BTRFS_DIR="$MNT"
cargo test --workspace "$@"
