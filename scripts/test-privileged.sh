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

# cgroups v2：このコンテナの cgroup の中で、子の cgroup に上限を使えるようにする。
# プロセスを持つ cgroup は子に上限を配れないので、今いるプロセスを葉（init）に移してから有効にする。
CG=/sys/fs/cgroup
mkdir -p "$CG/init"
for pid in $(cat "$CG/cgroup.procs"); do
    echo "$pid" > "$CG/init/cgroup.procs" 2>/dev/null || true
done
echo "+memory +pids +cpu" > "$CG/cgroup.subtree_control"
export OTAS_TEST_CGROUP_ROOT="$CG"

export OTAS_TEST_PRIVILEGED=1
export OTAS_TEST_BTRFS_DIR="$MNT"
cargo test --workspace "$@"
