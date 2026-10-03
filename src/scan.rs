// Discovers local HTTP(S) servers: TCP listeners owned by console processes of the
// current user session, confirmed by a tiny HTTP/TLS probe. Also stops them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{BOOL, PWSTR};

pub const WM_SNAPSHOT: u32 = 0x8000 + 2; // WM_APP + 2

#[derive(Clone, Copy, PartialEq)]
pub struct Port {
    pub port: u16,
    pub tls: bool,
}

#[derive(Clone)]
pub struct Server {
    pub pid: u32,
    pub created: u64,
    pub ports: Vec<Port>,
    pub name: String,
    pub detail: String,
    pub mem: u64,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub servers: Vec<Server>,
    pub mem_total: u64,
    pub mem_avail: u64,
}

pub enum Cmd {
    Visible(bool),
    RefreshSoon,
}

/// Starts the background scanner; snapshots are posted to `hwnd` as boxed pointers in LPARAM.
pub fn spawn(hwnd: isize) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let post = |s: Snapshot| unsafe {
            let p = Box::into_raw(Box::new(s));
            let h = HWND(hwnd as *mut c_void);
            if PostMessageW(Some(h), WM_SNAPSHOT, WPARAM(0), LPARAM(p as isize)).is_err() {
                drop(Box::from_raw(p));
            }
        };
        let mut sc = Scanner::new();
        let mut visible = false;
        sc.scan(&post);
        loop {
            let wait = if visible { 1500 } else { 30_000 };
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(Cmd::Visible(v)) => {
                    visible = v;
                    if !v {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Cmd::RefreshSoon) => std::thread::sleep(Duration::from_millis(250)),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            sc.scan(&post);
        }
    });
    tx
}

#[derive(Clone, Copy, PartialEq)]
enum Probe {
    Http,
    Https,
    No,
}

struct ProcInfo {
    created: u64,
    eligible: bool,
    name: String,
    detail: String,
}

struct Scanner {
    own_pid: u32,
    own_session: u32,
    probes: HashMap<(u32, u16), (Probe, Instant)>,
    procs: HashMap<u32, ProcInfo>,
    console_exe: HashMap<String, bool>,
}

struct Listener {
    pid: u32,
    port: u16,
    addr: SocketAddr,
}

impl Scanner {
    fn new() -> Self {
        let own_pid = std::process::id();
        let mut own_session = 0;
        unsafe {
            let _ = ProcessIdToSessionId(own_pid, &mut own_session);
        }
        Scanner { own_pid, own_session, probes: HashMap::new(), procs: HashMap::new(), console_exe: HashMap::new() }
    }

