use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;

use kvm_bindings::{kvm_pit_config, kvm_regs, kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit};
use linux_loader::cmdline::Cmdline;
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::{load_cmdline, KernelLoader};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

const GUEST_MEM_SIZE: u64 = 128 << 20; // 128 MB

// Guest physical address layout
const GDT_ADDR: u64 = 0x500;
const BOOT_PARAMS_ADDR: u64 = 0x7000;
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xA000;
const PD_ADDR: u64 = 0xB000;
const CMDLINE_ADDR: u64 = 0x20000;

const CMDLINE_STR: &str = "console=ttyS0 reboot=t panic=1 quiet";

// ---------------------------------------------------------------------------
// Terminal: puts stdin into raw mode so we get keypresses immediately
// ---------------------------------------------------------------------------
struct Terminal {
    fd: i32,
    orig: libc::termios,
}

impl Terminal {
    fn setup() -> Self {
        let fd = io::stdin().as_raw_fd();
        let mut orig = unsafe { std::mem::zeroed::<libc::termios>() };
        unsafe { libc::tcgetattr(fd, &mut orig) };

        let mut raw = orig;
        // Disable canonical mode and echo; keep ISIG so Ctrl+C still works
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_iflag &= !(libc::ICRNL); // don't translate CR→NL
        raw.c_cc[libc::VMIN] = 0; // non-blocking reads
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };

        // Set stdin to non-blocking
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        Terminal { fd, orig }
    }

    fn read_available(&self, buf: &mut [u8]) -> usize {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n > 0 {
            n as usize
        } else {
            0
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
    }
}

// ---------------------------------------------------------------------------
// Serial: minimal 16550 UART emulation on ports 0x3F8–0x3FF (COM1)
// ---------------------------------------------------------------------------
struct Serial {
    input: VecDeque<u8>,
    ier: u8,            // Interrupt Enable Register
    lcr: u8,            // Line Control Register
    mcr: u8,            // Modem Control Register
    scr: u8,            // Scratch Register
    dll: u8,            // Divisor Latch Low  (accessible when DLAB=1)
    dlm: u8,            // Divisor Latch High (accessible when DLAB=1)
    thre_pending: bool,  // Transmitter Holding Register Empty interrupt pending
}

impl Serial {
    fn new() -> Self {
        Serial {
            input: VecDeque::new(),
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            thre_pending: false,
        }
    }

