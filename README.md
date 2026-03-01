# vmm-demo

A minimal Virtual Machine Monitor (VMM) written in Rust that boots a Linux kernel to an interactive shell using KVM.

## What it does

- Creates a KVM virtual machine with 128 MB of RAM
- Loads a bzImage Linux kernel and initramfs into guest memory
- Sets up 64-bit long mode with identity-mapped page tables
- Emulates a 16550 UART serial console (COM1) for guest I/O
- Boots to a BusyBox shell you can type commands into

## Prerequisites

- Linux with KVM support (`/dev/kvm` must exist)
- Rust (2024 edition)
- A Linux kernel compiled as a bzImage
- BusyBox (statically linked) for the initramfs

### Building a minimal kernel

Clone the Linux source and build a tiny kernel using the provided config fragment:

```bash
cd /path/to/linux
make tinyconfig
scripts/kconfig/merge_config.sh .config /path/to/vmm-demo/vmm-kernel.config
make -j$(nproc) bzImage
```

The resulting kernel is at `arch/x86/boot/bzImage`.

### Building the initramfs

Install busybox-static, then run the included script:

```bash
sudo apt install busybox-static   # Debian/Ubuntu
bash mk-initramfs.sh
```

This creates `initramfs.cpio.gz` in the current directory.

## Build and run

```bash
cargo build --release
cargo run --release -- /path/to/linux/arch/x86/boot/bzImage initramfs.cpio.gz
```

Diagnostic messages go to stderr. To see only guest output:

```bash
cargo run --release -- /path/to/bzImage initramfs.cpio.gz 2>/dev/null
```

## Guest memory layout

| Address | Contents                              |
|---------|---------------------------------------|
| 0x500   | GDT (Global Descriptor Table)         |
| 0x7000  | Boot params (zero page)               |
| 0x9000  | PML4 page table                       |
| 0xA000  | PDPT page table                       |
| 0xB000  | PD page table (512 x 2 MB pages)      |
| 0x20000 | Kernel command line                   |
| ~top    | Initramfs (page-aligned, end of RAM)  |

## How it works

1. Opens `/dev/kvm` and creates a VM with an IRQ chip and PIT timer
2. Loads the bzImage kernel and initramfs into guest memory
3. Builds the boot params (zero page) with setup header, command line pointer, ramdisk location, and e820 memory map
4. Sets up identity-mapped page tables covering the first 1 GB
5. Configures the vCPU for 64-bit long mode and jumps to the kernel's 64-bit entry point
6. Runs the vCPU in a loop, handling serial I/O via port-mapped 16550 UART emulation
7. Injects IRQ 4 (COM1) for both transmit-complete and receive-data-available interrupts
8. Uses a periodic SIGALRM to break out of KVM's in-kernel HLT loop for stdin polling
