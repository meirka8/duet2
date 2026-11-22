#!/usr/bin/env bash
# Root-only block-device plumbing for the privileged tier of the
# data-safety suite (T-10.2.1, crates/duet-ops/tests/data_safety/privileged.rs).
#
# The tests themselves run unprivileged -- Duet is an unprivileged program
# writing to a filesystem somebody else mounted -- and call this script
# through `sudo -n` for the handful of operations that need root:
#
#   loop-up     <image> <mountpoint> <size-mb>
#       A small ext4 filesystem on a loop device, owned by the caller.
#   loop-down   <mountpoint>
#
#   flakey-up   <name> <image> <mountpoint> <size-mb>
#       The same, but through a device-mapper `flakey` target, so the
#       device can be made to fail.
#   flakey-mode <name> allow|drop|error
#       allow  normal operation
#       drop   every write from now on is silently discarded -- what a
#              power cut does to everything not yet on the platter
#       error  every read and write fails with EIO -- a device that has
#              gone away
#   flakey-remount <name> <mountpoint>
#       Unmount, restore normal operation, mount again (replaying the
#       filesystem journal): the machine coming back up.
#   flakey-down <name> <mountpoint>
#
# <name> must match duet-ds-[a-z0-9-]+ ; nothing outside that namespace is
# ever touched.
set -euo pipefail

die() { echo "data-safety-device: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "must run as root (the suite calls it through 'sudo -n')"

owner_uid="${SUDO_UID:-0}"
owner_gid="${SUDO_GID:-0}"
state_dir=/run/duet-data-safety

check_name() {
    [[ "$1" =~ ^duet-ds-[a-z0-9-]+$ ]] || die "refusing device name '$1'"
}

make_image() {
    local image=$1 size_mb=$2
    [[ "$size_mb" =~ ^[0-9]+$ ]] || die "bad size '$size_mb'"
    truncate -s "${size_mb}M" "$image"
}

allow_table()  { echo "0 $1 flakey $2 0 180 0"; }
drop_table()   { echo "0 $1 flakey $2 0 0 180 1 drop_writes"; }
error_table()  { echo "0 $1 error"; }

load_table() {
    local name=$1 table=$2
    dmsetup suspend --nolockfs "$name"
    dmsetup load "$name" --table "$table"
    dmsetup resume "$name"
}

cmd="${1:-}"; shift || true
case "$cmd" in
loop-up)
    image=$1; mnt=$2; size_mb=$3
    make_image "$image" "$size_mb"
    # -m 0: no blocks reserved for root, so "full" means full for the
    # unprivileged caller at the same point it does for anyone.
    mkfs.ext4 -q -F -m 0 "$image"
    mkdir -p "$mnt"
    mount -o loop "$image" "$mnt"
    chown "$owner_uid:$owner_gid" "$mnt"
    ;;
loop-down)
    mnt=$1
    umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
    ;;
flakey-up)
    name=$1; image=$2; mnt=$3; size_mb=$4
    check_name "$name"
    modprobe dm-flakey 2>/dev/null || true
    dmsetup targets | grep -q '^flakey' \
        || die "the dm-flakey target is not available (try: apt-get install linux-modules-extra-\$(uname -r))"
    make_image "$image" "$size_mb"
    loopdev=$(losetup --find --show "$image")
    sectors=$(blockdev --getsz "$loopdev")
    mkdir -p "$state_dir"
    printf '%s %s\n' "$loopdev" "$sectors" > "$state_dir/$name"
    dmsetup create "$name" --table "$(allow_table "$sectors" "$loopdev")"
    mkfs.ext4 -q -F -m 0 "/dev/mapper/$name"
    mkdir -p "$mnt"
    mount "/dev/mapper/$name" "$mnt"
    chown "$owner_uid:$owner_gid" "$mnt"
    ;;
flakey-mode)
    name=$1; mode=$2
    check_name "$name"
    read -r loopdev sectors < "$state_dir/$name"
    case "$mode" in
    allow) load_table "$name" "$(allow_table "$sectors" "$loopdev")" ;;
    drop)  load_table "$name" "$(drop_table "$sectors" "$loopdev")" ;;
    error) load_table "$name" "$(error_table "$sectors")" ;;
    *) die "unknown mode '$mode'" ;;
    esac
    ;;
flakey-remount)
    name=$1; mnt=$2
    check_name "$name"
    read -r loopdev sectors < "$state_dir/$name"
    umount "$mnt" 2>/dev/null || umount -l "$mnt"
    load_table "$name" "$(allow_table "$sectors" "$loopdev")"
    # A filesystem that saw I/O errors is flagged; let e2fsck replay the
    # journal and clear the flag the way a boot would, then mount.
    e2fsck -p "/dev/mapper/$name" >/dev/null 2>&1 || true
    mount "/dev/mapper/$name" "$mnt"
    ;;
flakey-down)
    name=$1; mnt=$2
    check_name "$name"
    umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
    if [ -f "$state_dir/$name" ]; then
        read -r loopdev _ < "$state_dir/$name"
        dmsetup remove --retry "$name" 2>/dev/null || dmsetup remove --force "$name" 2>/dev/null || true
        losetup -d "$loopdev" 2>/dev/null || true
        rm -f "$state_dir/$name"
    fi
    ;;
*)
    die "usage: $0 loop-up|loop-down|flakey-up|flakey-mode|flakey-remount|flakey-down ..."
    ;;
esac