    fn scan(&mut self, post: &dyn Fn(Snapshot)) {
        let listeners = tcp_listeners();
        let table = process_table();
        let pids: HashSet<u32> = listeners.iter().map(|l| l.pid).collect();

        self.procs.retain(|pid, _| pids.contains(pid));
        for &pid in &pids {
            self.refresh_proc(pid, &table);
        }
        let cands: Vec<&Listener> = listeners
            .iter()
            .filter(|l| self.procs.get(&l.pid).is_some_and(|p| p.eligible))
            .collect();
        let live: HashSet<(u32, u16)> = cands.iter().map(|l| (l.pid, l.port)).collect();
        self.probes.retain(|k, _| live.contains(k));

        let todo: Vec<(u32, u16, SocketAddr)> = cands
            .iter()
            .filter(|l| match self.probes.get(&(l.pid, l.port)) {
                None => true,
                Some((Probe::No, t)) => t.elapsed() > Duration::from_secs(30),
                _ => false,
            })
            .map(|l| (l.pid, l.port, l.addr))
            .collect();

        post(self.build(&cands, &table));
        if todo.is_empty() {
            return;
        }
        let results: Vec<((u32, u16), Probe)> = std::thread::scope(|s| {
            let handles: Vec<_> = todo
                .iter()
                .map(|&(pid, port, addr)| s.spawn(move || ((pid, port), probe(addr))))
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        let now = Instant::now();
        let mut changed = false;
        for (k, p) in results {
            changed |= p != Probe::No;
            self.probes.insert(k, (p, now));
        }
        if changed {
            post(self.build(&cands, &table));
        }
    }

    fn refresh_proc(&mut self, pid: u32, table: &ProcTable) {
        let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
            .or_else(|| open(pid, PROCESS_QUERY_LIMITED_INFORMATION))
        else {
            self.procs.insert(pid, ProcInfo { created: 0, eligible: false, name: String::new(), detail: String::new() });
            return;
        };
        let created = create_time(&h);
        if self.procs.get(&pid).is_some_and(|p| p.created == created) {
            return;
        }
        let exe = table.exe.get(&pid).cloned().unwrap_or_default();
        let mut eligible = pid > 4 && pid != self.own_pid && !BLOCKLIST.contains(&exe.to_lowercase().as_str());
        if eligible {
            let mut session = u32::MAX;
            unsafe {
                let _ = ProcessIdToSessionId(pid, &mut session);
            }
            eligible = session == self.own_session;
        }
        if eligible {
            eligible = match image_path(&h) {
                Some(path) if !installed_app(&path) => {
                    *self.console_exe.entry(path.clone()).or_insert_with(|| is_console_exe(&path))
                }
                _ => false,
            };
        }
        let (name, detail) = if eligible {
            derive_name(&exe, command_line(&h).as_deref(), current_dir(&h).as_deref())
        } else {
            (String::new(), String::new())
        };
        self.procs.insert(pid, ProcInfo { created, eligible, name, detail });
    }

    fn build(&self, cands: &[&Listener], table: &ProcTable) -> Snapshot {
        let mut by_pid: BTreeMap<u32, Vec<Port>> = BTreeMap::new();
        for l in cands {
            let tls = match self.probes.get(&(l.pid, l.port)) {
                Some((Probe::Http, _)) => false,
                Some((Probe::Https, _)) => true,
                _ => continue,
            };
            let ports = by_pid.entry(l.pid).or_default();
            if !ports.iter().any(|p| p.port == l.port) {
                ports.push(Port { port: l.port, tls });
            }
        }

        let mut times = HashMap::new();
        let trees: HashMap<u32, Vec<u32>> = by_pid.keys().map(|&pid| (pid, table.tree(pid, &mut times))).collect();

        // A server spawned by another listed server is folded into its ancestor.
        let mut absorbed: HashSet<u32> = HashSet::new();
        for tree in trees.values() {
            absorbed.extend(tree.iter().skip(1).filter(|p| by_pid.contains_key(p)));
        }
        let mut servers = Vec::new();
        for (&pid, ports) in &by_pid {
            if absorbed.contains(&pid) {
                continue;
            }
            let mut ports = ports.clone();
            for &b in trees[&pid].iter().skip(1) {
                if let Some(extra) = by_pid.get(&b) {
                    ports.extend(extra.iter().copied().filter(|p| !ports.iter().any(|q| q.port == p.port)).collect::<Vec<_>>());
                }
            }
            ports.sort_by_key(|p| p.port);
            let info = &self.procs[&pid];
            let mem = trees[&pid].iter().map(|&p| working_set(p)).sum();
            servers.push(Server { pid, created: info.created, ports, name: info.name.clone(), detail: info.detail.clone(), mem });
        }
        servers.sort_by_key(|s| s.ports[0].port);

        let mut ms = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        unsafe {
            let _ = GlobalMemoryStatusEx(&mut ms);
        }
        Snapshot { servers, mem_total: ms.ullTotalPhys, mem_avail: ms.ullAvailPhys }
    }
}

/// Processes whose listeners must never be offered for killing.
const BLOCKLIST: &[&str] = &[
    "system", "svchost.exe", "lsass.exe", "services.exe", "wininit.exe", "spoolsv.exe",
    "com.docker.backend.exe", "com.docker.proxy.exe", "docker.exe", "dockerd.exe", "vpnkit.exe",
    "wslrelay.exe", "wslhost.exe", "wsl.exe", "wslservice.exe",
];

// ---------------------------------------------------------------- stopping

/// Terminates the server process and all its descendants. Returns false if the root
/// process could not be terminated (typically: it runs elevated).
pub fn kill(pid: u32, created: u64) -> bool {
    let Some(root) = open(pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION) else {
        return open(pid, PROCESS_QUERY_LIMITED_INFORMATION).is_none_or(|h| create_time(&h) != created);
    };
    if create_time(&root) != created {
        return true; // already gone, pid reused
    }
    let table = process_table();
    let tree = table.tree(pid, &mut HashMap::new());
    let ok = unsafe { TerminateProcess(root.0, 1).is_ok() };
    for &child in tree.iter().skip(1) {
        if let Some(h) = open(child, PROCESS_TERMINATE) {
            unsafe {
                let _ = TerminateProcess(h.0, 1);
            }
        }
    }
    ok
}

// ---------------------------------------------------------------- probing

fn probe(addr: SocketAddr) -> Probe {
    // HTTP/1.1 without Host: most servers answer 400 immediately, without routing
    // the request into the app (no page compilation, no side effects).
    if let Some(head) = exchange(addr, b"HEAD / HTTP/1.1\r\nConnection: close\r\n\r\n") {
        if head.starts_with(b"HTTP/") {
            return Probe::Http;
        }
    }
    match exchange(addr, &client_hello()) {
        Some(h) if h.len() >= 2 && (h[0] == 0x16 || h[0] == 0x15) && h[1] == 0x03 => Probe::Https,
        _ => Probe::No,
    }
}

fn exchange(addr: SocketAddr, req: &[u8]) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(900))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(300))).ok()?;
    s.write_all(req).ok()?;
    let mut buf = [0u8; 8];
    let mut n = 0;
    while n < 5 {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    (n > 0).then(|| buf[..n].to_vec())
}

