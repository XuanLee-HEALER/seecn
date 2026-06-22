// ntstat-probe: standalone reverse-engineering probe for the private
// `com.apple.network.statistics` (ntstat) kernel control socket on macOS.
//
// Goal of v1: prove the handshake works and dump whatever the kernel sends back,
// so we can calibrate struct offsets against real `claude` flows. No project deps;
// build with: rustc -O tools/ntstat-probe.rs -o /tmp/ntstat-probe
#![allow(non_camel_case_types, dead_code)]

use std::mem;
use std::os::raw::{c_char, c_int, c_ulong, c_void};

// --- PF_SYSTEM / SYSPROTO_CONTROL constants (sys/socket.h, sys/sys_domain.h) ---
const PF_SYSTEM: c_int = 32;
const AF_SYSTEM: u8 = 32;
const SOCK_DGRAM: c_int = 2;
const SYSPROTO_CONTROL: c_int = 2;
const AF_SYS_CONTROL: u16 = 2;

// CTLIOCGINFO = _IOWR('N', 3, struct ctl_info), sizeof(ctl_info)=100 -> 0xC0644E03
const CTLIOCGINFO: c_ulong = 0xC064_4E03;

const SO_RCVTIMEO: c_int = 0x1006;
const SOL_SOCKET: c_int = 0xffff;

#[repr(C)]
struct ctl_info {
    ctl_id: u32,
    ctl_name: [c_char; 96],
}

#[repr(C)]
struct sockaddr_ctl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: u16,
    sc_id: u32,
    sc_unit: u32,
    sc_reserved: [u32; 5],
}

#[repr(C)]
struct timeval {
    tv_sec: i64,
    tv_usec: i32,
}

extern "C" {
    fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, val: *const c_void, len: u32) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn __error() -> *mut c_int;
    fn strerror(e: c_int) -> *const c_char;
}

fn errno() -> c_int {
    unsafe { *__error() }
}
fn errstr() -> String {
    unsafe {
        let p = strerror(errno());
        let mut s = String::new();
        let mut i = 0;
        while *p.add(i) != 0 {
            s.push(*p.add(i) as u8 as char);
            i += 1;
        }
        format!("{} ({})", s, errno())
    }
}

// --- ntstat protocol (bsd/net/ntstat.h, private) ---
const NSTAT_MSG_TYPE_ADD_ALL_SRCS: u32 = 1002;
const NSTAT_MSG_TYPE_QUERY_SRC: u32 = 1004;
const NSTAT_MSG_TYPE_GET_SRC_DESC: u32 = 1005;

const NSTAT_PROVIDER_TCP_USERLAND: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct nstat_msg_hdr {
    context: u64,
    ty: u32,
    length: u16,
    flags: u16,
}

// Newer (events-based) add_all_srcs layout. We set events=0 / target_pid=0 and
// see whether the kernel accepts it; if not we will shrink the struct.
#[repr(C)]
struct nstat_msg_add_all_srcs {
    hdr: nstat_msg_hdr, // 0..16
    provider: u32,      // 16..20
    _pad: u32,          // 20..24
    filter: u64,        // 24..32
    events: u64,        // 32..40
    target_pid: i32,    // 40..44
    target_uuid: [u8; 16], // 44..60
}

fn hexdump(buf: &[u8]) {
    for (i, chunk) in buf.chunks(16).enumerate() {
        let mut hex = String::new();
        let mut asc = String::new();
        for b in chunk {
            hex.push_str(&format!("{:02x} ", b));
            asc.push(if *b >= 0x20 && *b < 0x7f { *b as char } else { '.' });
        }
        println!("    {:04x}  {:<48} {}", i * 16, hex, asc);
    }
}

fn type_label(t: u32) -> &'static str {
    match t {
        1 => "ERROR",
        10001 => "SRC_ADDED",
        10002 => "SRC_REMOVED",
        10003 => "SRC_DESC",
        10004 => "SRC_COUNTS",
        10005 => "SRC_UPDATE",
        _ => "?",
    }
}

// Open a fresh ntstat socket, ADD_ALL_SRCS for one provider, dump responses.
unsafe fn probe(provider: u32, label: &str) {
    println!("\n################ provider={} ({}) ################", provider, label);
    let fd = socket(PF_SYSTEM, SOCK_DGRAM, SYSPROTO_CONTROL);
    if fd < 0 {
        println!("socket() FAILED: {}", errstr());
        return;
    }
    let mut info: ctl_info = mem::zeroed();
    let name = b"com.apple.network.statistics\0";
    for (i, &c) in name.iter().enumerate() {
        info.ctl_name[i] = c as c_char;
    }
    if ioctl(fd, CTLIOCGINFO, &mut info as *mut ctl_info) < 0 {
        println!("ioctl(CTLIOCGINFO) FAILED: {}", errstr());
        close(fd);
        return;
    }
    let mut sc: sockaddr_ctl = mem::zeroed();
    sc.sc_len = mem::size_of::<sockaddr_ctl>() as u8;
    sc.sc_family = AF_SYSTEM;
    sc.ss_sysaddr = AF_SYS_CONTROL;
    sc.sc_id = info.ctl_id;
    if connect(fd, &sc as *const _ as *const c_void, mem::size_of::<sockaddr_ctl>() as u32) < 0 {
        println!("connect() FAILED: {}", errstr());
        close(fd);
        return;
    }
    let tv = timeval { tv_sec: 2, tv_usec: 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv as *const _ as *const c_void, mem::size_of::<timeval>() as u32);

    let mut req: nstat_msg_add_all_srcs = mem::zeroed();
    req.hdr.ty = NSTAT_MSG_TYPE_ADD_ALL_SRCS;
    req.hdr.length = mem::size_of::<nstat_msg_add_all_srcs>() as u16;
    req.hdr.context = 0xAAAA_0001;
    req.provider = provider;
    let n = write(fd, &req as *const _ as *const c_void, mem::size_of::<nstat_msg_add_all_srcs>());
    println!("write(ADD_ALL_SRCS len={}) -> {}", req.hdr.length, n);

    let mut buf = [0u8; 4096];
    let mut count = 0;
    let mut type_hist: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    loop {
        let r = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        if r <= 0 {
            println!("read() -> {} ({}); stop after {} msgs", r, errstr(), count);
            break;
        }
        count += 1;
        let r = r as usize;
        if r >= mem::size_of::<nstat_msg_hdr>() {
            let hdr = &*(buf.as_ptr() as *const nstat_msg_hdr);
            *type_hist.entry(hdr.ty).or_insert(0) += 1;
            if type_hist[&hdr.ty] <= 2 {
                println!(
                    "MSG #{:<3} type={:<6}{:<12} len={:<4} flags={:#06x} ctx={:#x} (read {})",
                    count, hdr.ty, type_label(hdr.ty), hdr.length, hdr.flags, hdr.context, r
                );
                hexdump(&buf[..r.min(176)]);
            }
        }
        if count >= 60 {
            break;
        }
    }
    println!("--- histogram provider={} ---", provider);
    let mut types: Vec<_> = type_hist.into_iter().collect();
    types.sort();
    for (t, c) in types {
        println!("  {:<6} x{:<4} {}", t, c, type_label(t));
    }
    close(fd);
}

fn main() {
    unsafe {
        probe(2, "TCP_KERNEL");
        probe(3, "TCP_USERLAND");
        probe(4, "UDP_KERNEL");
        probe(5, "UDP_USERLAND");
    }
}
