#!/bin/bash
set -e

INITRAMFS_DIR=$(mktemp -d)
trap "rm -rf $INITRAMFS_DIR" EXIT

# Create directory structure
mkdir -p "$INITRAMFS_DIR"/{bin,sbin,etc,proc,sys,dev}

# Copy busybox (statically linked)
cp /bin/busybox "$INITRAMFS_DIR/bin/busybox"

# Create symlinks for common commands
for cmd in sh bash ls cat echo mount mkdir mknod ps grep dmesg uname whoami hostname setsid cttyhack; do
    ln -s busybox "$INITRAMFS_DIR/bin/$cmd"
done

# Create the init script (PID 1)
cat > "$INITRAMFS_DIR/init" << 'INIT'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

echo ""
echo "==========================="
echo "  Welcome to vmm-demo!"
echo "==========================="
echo ""

exec setsid cttyhack /bin/sh
INIT
chmod +x "$INITRAMFS_DIR/init"

# Pack into a cpio archive (gzipped)
(cd "$INITRAMFS_DIR" && find . | cpio -o -H newc --quiet | gzip) > initramfs.cpio.gz

echo "Created initramfs.cpio.gz ($(du -h initramfs.cpio.gz | cut -f1))"