/// Minimal TLS 1.2 ClientHello; any TLS server answers with a handshake or an alert record.
fn client_hello() -> Vec<u8> {
    let suites: &[u16] = &[0x1301, 0x1302, 0x1303, 0xc02f, 0xc030, 0xc02b, 0xc02c, 0x009c, 0x009d, 0x002f, 0x0035];
    let mut ext = Vec::new();
    ext.extend([0x00, 0x0a, 0x00, 0x06, 0x00, 0x04, 0x00, 0x1d, 0x00, 0x17]); // supported_groups
    ext.extend([0x00, 0x0b, 0x00, 0x02, 0x01, 0x00]); // ec_point_formats
    ext.extend([0x00, 0x0d, 0x00, 0x08, 0x00, 0x06, 0x04, 0x01, 0x04, 0x03, 0x08, 0x04]); // signature_algorithms
    let mut body = vec![0x03, 0x03];
    body.extend((0..32u8).map(|i| i.wrapping_mul(37).wrapping_add(11)));
    body.push(0);
    body.extend(((suites.len() * 2) as u16).to_be_bytes());
    for s in suites {
        body.extend(s.to_be_bytes());
    }
    body.extend([1, 0]);
    body.extend((ext.len() as u16).to_be_bytes());
    body.extend(ext);
    let mut hs = vec![0x01];
    hs.extend(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend(body);
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend((hs.len() as u16).to_be_bytes());
    rec.extend(hs);
    rec
}

// ---------------------------------------------------------------- tcp table

fn tcp_listeners() -> Vec<Listener> {
    let mut out: Vec<Listener> = Vec::new();
    for af in [AF_INET.0 as u32, AF_INET6.0 as u32] {
        let mut size = 0u32;
        let mut buf: Vec<u64> = Vec::new();
        let mut ok = false;
        for _ in 0..4 {
            buf.resize(size as usize / 8 + 64, 0);
            size = (buf.len() * 8) as u32;
            let r = unsafe {
                GetExtendedTcpTable(Some(buf.as_mut_ptr() as *mut c_void), &mut size, false, af, TCP_TABLE_OWNER_PID_LISTENER, 0)
            };
            if r == NO_ERROR.0 {
                ok = true;
                break;
            }
            if r != ERROR_INSUFFICIENT_BUFFER.0 {
                break;
            }
        }
        if !ok {
            continue;
        }
        unsafe {
            if af == AF_INET.0 as u32 {
                let t = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(t.table.as_ptr(), t.dwNumEntries as usize);
                for r in rows {
                    let ip = Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes());
                    let ip = if ip.is_unspecified() { Ipv4Addr::LOCALHOST } else { ip };
                    push_listener(&mut out, r.dwOwningPid, r.dwLocalPort, IpAddr::V4(ip));
                }
            } else {
                let t = &*(buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(t.table.as_ptr(), t.dwNumEntries as usize);
                for r in rows {
                    let ip = Ipv6Addr::from(r.ucLocalAddr);
                    let ip = if ip.is_unspecified() { Ipv6Addr::LOCALHOST } else { ip };
                    push_listener(&mut out, r.dwOwningPid, r.dwLocalPort, IpAddr::V6(ip));
                }
            }
        }
    }
    out
}

fn push_listener(out: &mut Vec<Listener>, pid: u32, raw_port: u32, ip: IpAddr) {
    let port = u16::from_be(raw_port as u16);
    if !out.iter().any(|l| l.pid == pid && l.port == port) {
        out.push(Listener { pid, port, addr: SocketAddr::new(ip, port) });
    }
}

// ---------------------------------------------------------------- processes

struct ProcTable {
    exe: HashMap<u32, String>,
    children: HashMap<u32, Vec<u32>>,
}

fn process_table() -> ProcTable {
    let mut t = ProcTable { exe: HashMap::new(), children: HashMap::new() };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return t };
        let snap = Handle(snap);
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap.0, &mut e).is_ok();
        while more {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            t.exe.insert(e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..len]));
            if e.th32ParentProcessID != e.th32ProcessID {
                t.children.entry(e.th32ParentProcessID).or_default().push(e.th32ProcessID);
            }
            more = Process32NextW(snap.0, &mut e).is_ok();
        }
    }
    t
}