    fn handle_read(&mut self, port: u16) -> u8 {
        let dlab = self.lcr & 0x80 != 0;
        match port - 0x3F8 {
            0 if dlab => self.dll,
            0 => self.input.pop_front().unwrap_or(0), // RBR: read data
            1 if dlab => self.dlm,
            1 => self.ier,
            2 => {
                // IIR: report what interrupt is pending (highest priority first)
                // Bit 0: 0 = interrupt pending, 1 = no interrupt
                // Bits 3:1: interrupt type (when bit 0 = 0)
                if !self.input.is_empty() && self.ier & 0x01 != 0 {
                    0x04 // Received Data Available (priority 2)
                } else if self.thre_pending && self.ier & 0x02 != 0 {
                    self.thre_pending = false; // reading IIR clears THRE interrupt
                    0x02 // Transmitter Holding Register Empty (priority 3)
                } else {
                    0x01 // No interrupt pending
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                // LSR: transmitter always ready; set Data Ready if we have input
                let mut lsr = 0x60; // THRE | TEMT
                if !self.input.is_empty() {
                    lsr |= 0x01; // DR
                }
                lsr
            }
            6 => 0xB0,  // MSR: CTS + DSR + DCD
            7 => self.scr,
            _ => 0,
        }
    }

    /// Returns true if IRQ 4 should be injected after this write.
    fn handle_write(&mut self, port: u16, value: u8) -> bool {
        let dlab = self.lcr & 0x80 != 0;
        match port - 0x3F8 {
            0 if dlab => { self.dll = value; false }
            0 => {
                // THR: guest is writing a character → print it
                let _ = io::stdout().write_all(&[value]);
                let _ = io::stdout().flush();
                // Transmission is instant; signal THRE so driver sends the next char
                self.thre_pending = true;
                self.ier & 0x02 != 0
            }
            1 if dlab => { self.dlm = value; false }
            1 => {
                self.ier = value;
                // If THRE interrupt just got enabled while transmitter is idle
                // (which it always is), fire immediately
                if value & 0x02 != 0 {
                    self.thre_pending = true;
                    true
                } else {
                    false
                }
            }
            2 => false, // FCR: ignore
            3 => { self.lcr = value; false }
            4 => { self.mcr = value; false }
            7 => { self.scr = value; false }
            _ => false,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let kernel_path = args.next().expect("Usage: vmm-demo <bzImage> <initramfs>");
    let initrd_path = args.next().expect("Usage: vmm-demo <bzImage> <initramfs>");

    // 1. Open /dev/kvm and create a VM
    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;

    // 2. Set up guest memory
    let guest_mem =
        GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), GUEST_MEM_SIZE as usize)])?;

    let host_addr = guest_mem.get_host_address(GuestAddress(0))?;
    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: GUEST_MEM_SIZE,
        userspace_addr: host_addr as u64,
        flags: 0,
    };
    unsafe { vm.set_user_memory_region(mem_region)? };
    eprintln!("Guest memory: {} MB", GUEST_MEM_SIZE >> 20);

    // 3. Create IRQ chip and PIT (must be before vCPU creation)
    vm.create_irq_chip()?;
    vm.create_pit2(kvm_pit_config::default())?;

    // 4. Load the bzImage kernel into guest memory
    let mut kernel_file = File::open(&kernel_path)?;
    let kernel_result = BzImage::load(&guest_mem, None, &mut kernel_file, None)?;
    eprintln!(
        "Kernel loaded at: {:#x}, size: {} bytes",
        kernel_result.kernel_load.raw_value(),
        kernel_result.kernel_end
    );

    // 5. Load kernel command line
    let mut cmdline = Cmdline::new(256)?;
    cmdline.insert_str(CMDLINE_STR)?;
    load_cmdline(&guest_mem, GuestAddress(CMDLINE_ADDR), &cmdline)?;

    // 6. Load initramfs
    let initrd_data = fs::read(&initrd_path)?;
    let initrd_addr = GuestAddress((GUEST_MEM_SIZE - initrd_data.len() as u64) & !0xFFF);
    guest_mem.write_slice(&initrd_data, initrd_addr)?;
    eprintln!(
        "Initramfs at: {:#x} ({} bytes)",
        initrd_addr.raw_value(),
        initrd_data.len()
    );

    // 7. Build boot params (the "zero page")
    let mut boot_params = [0u8; 4096];

    kernel_file.seek(SeekFrom::Start(0x1F1))?;
    kernel_file.read(&mut boot_params[0x1F1..0x280])?;

    boot_params[0x210] = 0xFF; // type_of_loader
    boot_params[0x211] |= 0x01 | 0x40; // LOADED_HIGH | KEEP_SEGMENTS

    boot_params[0x218..0x21C]
        .copy_from_slice(&(initrd_addr.raw_value() as u32).to_le_bytes()); // ramdisk_image
    boot_params[0x21C..0x220]
        .copy_from_slice(&(initrd_data.len() as u32).to_le_bytes()); // ramdisk_size
    boot_params[0x228..0x22C]
        .copy_from_slice(&(CMDLINE_ADDR as u32).to_le_bytes()); // cmd_line_ptr
    boot_params[0x238..0x23C]
        .copy_from_slice(&(CMDLINE_STR.len() as u32).to_le_bytes()); // cmdline_size

    // e820 memory map: one entry covering all guest RAM
    boot_params[0x1E8] = 1;
    boot_params[0x2D0..0x2D8].copy_from_slice(&0u64.to_le_bytes());
    boot_params[0x2D8..0x2E0].copy_from_slice(&GUEST_MEM_SIZE.to_le_bytes());
    boot_params[0x2E0..0x2E4].copy_from_slice(&1u32.to_le_bytes()); // E820_RAM

    guest_mem.write_slice(&boot_params, GuestAddress(BOOT_PARAMS_ADDR))?;

    // 8. Page tables (identity-mapped first 1 GB with 2 MB pages)
    guest_mem.write_obj(PDPT_ADDR | 0x3u64, GuestAddress(PML4_ADDR))?;
    guest_mem.write_obj(PD_ADDR | 0x3u64, GuestAddress(PDPT_ADDR))?;
    for i in 0u64..512 {
        let entry: u64 = (i << 21) | 0x83; // present + writable + page_size
        guest_mem.write_obj(entry, GuestAddress(PD_ADDR + i * 8))?;
    }

    // 9. GDT
    let gdt: [u64; 4] = [
        0,
        0x00AF_9A00_0000_FFFF, // 0x08: 64-bit code
        0x00CF_9200_0000_FFFF, // 0x10: 64-bit data
        0,
    ];
    for (i, &entry) in gdt.iter().enumerate() {
        guest_mem.write_obj(entry, GuestAddress(GDT_ADDR + (i as u64) * 8))?;
    }

    // 10. Create vCPU and configure registers
    eprintln!("Creating vCPU...");
    let mut vcpu = vm.create_vcpu(0)?;

    // Expose host CPU features to the guest (without this, CPUID returns
    // almost nothing and the kernel panics during early feature detection)
    eprintln!("Setting up CPUID...");
    let cpuid = kvm.get_supported_cpuid(80)?;
    eprintln!("Got {} CPUID entries, applying to vCPU...", cpuid.as_slice().len());
    vcpu.set_cpuid2(&cpuid)?;

    eprintln!("Configuring vCPU registers...");
    let mut sregs = vcpu.get_sregs()?;
    sregs.cr0 = 0x8005_0033;
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 = 0x20;
    sregs.efer = 0xD00;

    sregs.cs = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x08,
        type_: 11,
        present: 1,
        dpl: 0,
        db: 0,
        s: 1,
        l: 1,
        g: 1,
        ..Default::default()
    };

    let data_seg = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x10,
        type_: 3,
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        ..Default::default()
    };
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;

    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = 31;
    vcpu.set_sregs(&sregs)?;

    let regs = kvm_regs {
        rip: kernel_result.kernel_load.raw_value() + 0x200,
        rsi: BOOT_PARAMS_ADDR,
        rflags: 0x2,
        ..Default::default()
    };
    vcpu.set_regs(&regs)?;

    eprintln!("Booting kernel (entry {:#x})...\n", regs.rip);

    // 11. Set up a periodic signal to kick vcpu.run() out of in-kernel HLT.
    //     Without this, the vCPU blocks in KVM forever when the guest is idle
    //     and we never get a chance to poll stdin.
    extern "C" fn noop_handler(_: libc::c_int) {}
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = noop_handler as *const () as usize;
        sa.sa_flags = 0; // no SA_RESTART — we want vcpu.run() to be interrupted
        libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());

        let timer = libc::itimerval {
            it_interval: libc::timeval { tv_sec: 0, tv_usec: 10_000 }, // every 10ms
            it_value: libc::timeval { tv_sec: 0, tv_usec: 10_000 },
        };
        libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut());
    }

    // 12. Set up terminal and serial, then run the guest
    let terminal = Terminal::setup();
    let mut serial = Serial::new();

    loop {
        // Poll stdin for input before each vCPU run
        let mut buf = [0u8; 64];
        let n = terminal.read_available(&mut buf);
        if n > 0 {
            for &b in &buf[..n] {
                serial.input.push_back(b);
            }
            // Inject IRQ 4 (COM1) if the guest enabled receive interrupts
            if serial.ier & 0x01 != 0 {
                vm.set_irq_line(4, true)?;
                vm.set_irq_line(4, false)?;
            }
        }

        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => {
                if (0x3F8..=0x3FF).contains(&port) {
                    if serial.handle_write(port, data[0]) {
                        vm.set_irq_line(4, true)?;
                        vm.set_irq_line(4, false)?;
                    }
                }
            }
            Ok(VcpuExit::IoIn(port, data)) => {
                if (0x3F8..=0x3FF).contains(&port) {
                    data[0] = serial.handle_read(port);
                } else {
                    data.fill(0);
                }
            }
            Ok(VcpuExit::Hlt) => {}
            Ok(VcpuExit::Shutdown) => {
                eprintln!("\nGuest shutdown.");
                break;
            }
            Ok(_) => {}
            // SIGALRM interrupted vcpu.run() — just loop back and check stdin
            Err(e) if e.errno() == libc::EINTR => continue,
            Err(e) => return Err(e.into()),
        }
    }

    drop(terminal); // restore terminal settings
    Ok(())
}