impl ProcTable {
    /// The process and its live descendants (root first). A child only counts if it was
    /// created after its parent, which rules out stale parent ids reused by the OS.
    fn tree(&self, root: u32, times: &mut HashMap<u32, u64>) -> Vec<u32> {
        let mut time = |pid: u32| *times.entry(pid).or_insert_with(|| open(pid, PROCESS_QUERY_LIMITED_INFORMATION).map_or(0, |h| create_time(&h)));
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            let pt = time(p);
            if let Some(kids) = self.children.get(&p) {
                for &k in kids {
                    if !out.contains(&k) && pt != 0 && time(k) >= pt {
                        out.push(k);
                    }
                }
            }
            i += 1;
        }
        out
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Handle> {
    unsafe { OpenProcess(access, false, pid).ok().map(Handle) }
}

fn create_time(h: &Handle) -> u64 {
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe {
        if GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u).is_err() {
            return 0;
        }
    }
    let c: FILETIME = c;
    ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64
}

fn working_set(pid: u32) -> u64 {
    let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return 0 };
    let mut pmc = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe {
        if GetProcessMemoryInfo(h.0, &mut pmc, pmc.cb).is_err() {
            return 0;
        }
    }
    pmc.WorkingSetSize as u64
}

fn image_path(h: &Handle) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()? };
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Launchers, updaters and store apps (Battle.net agent, Steam helpers…) live here;
/// dev runtimes (node, python, dotnet, java) live in Program Files or user folders.
fn installed_app(path: &str) -> bool {
    let p = path.to_lowercase();
    ["c:\\programdata\\", "\\program files (x86)\\", "\\windowsapps\\", "c:\\windows\\"].iter().any(|d| p.contains(d))
}

/// Reads the PE header: true for console-subsystem executables (node, python, dotnet, go…),
/// false for GUI apps (Spotify, Discord, VS Code helpers…) that also expose local servers.
fn is_console_exe(path: &str) -> bool {
    let mut hdr = [0u8; 4096];
    let Ok(n) = File::open(path).and_then(|mut f| f.read(&mut hdr)) else { return false };
    if n < 0x40 || &hdr[..2] != b"MZ" {
        return false;
    }
    let pe = u32::from_le_bytes(hdr[0x3c..0x40].try_into().unwrap()) as usize;
    let at = pe + 24 + 68;
    if at + 2 > n || &hdr[pe..pe + 4] != b"PE\0\0" {
        return false;
    }
    u16::from_le_bytes([hdr[at], hdr[at + 1]]) == 3
}

#[repr(C)]
struct UStr {
    len: u16,
    max: u16,
    buf: *const u16,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

fn command_line(h: &Handle) -> Option<String> {
    let mut buf: Vec<u64> = vec![0; 1024];
    for _ in 0..3 {
        let mut ret = 0u32;
        let st = unsafe { NtQueryInformationProcess(h.0, 60, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret) };
        if st == 0 {
            let us = unsafe { &*(buf.as_ptr() as *const UStr) };
            if us.buf.is_null() {
                return None;
            }
            let s = unsafe { std::slice::from_raw_parts(us.buf, us.len as usize / 2) };
            return Some(String::from_utf16_lossy(s));
        }
        if ret as usize <= buf.len() * 8 {
            return None;
        }
        buf.resize(ret as usize / 8 + 8, 0);
    }
    None
}

/// Current directory of another (64-bit) process, read from its PEB.
fn current_dir(h: &Handle) -> Option<String> {
    unsafe {
        let mut wow = BOOL(0);
        IsWow64Process(h.0, &mut wow).ok()?;
        if wow.as_bool() {
            return None;
        }
        let mut pbi = [0usize; 6];
        let mut ret = 0u32;
        if NtQueryInformationProcess(h.0, 0, pbi.as_mut_ptr() as *mut c_void, 48, &mut ret) != 0 {
            return None;
        }
        let peb = pbi[1];
        let mut params = 0usize;
        ReadProcessMemory(h.0, (peb + 0x20) as *const c_void, &mut params as *mut _ as *mut c_void, 8, None).ok()?;
        let mut us = UStr { len: 0, max: 0, buf: std::ptr::null() };
        ReadProcessMemory(h.0, (params + 0x38) as *const c_void, &mut us as *mut _ as *mut c_void, size_of::<UStr>(), None).ok()?;
        if us.len == 0 || us.len > 4096 {
            return None;
        }
        let mut s = vec![0u16; us.len as usize / 2];
        ReadProcessMemory(h.0, us.buf as *const c_void, s.as_mut_ptr() as *mut c_void, us.len as usize, None).ok()?;
        Some(String::from_utf16_lossy(&s))
    }
}

// ---------------------------------------------------------------- naming

fn last_segment(p: &str) -> Option<String> {
    let p = p.trim_end_matches(['\\', '/', '"', ' ']);
    let seg = p.rsplit(['\\', '/', '"']).next()?.trim();
    (!seg.is_empty() && !seg.ends_with(':')).then(|| seg.to_string())
}

fn meaningful_dir(dir: &str) -> bool {
    let d = dir.trim_end_matches('\\').to_lowercase();
    let home = std::env::var("USERPROFILE").unwrap_or_default().trim_end_matches('\\').to_lowercase();
    d.len() > 3 && d != home && !d.starts_with("c:\\windows") && !d.contains("\\program files")
}

/// Human name for a server: the project folder (from node_modules path or cwd), plus a
/// short detail (the tool, e.g. "vite", or the executable name).
fn derive_name(exe: &str, cmd: Option<&str>, cwd: Option<&str>) -> (String, String) {
    let stem = if exe.to_lowercase().ends_with(".exe") { &exe[..exe.len() - 4] } else { exe };
    let mut tool = String::new();
    let mut project = None;
    if let Some(cmd) = cmd {
        let norm = cmd.replace('/', "\\");
        let lower = norm.to_lowercase();
        const NM: &str = "\\node_modules\\";
        if let (Some(first), Some(last)) = (lower.find(NM), lower.rfind(NM)) {
            let mut parts = norm[last + NM.len()..].split('\\');
            let mut t = parts.next().unwrap_or("").to_string();
            if t == ".bin" {
                t = parts.next().unwrap_or("").to_string();
            } else if t.starts_with('@') {
                t = format!("{t}/{}", parts.next().unwrap_or(""));
            }
            tool = t.split(['"', ' ']).next().unwrap_or("").to_string();
            let before = &norm[..first];
            if !before.to_lowercase().contains("\\appdata\\") {
                project = last_segment(before);
            }
        }
    }
    if project.is_none() {
        project = cwd.filter(|d| meaningful_dir(d)).and_then(last_segment);
    }
    let detail = if tool.is_empty() { stem.to_string() } else { tool.clone() };
    match project {
        Some(p) if p.eq_ignore_ascii_case(&detail) => (p, String::new()),
        Some(p) => (p, detail),
        None => (stem.to_string(), tool),
    }
}
