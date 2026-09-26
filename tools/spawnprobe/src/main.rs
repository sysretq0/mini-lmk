//! Kill-dispatch measurement harness -- the source of every latency number in
//! `docs/ROADMAP.md` section 3, and the only way to check the acceptance criteria
//! in section 8.
//!
//! It is deliberately not a workspace member of the daemon (see the `[workspace]`
//! note in `../../Cargo.toml`): it is edition 2024, it hand-writes aarch64 `clone`
//! assembly, and it resolves Bionic symbols through `dlsym` that the product must
//! not depend on. A measurement harness that can be silently reached by a product
//! build is a product decision waiting to happen. Build it on purpose:
//!
//! ```text
//! cargo build --release --target aarch64-linux-android \
//!     --manifest-path tools/spawnprobe/Cargo.toml
//! adb push tools/spawnprobe/target/aarch64-linux-android/release/spawnprobe /data/local/tmp/
//! adb shell /data/local/tmp/spawnprobe 60 cmd vf
//! ```
//!
//! Modes are argv[3]; argv[1] is the iteration count (default 400) and argv[2] the
//! target, `true` (a few KB, no side effects, safe at n=400) or `cmd` (the REAL
//! kill dispatch: `/system/bin/cmd activity kill` with no package argument, which
//! makes ActivityManagerService throw `IllegalArgumentException` and kill nothing).
//! Each mode and the roadmap section it regenerates:
//!
//!   ladder    (default) 3.1 -- the backend ladder, plus the file-action-count
//!                         decomposition. Note what it says: the cliff is a STEP at
//!                         "you passed a non-NULL actions object", not a slope per
//!                         action (measured on a contended device against true,
//!                         n=40: empty 753, 1 action 755, 2 727, 3 747 us, NULL 547 us).
//!                         Bionic branches on actions != NULL, so once you have an
//!                         object the count is free -- which is why D5's three adddup2
//!                         calls are not three payments.
//!   vf        3.1, 3.5 -- every backend including POSIX_SPAWN_USEVFORK, the raw
//!                         clone control, and the child clock readings that split
//!                         the parent's window
//!   vfsweep   3.2 -- the same suite re-measured inside ONE process at rising
//!                    resident footprint. This is the decisive table.
//!   env       3.3 -- in-process A/B on envp size (no cross-run subtraction)
//!   vfopen    3.4 -- adddup2 vs addopen on the vfork path, interleaved rounds
//!   openfd    3.4 -- the same comparison without POSIX_SPAWN_USEVFORK
//!   fdcheck   3.4, 6.3 -- do the file actions really redirect the child's fds
//!                         0/1/2, or does the fast path silently inherit ours?
//!   execfail  6.2 -- exec failure is asynchronous: rc=0 with a pid, status 127
//!   sweep     2   -- fork() latency against the parent's own RSS (page-table copy)
//!   ipc       5.1 -- the socketpair round trip a spawn helper would need
//!   rss [MB]  2   -- touch MB MiB of anonymous memory, then time fork()
//!
//! Three guardrails, each one here because its absence produced a number that was
//! published and then had to be withdrawn:
//!
//!   * A row that collected nothing prints `R MISSING`, and a row that collected
//!     fewer samples than it asked for says `!! SHORT`. A silently-absent row is
//!     read as "not measured", never as "the backend failed at sample 4 of 150",
//!     and used to swallow whole columns of a sweep under device contention.
//!   * More than `REAL_TARGET_MAX_ITERS` dispatches against the real kill target
//!     is refused unless `SPAWNPROBE_FORCE=1`: each one is a Binder round trip that
//!     dumps an exception stack trace into system_server's log.
//!   * Every run prints its own confounds -- load average, thread count, anonymous
//!     huge pages, swap, Android API level, resident set -- because cross-run
//!     subtraction and a contended device are the two ways this harness has lied.
//!
//! The timed window is the parent's dispatch call only. The `waitpid()` that reaps
//! the child happens outside it, because the question is "how long is the epoll
//! thread unable to call `epoll_wait()`", not "how long does the child live".
//!
//! `/system/bin/cmd` is a 46,304-byte ELF (not a shell script) that boots a runtime,
//! so this is also the test of whether posix_spawn's CLONE_VM|CLONE_VFORK fast path
//! -- which suspends the CALLER until the child's execve completes -- scales with the
//! child binary where a plain fork() does not.

// Every timed window in this file is a straight line of FFI calls. Wrapping 80+ of
// them in nested `unsafe { }` blocks would add no information a reader can use, and
// this crate is read for its protocol, not compiled into anything that ships. The
// daemon does not carry this allow.
#![allow(unsafe_op_in_unsafe_fn)]

use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::time::Instant;

// `libc::environ` is not declared for the Android target, so bind it directly.
unsafe extern "C" {
    static environ: *const *const libc::c_char;
}

type PosixSpawnFn = unsafe extern "C" fn(
    pid: *mut libc::pid_t,
    path: *const libc::c_char,
    file_actions: *const libc::c_void,
    attrp: *const libc::c_void,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> libc::c_int;

fn resolve_posix_spawn() -> Option<PosixSpawnFn> {
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, b"posix_spawn\0".as_ptr() as *const libc::c_char) };
    if sym.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute::<*mut libc::c_void, PosixSpawnFn>(sym) })
    }
}

type ActionsInit = unsafe extern "C" fn(*mut *mut libc::c_void) -> libc::c_int;
type ActionsAdddup2 = unsafe extern "C" fn(*mut *mut libc::c_void, libc::c_int, libc::c_int) -> libc::c_int;
type ActionsDestroy = unsafe extern "C" fn(*mut *mut libc::c_void) -> libc::c_int;

unsafe fn sym(name: &[u8]) -> Option<*mut libc::c_void> {
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const libc::c_char) };
    if p.is_null() { None } else { Some(p) }
}

/// The three file_actions entry points, all __INTRODUCED_IN(28) in Bionic's spawn.h.
/// posix_spawn_file_actions_t is a pointer to an opaque struct, so the object the
/// caller owns is exactly one pointer wide.
struct FileActions {
    init: ActionsInit,
    adddup2: ActionsAdddup2,
    destroy: ActionsDestroy,
    obj: *mut libc::c_void,
}

impl FileActions {
    fn resolve() -> Option<Self> {
        unsafe {
            let init = sym(b"posix_spawn_file_actions_init\0")?;
            let adddup2 = sym(b"posix_spawn_file_actions_adddup2\0")?;
            let destroy = sym(b"posix_spawn_file_actions_destroy\0")?;
            Some(FileActions {
                init: core::mem::transmute(init),
                adddup2: core::mem::transmute(adddup2),
                destroy: core::mem::transmute(destroy),
                obj: core::ptr::null_mut(),
            })
        }
    }
}

fn open_devnull() -> std::fs::File {
    std::fs::File::open("/dev/null").expect("/dev/null")
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
}

/// The sample count the row being collected right now was asked for. 0 means "this
/// loop was never annotated", in which case `stats` only checks for emptiness.
///
/// Deliberately global state: this binary is single-threaded by construction, and
/// a per-row parameter would have touched every one of the 25 call sites. It is set
/// by [`want`] and by [`Bench::new`], which already receives the requested count.
static WANT_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn want(n: usize) {
    WANT_N.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Prints one row of results.
///
/// An empty sample set used to panic on the percentile index, and a truncated one
/// printed a plausible-looking row whose `n=` nobody read. Both now announce
/// themselves, because the failure that matters in this harness is not a crash --
/// it is a column silently missing from a table that someone then quotes.
fn stats(label: &str, mut v: Vec<f64>, note: &str) {
    let want = WANT_N.load(std::sync::atomic::Ordering::Relaxed);
    if v.is_empty() {
        println!("R MISSING {label:34} n=0 of {want} -- NO DATA, nothing to quote ({note})");
        return;
    }
    v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = |q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let short = if want > 0 && v.len() < want {
        format!("  !! SHORT: {} of {want} samples survived -- the backend FAILED partway", v.len())
    } else {
        String::new()
    };
    println!(
        "R {label:34} P50 {:9.1} us  P95 {:9.1} us  P99 {:9.1} us  mean {:9.1} us  n={}  {note}{short}",
        p(0.50), p(0.95), p(0.99), mean, v.len()
    );
}

/// First whitespace-delimited word of a pseudo-file (`/proc/loadavg`), or "?".
fn first_word(path: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".into())
}

/// A `Key:	value` line from /proc/self/status (`Threads`, `Seccomp`), or "?".
fn status_field(key: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "?".into())
}

/// Android API level, or "n/a" off-device. Whether `posix_spawn` exists, whether it
/// takes the vfork path, and whether it applies file actions at all are per-release
/// behaviours (docs/ROADMAP.md section 6.4), so a row without its API level beside it
/// is not reproducible even in principle.
fn api_level() -> String {
    #[cfg(target_os = "android")]
    {
        let mut buf = [0u8; 32];
        let n = unsafe {
            libc::__system_property_get(
                b"ro.build.version.sdk\0".as_ptr() as *const libc::c_char,
                buf.as_mut_ptr() as *mut libc::c_char,
            )
        };
        if n > 0 {
            String::from_utf8_lossy(&buf[..n as usize]).trim_end().to_string()
        } else {
            "unknown".into()
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        "n/a".into()
    }
}

/// The confound banner. `docs/ROADMAP.md` section 9 lists the conditions that make a
/// row unquotable; this prints them so a capture cannot be pasted without them.
fn confounds(mode: &str, sections: &str, iters: usize) {
    let (ahp, sw) = unsafe { thp_swap() };
    println!("R mode={mode} -> docs/ROADMAP.md {sections}");
    println!(
        "R iters={iters} rss={} MB threads={} api={} loadavg={} anon_hugepages={} kB swap={} kB seccomp={}",
        unsafe { rss_mb() } / 1024,
        status_field("Threads"),
        api_level(),
        first_word("/proc/loadavg"),
        ahp,
        sw,
        status_field("Seccomp"),
    );
}

/// Dispatches to a real Android process are not free for anybody else: every
/// `cmd activity kill` is a Binder call into system_server that logs an exception.
const REAL_TARGET_MAX_ITERS: usize = 120;

fn usage(prog: &str) -> ! {
    eprintln!("usage: {prog} [iters] [true|cmd] [mode] [arg]");
    eprintln!("  mode: ladder (default) vf vfsweep env vfopen openfd fdcheck execfail sweep ipc rss");
    eprintln!("  rss is the only mode that takes a 4th argument (MB to touch)");
    eprintln!("  cmd dispatches are capped at {REAL_TARGET_MAX_ITERS} iterations (SPAWNPROBE_FORCE=1 to override)");
    std::process::exit(2)
}

const TRUE_PATH: &[u8] = b"/system/bin/true\0";
const CMD_PATH: &[u8] = b"/system/bin/cmd\0";

/// The child to dispatch: path plus the argv that follows it.
struct Target {
    path: &'static [u8],
    extra: &'static [&'static str],
}

impl Target {
    fn argv(&self) -> Vec<*const libc::c_char> {
        let mut v: Vec<*const libc::c_char> = vec![self.path.as_ptr() as *const libc::c_char];
        for a in self.extra {
            let c = std::ffi::CString::new(*a).unwrap();
            v.push(Box::leak(c.into_boxed_c_str()).as_ptr());
        }
        v.push(std::ptr::null());
        v
    }
}

fn target_from(name: &str) -> Target {
    match name {
        "cmd" => Target { path: CMD_PATH, extra: &["activity", "kill"] },
        _ => Target { path: TRUE_PATH, extra: &[] },
    }
}


// --- IPC ping-pong: how much would a helper process cost per handoff? ---
unsafe fn ipc_pingpong(iters: u32) {
    use std::time::Instant;
    // Probe once for AF_UNIX, then CLOSE the descriptor. Leaking it here would
    // inflate the very fd census the same run reports.
    let probe = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
    if probe < 0 {
        println!("R ipc: socket(AF_UNIX) unsupported: {}", std::io::Error::last_os_error());
        return;
    }
    libc::close(probe);
    let mut sp = [0i32; 2];
    if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) != 0 {
        // std's last_os_error() rather than libc::__errno(): __errno is Bionic-only,
        // and this harness is expected to at least type-check for a host build.
        println!("R ipc: socketpair failed {}", std::io::Error::last_os_error());
        return;
    }
    let pid = libc::fork();
    if pid == 0 {
        libc::close(sp[0]);
        let mut buf = [0u8; 8];
        for _ in 0..iters {
            if libc::read(sp[1], buf.as_mut_ptr() as *mut libc::c_void, 8) != 8 { break; }
            if libc::write(sp[1], buf.as_ptr() as *const libc::c_void, 8) != 8 { break; }
        }
        libc::_exit(0);
    }
    libc::close(sp[1]);
    // Establish the peer once, then time a single ping-pong repeatedly.
    let mut samples = Vec::with_capacity(iters as usize);
    let mut buf = [7u8; 8];
    let mut warm = 0i32;
    want((iters as usize).saturating_sub(10)); // the loop drops 10 warm-up round trips
    for i in 0..(iters as i64) {
        let t = Instant::now();
        if libc::write(sp[0], buf.as_ptr() as *const libc::c_void, 8) != 8 { break; }
        if libc::read(sp[0], buf.as_mut_ptr() as *mut libc::c_void, 8) != 8 { break; }
        let el = t.elapsed().as_micros() as u64;
        if i >= 10 { samples.push(el); } else { warm += 1; }
        let _ = warm;
    }
    libc::waitpid(pid, std::ptr::null_mut(), 0);
    libc::close(sp[0]);
    stats("unix socketpair ping-pong", samples.iter().map(|x| *x as f64).collect(), &format!("n={} (10 warm-up dropped)", samples.len()));
}


unsafe fn rss_mb() -> usize {
    let ok = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in ok.lines() {
        if let Some(rest) = l.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

/// (AnonHugePages kB, Swap kB) from smaps_rollup: the two things that would
/// change the number of PRESENT page-table entries the fork must copy.
unsafe fn thp_swap() -> (usize, usize) {
    let mut ahp = 0; let mut sw = 0;
    if let Ok(ok) = std::fs::read_to_string("/proc/self/smaps_rollup") {
        for l in ok.lines() {
            let num = l.split_once(':').map(|(_, v)| v.trim().trim_end_matches(" kB").trim().parse::<usize>().unwrap_or(0)).unwrap_or(0);
            if l.starts_with("AnonHugePages:") { ahp = num; }
            if l.starts_with("Swap:") { sw = num; }
        }
    }
    (ahp, sw)
}

unsafe fn rss_mode(mb: usize) {
    use std::time::Instant;
    let mut touched = std::ptr::null_mut();
    if mb > 0 {
        let len = mb * 1024 * 1024;
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        if p == libc::MAP_FAILED { println!("R rss: mmap of {mb} MB failed"); return; }
        touched = p;
        let mut i = 0usize;
        while i < len { *(p as *mut u8).add(i) = 1; i += 4096; }  // one byte per page
    }
    println!("R rss_probe parent_vmRSS={} MB (asked to touch {} MB)", rss_mb() / 1024, mb);
    let base = rss_mb();
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let t = Instant::now();
        let pid = fork_exec(3, TRUE_PATH.as_ptr() as *const libc::c_char, std::ptr::null());
        samples.push(t.elapsed().as_micros() as u64);
        if pid > 0 { reap_wait(pid); }
    }
    stats("fork+execve", samples.iter().map(|x| *x as f64).collect(),
        &format!("parent={} MB", base / 1024));
    if !touched.is_null() { libc::munmap(touched, mb * 1024 * 1024); }
}


/// One process, increasing resident footprint, fork cost re-measured at each
/// step. Within-process, so no cross-run drift can contaminate the curve.
unsafe fn rss_sweep() {
    use std::time::Instant;
    const LEN: usize = 1536 * 1024 * 1024; // 1.5 GB reserve, touched progressively
    let p = libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
    if p == libc::MAP_FAILED { println!("R sweep: mmap failed"); return; }
    println!("R thp_setting={}", std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_else(|_| "?".into()).trim().to_string());
    println!("R meminfo={}", std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().take(3).collect::<Vec<_>>().join(" | "));
    let base = rss_mb() * 1024; // bytes resident before any touching
    let mut touched_bytes = 0usize;
    for step_mb in [0usize, 4, 16, 32, 64, 128, 256, 512, 1024, 1536] {
        let target = step_mb * 1024 * 1024;
        while touched_bytes < target {
            *(p as *mut u8).add(touched_bytes) = 1; // one byte per page
            touched_bytes += 4096;
        }
        let rss = rss_mb(); // kB
        let mut v = Vec::with_capacity(150);
        let mut w = Vec::with_capacity(150);
        let argv0: Vec<*const libc::c_char> = vec![TRUE_PATH.as_ptr() as *const libc::c_char, std::ptr::null()];
        want(150);
        for _ in 0..150 {
            let t = Instant::now();
            let pid = fork_exec(3, TRUE_PATH.as_ptr() as *const libc::c_char, std::ptr::null());
            v.push(t.elapsed().as_micros() as u64);
            if pid > 0 { reap_wait(pid); } else { v.pop(); break; }
            // The raw row reports raw::mono_ns (sp[0]) -- the same clock the child
            // stamps its own offsets with -- so a second Instant here is dead weight.
            let (pid, sp) = raw_vfork_once(3, TRUE_PATH.as_ptr() as *const libc::c_char, argv0.as_ptr(), environ, false);
            w.push(sp[0] as f64);
            if pid > 0 { reap_wait(pid); } else { w.pop(); break; }
        }
        let (ahp, sw) = thp_swap();
        stats(&format!("fork, rss {rss:>7} kB thp {ahp:>7} kB swap {sw:>6} kB"),
            v.iter().map(|x| *x as f64).collect(), &format!("touched {} MB", step_mb));
        stats(&format!("raw clone VM|VFORK, rss {rss:>7} kB"),
            w, "no page tables copied");
    }
    let _ = base;
    libc::munmap(p, LEN);
}

/// Times one dispatch window and the full child lifetime separately.
///
/// `spawn` is the parent's stall: the question the roadmap cares about, because
/// it is how long the epoll thread cannot call epoll_wait(). `total` is measured
/// from before the spawn to after waitpid returns, so `total - spawn` is what the
/// child spends on its own AFTER the parent is released -- the part of exec,
/// dynamic linking and teardown that a vfork-family backend does not hide behind.
struct Bench {
    spawn: Vec<f64>,
    total: Vec<f64>,
    exits: Vec<String>,
}

impl Bench {
    fn new(n: usize) -> Self {
        // The capacity argument is the requested sample count; reuse it as the
        // expectation so rows() can flag a truncated row without a new call site.
        want(n);
        Bench { spawn: Vec::with_capacity(n), total: Vec::with_capacity(n), exits: Vec::new() }
    }
    fn push(&mut self, t0: std::time::Instant, pid: libc::pid_t, track_exit: bool) {
        let stall = t0.elapsed().as_secs_f64() * 1e6;
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let whole = t0.elapsed().as_secs_f64() * 1e6;
        self.spawn.push(stall);
        self.total.push(whole);
        if track_exit && self.exits.len() < 6 {
            self.exits.push(format!(
                "exit{}{}={}",
                if libc::WIFEXITED(status) { 'E' } else { '?' },
                if libc::WIFSIGNALED(status) { "sig" } else { "" },
                if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    -1
                }
            ));
        }
    }
    fn rows(self, label: &str, note: &str) {
        let exits = self.exits.join(",");
        stats(&format!("{label} [parent]"), self.spawn, &format!("{note} {exits}"));
        stats(&format!("{label} [whole]"), self.total, "spawn+waitpid: the child's own runtime too");
    }
}

/// The vfork-family question: can the parent's window be made both cheaper than
/// fork() and independent of the daemon's resident memory, while still
/// redirecting the child's stdio?
unsafe fn vfork_suite(iters: usize, tgt: &Target) {
    let argv = tgt.argv();
    let envp = environ;
    let pathp = tgt.path.as_ptr() as *const libc::c_char;
    let bin = if std::ptr::eq(tgt.path, CMD_PATH) { "/system/bin/cmd" } else { "/system/bin/true" };
    let devnull =
        unsafe { libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC) };
    let (ahp, sw) = unsafe { thp_swap() };
    println!(
        "R suite=vfork target={bin} iters={iters} devnull_fd={devnull} pid={} thp={ahp} kB swap={sw} kB loadavg={}",
        unsafe { libc::getpid() },
        first_word("/proc/loadavg")
    );
    let mk = || {
        let mut c = Command::new(bin);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for a in tgt.extra {
            c.arg(a);
        }
        c
    };

    // Does Bionic even accept POSIX_SPAWN_USEVFORK? glibc made it a no-op in 2.24;
    // the disassembly says Bionic still tests bit 6. getflags() settles the store.
    if let Some(mut at) = SpawnAttr::resolve() {
        let r_init = unsafe { (at.init)(&mut at.obj) };
        let r_set = unsafe { (at.setflags)(&mut at.obj, POSIX_SPAWN_USEVFORK) };
        let mut got: libc::c_short = -1;
        let r_get = unsafe { (at.getflags)(&at.obj, &mut got) };
        println!(
            "R usevfork_attr init={r_init} setflags={r_set} getflags={r_get} readback=0x{got:x} want=0x{:x} -- bit 6 is stored; the flat USEVFORK row below is the behavioural proof that Bionic honours it",
            POSIX_SPAWN_USEVFORK
        );
        let _ = unsafe { (at.destroy)(&mut at.obj) };
    } else {
        println!("R posix_spawnattr_* unresolved -> USEVFORK branch not testable");
    }

    let mut b = Bench::new(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let child = match mk().spawn() {
            Ok(c) => c,
            Err(e) => {
                println!("R std::process::Command spawn failed: {e}");
                break;
            }
        };
        b.push(t, child.id() as libc::pid_t, true);
    }
    b.rows("std::process::Command", "");

    let mut b = Bench::new(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let pid = unsafe { fork_exec(devnull, pathp, argv.as_ptr()) };
        b.push(t, pid, true);
    }
    b.rows("fork + dup2 + execve", "");

    // The pure-Rust backend: clone(CLONE_VM|CLONE_VFORK), asm child, dup3 x3.
    if cfg!(target_arch = "aarch64") {
        let mut b = Bench::new(iters);
        let mut entry = Vec::new();
        let mut pre_exec = Vec::new();
        for _ in 0..iters {
            let t = Instant::now();
            let (pid, sp) = unsafe { raw_vfork_once(devnull, pathp, argv.as_ptr(), envp, false) };
            if pid <= 0 {
                println!("R raw_clone FAILED pid={pid} (errno {})", std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
                break;
            }
            entry.push(sp[1]);
            pre_exec.push(sp[2]);
            b.push(t, pid, true);
        }
        b.rows("raw clone VM|VFORK", "[pure asm child]");
        // Where inside the parent's window the child's own clock says it is.
        if !entry.is_empty() {
            stats("  child: clone->entry", entry.clone(), "kernel setup, parent already suspended");
            stats("  child: entry->pre-execve", pre_exec.iter().zip(entry.iter()).map(|(a, b)| a - b).collect(), "3 dup3 + 2 clock_gettime");
        }
        // Control: CLONE_VM without CLONE_VFORK. The parent is NOT suspended, so
        // the child may read ARGS after the parent has moved on -- unsound, and it
        // exists only to price the suspension itself.
        // Reaped inside each iteration, so only one child is ever alive: without
        // CLONE_VFORK the parent would otherwise re-enter this loop -- and rewrite
        // ARGS and CHILD_STACK -- underneath a running child.
        let n = iters / 4;
        let mut b = Bench::new(n);
        for _ in 0..n {
            let t = Instant::now();
            let (pid, _) = unsafe { raw_vfork_once(devnull, pathp, argv.as_ptr(), envp, true) };
            if pid <= 0 {
                println!("R raw_clone VM-only FAILED at sample {} of {n}", b.spawn.len() + 1);
                break;
            }
            b.push(t, pid, false);
        }
        b.rows("raw clone VM only (no VFORK)", "UNSAFE control, prices the suspension");
    } else {
        println!("R raw clone backend is aarch64 asm only");
    }

    if let Some(spawn) = resolve_posix_spawn() {
        // NULL file_actions: Bionic's vfork fast path, but the child inherits our stdio.
        let mut b = Bench::new(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let mut pid: libc::pid_t = 0;
            let rc = unsafe { spawn(&mut pid, pathp, core::ptr::null(), core::ptr::null(), argv.as_ptr(), envp) };
            if rc != 0 {
                println!("R posix_spawn NULL rc={rc}");
                break;
            }
            b.push(t, pid, true);
        }
        b.rows("posix_spawn NULL actions", "inherits our fd 0/1/2 -- unshipable");

        // 3 dup2 actions: the shape production needs. Bionic switches to fork().
        if let Some(mut fa) = FileActions::resolve() {
            let mut b = Bench::new(iters);
            for _ in 0..iters {
                let t = Instant::now();
                let mut pid: libc::pid_t = 0;
                unsafe { (fa.init)(&mut fa.obj) };
                for fd in 0..3 {
                    unsafe { (fa.adddup2)(&mut fa.obj, devnull, fd) };
                }
                let rc = unsafe {
                    spawn(&mut pid, pathp, &fa.obj as *const _ as *const libc::c_void, core::ptr::null(), argv.as_ptr(), envp)
                };
                let _ = unsafe { (fa.destroy)(&mut fa.obj) };
                if rc != 0 {
                    println!("R posix_spawn actions rc={rc}");
                    break;
                }
                b.push(t, pid, true);
            }
            b.rows("posix_spawn + 3 dup2", "Bionic: actions => fork()");

            // Same, forced back onto the vfork path with POSIX_SPAWN_USEVFORK.
            if let Some(mut at) = SpawnAttr::resolve() {
                let mut b = Bench::new(iters);
                for _ in 0..iters {
                    let t = Instant::now();
                    let mut pid: libc::pid_t = 0;
                    unsafe { (fa.init)(&mut fa.obj) };
                    for fd in 0..3 {
                        unsafe { (fa.adddup2)(&mut fa.obj, devnull, fd) };
                    }
                    unsafe { (at.init)(&mut at.obj) };
                    let rset = unsafe { (at.setflags)(&mut at.obj, POSIX_SPAWN_USEVFORK) };
                    let rc = unsafe {
                        spawn(&mut pid, pathp, &fa.obj as *const _ as *const libc::c_void, &at.obj as *const _ as *const libc::c_void, argv.as_ptr(), envp)
                    };
                    let _ = unsafe { (fa.destroy)(&mut fa.obj) };
                    let _ = unsafe { (at.destroy)(&mut at.obj) };
                    if rc != 0 {
                        println!("R posix_spawn USEVFORK rc={rc} setflags={rset}");
                        break;
                    }
                    b.push(t, pid, true);
                }
                b.rows("posix_spawn + 3 dup2 + USEVFORK", &format!("attrp non-NULL, flags=0x{POSIX_SPAWN_USEVFORK:x}"));
            }
        }
    }

    // One sample is enough to show the timed window itself is nanoseconds. The old
    // Bench::new(8) implied eight requested samples and made the SHORT guard scream
    // at a healthy row, so this states its own count.
    let t = Instant::now();
    std::hint::black_box(&t);
    want(1);
    stats("harness baseline", vec![t.elapsed().as_secs_f64() * 1e6], "Instant::now() twice");
}

/// The article's second axis, measured here rather than assumed: the environment
/// is copied into the child's new stack by the kernel, which for a vfork-family
/// backend happens WHILE THE PARENT IS SUSPENDED. The daemon hands `cmd` its whole
/// `environ` today, so this prices that decision inside one process (no cross-run
/// drift), at three sizes, for the three backends that matter.
unsafe fn env_suite(iters: usize, tgt: &Target) {
    let argv = tgt.argv();
    let pathp = tgt.path.as_ptr() as *const libc::c_char;
    let devnull =
        unsafe { libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC) };
    let mk = || {
        let mut c = Command::new(if std::ptr::eq(tgt.path, CMD_PATH) { "/system/bin/cmd" } else { "/system/bin/true" });
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for a in tgt.extra {
            c.arg(a);
        }
        c
    };
    let env_bytes = || {
        let mut n = 0usize;
        unsafe {
            let mut p = environ;
            while !p.is_null() && !(*p).is_null() {
                n += libc::strlen(*p) + 1;
                p = p.add(1);
            }
        }
        n
    };
    let bin = if std::ptr::eq(tgt.path, CMD_PATH) { "/system/bin/cmd" } else { "/system/bin/true" };
    println!("R suite=env target={bin} iters={iters} pid={}", unsafe { libc::getpid() });
    let mut pad = String::with_capacity(301);
    for _ in 0..300 {
        pad.push('a');
    }
    // as-shipped -> fat -> empty (clearenv is destructive, so it goes last).
    for stage in 0..3 {
        match stage {
            1 => {
                for i in 0..100 {
                    let k = format!("MLMK_PAD_{i}");
                    unsafe { libc::setenv(k.as_ptr() as *const libc::c_char, pad.as_ptr() as *const libc::c_char, 1) };
                }
            }
            2 =>
                unsafe {
                    libc::clearenv();
                },
            _ => {}
        }
        let eb = env_bytes();
        let mut brok = 0usize;
        let mut b1 = Bench::new(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let (pid, _) = unsafe { raw_vfork_once(devnull, pathp, argv.as_ptr(), environ, false) };
            if pid <= 0 {
                brok += 1;
                break;
            }
            b1.push(t, pid, true);
        }
        b1.rows(&format!("raw clone VM|VFORK, env {eb} B"), "");
        let mut b2 = Bench::new(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let pid = unsafe { fork_exec(devnull, pathp, argv.as_ptr()) };
            b2.push(t, pid, true);
        }
        b2.rows(&format!("fork + dup2 + execve, env {eb} B"), "");
        let mut b3 = Bench::new(iters);
        for _ in 0..iters {
            let t = Instant::now();
            match mk().spawn() {
                Ok(c) => b3.push(t, c.id() as libc::pid_t, true),
                Err(e) => {
                    println!("R std spawn failed with env {eb} B: {e}");
                    break;
                }
            }
        }
        b3.rows(&format!("std::process::Command, env {eb} B"), "");
        println!("R stage_done={stage} env_bytes={eb} raw_failures={brok}");
    }
}

/// Keep `mb` MiB of anonymous memory mapped and touched (one byte per page) for
/// the rest of the process's life. Leaked on purpose: the point is a stable
/// resident footprint across every backend measured afterwards.
unsafe fn reserve_and_touch(mb: usize) {
    if mb == 0 { return; }
    let len = mb * 1024 * 1024;
    let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
    if p == libc::MAP_FAILED { println!("R vfsweep: mmap of {mb} MB failed"); return; }
    let mut i = 0usize;
    while i < len { *(p as *mut u8).add(i) = 1; i += 4096; }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// Does POSIX_SPAWN_USEVFORK still APPLY the file actions, or does Bionic skip
/// them on the shared-stack path? If it skipped them, the backend would leak the
/// daemon's stdout/stderr into every `cmd` child -- a correctness bug dressed up
/// as a 1.4 ms win. Proof: redirect fd 1/2 to a file, dup /dev/null onto fd 0,
/// exec `toybox ls -l /proc/self/fd`, and read the file back. If the actions were
/// applied, the listing lands in the file and names /dev/null for fd 0.
unsafe fn fdcheck_mode() {
    let spawn_fn = match resolve_posix_spawn() { Some(f) => f, None => { println!("FDCHECK posix_spawn unavailable"); return; } };
    let (mut fa, mut at) = match (FileActions::resolve(), SpawnAttr::resolve()) {
        (Some(f), Some(a)) => (f, a),
        _ => { println!("FDCHECK file_actions/attr unavailable (need API 28+)"); return; }
    };
    let devnull = std::fs::File::open("/dev/null").expect("devnull");
    for (usevfork, use_open) in &[(false, false), (true, false), (true, true)] {
        let outpath = if !*usevfork { "/data/local/tmp/fdcheck_fork.txt".to_string() }
            else if *use_open { "/data/local/tmp/fdcheck_vf_open.txt".to_string() }
            else { "/data/local/tmp/fdcheck_vf.txt".to_string() };
        let tmp = std::fs::OpenOptions::new().read(true).write(true).create(true)
            .truncate(true).open(outpath.as_str()).expect("tmp");
        let r0 = (fa.init)(&mut fa.obj);
        let (addopen, r1, r2, r3) = if *use_open {
            let ao = sym(b"posix_spawn_file_actions_addopen\0").unwrap();
            let ao = core::mem::transmute::<_, unsafe extern "C" fn(*mut *mut libc::c_void, libc::c_int, *const libc::c_char, libc::c_int, libc::mode_t) -> libc::c_int>(ao);
            let np = std::ffi::CString::new("/dev/null").unwrap();
            let tp = std::ffi::CString::new(outpath.as_str()).unwrap();
            (true, ao(&mut fa.obj, 0, np.as_ptr(), libc::O_RDWR, 0), ao(&mut fa.obj, 1, tp.as_ptr(), libc::O_RDWR|libc::O_CREAT|libc::O_TRUNC, 0o644), ao(&mut fa.obj, 2, tp.as_ptr(), libc::O_RDWR|libc::O_APPEND, 0o644))
        } else {
            (false, (fa.adddup2)(&mut fa.obj, devnull.as_raw_fd(), 0), (fa.adddup2)(&mut fa.obj, tmp.as_raw_fd(), 1), (fa.adddup2)(&mut fa.obj, tmp.as_raw_fd(), 2))
        };
        let ra = (at.init)(&mut at.obj);
        let rs = if *usevfork { (at.setflags)(&mut at.obj, POSIX_SPAWN_USEVFORK) } else { 0 };
        let sh = std::ffi::CString::new("/system/bin/toybox").unwrap();
        let a1 = std::ffi::CString::new("ls").unwrap();
        let a2 = std::ffi::CString::new("-l").unwrap();
        let a3 = std::ffi::CString::new("/proc/self/fd").unwrap();
        let argv: Vec<*const libc::c_char> =
            vec![sh.as_ptr(), a1.as_ptr(), a2.as_ptr(), a3.as_ptr(), std::ptr::null()];
        let envp: *const *const libc::c_char = environ;
        let mut pid: libc::pid_t = 0;
        let rc = spawn_fn(&mut pid, sh.as_ptr(), &fa.obj as *const _ as *const libc::c_void,
            if *usevfork { &at.obj as *const _ as *const libc::c_void } else { std::ptr::null() },
            argv.as_ptr(), envp);
        println!("FDCHECK file={outpath} usevfork={usevfork} addopen={addopen} init={r0} actions=({r1},{r2},{r3}) attr_init={ra} setflags={rs} spawn_rc={rc} pid={pid}");
        let mut status: libc::c_int = -1;
        libc::waitpid(pid, &mut status, 0);
        drop(tmp);
        match std::fs::read_to_string(outpath.as_str()) {
            Ok(txt) => {
                let fd0 = txt.lines().find(|l| l.ends_with("/fd/0 -> ") || l.contains("/fd/0 -> "))
                    .map(|l| l.split("/fd/0 -> ").nth(1).unwrap_or("?").to_string()).unwrap_or_else(|| "MISSING".into());
                let nlines = txt.lines().count();
                println!("FDCHECK   captured_lines={nlines} fd0={fd0}");
                for l in txt.lines().filter(|l| l.contains("/fd/")) { println!("FDCHECK     {}", l.trim()); }
            }
            Err(e) => println!("FDCHECK   no output captured ({e}) -- actions NOT applied"),
        }
    }
}

/// When the vfork path suspends the parent, how does a failed execve surface?
/// POSIX allows it to come back as the child's exit status instead of an errno,
/// which decides whether `spawn_errno` can ever report ENOENT for this backend.
unsafe fn execfail_mode() {
    let spawn_fn = match resolve_posix_spawn() { Some(f) => f, None => { println!("EXECFAIL posix_spawn unavailable"); return; } };
    let (mut fa, mut at) = match (FileActions::resolve(), SpawnAttr::resolve()) {
        (Some(f), Some(a)) => (f, a), _ => { println!("EXECFAIL actions/attr unavailable"); return; }
    };
    let devnull = std::fs::File::open("/dev/null").unwrap();
    for usevfork in &[false, true] {
        let _ = (fa.init)(&mut fa.obj);
        for fd in 0..3 { let _ = (fa.adddup2)(&mut fa.obj, devnull.as_raw_fd(), fd); }
        let _ = (at.init)(&mut at.obj);
        if *usevfork { let _ = (at.setflags)(&mut at.obj, POSIX_SPAWN_USEVFORK); }
        let bad = std::ffi::CString::new("/system/bin/no-such-binary").unwrap();
        let argv: Vec<*const libc::c_char> = vec![bad.as_ptr(), std::ptr::null()];
        let mut pid: libc::pid_t = -1;
        let t = std::time::Instant::now();
        let rc = spawn_fn(&mut pid, bad.as_ptr(), &fa.obj as *const _ as *const libc::c_void,
            if *usevfork { &at.obj as *const _ as *const libc::c_void } else { std::ptr::null() },
            argv.as_ptr(), environ);
        let dur = t.elapsed().as_micros();
        if rc != 0 {
            println!("EXECFAIL usevfork={usevfork} SYNCHRONOUS errno={rc} ({}) pid={pid} parent_waited={dur} us",
                std::io::Error::from_raw_os_error(rc));
            continue;
        }
        let mut status: libc::c_int = -1;
        let w = libc::waitpid(pid, &mut status, 0);
        println!("EXECFAIL usevfork={usevfork} rc=0 pid={pid} waitpid={w} status={status:#x} parent_waited={dur} us (child reported the error)");
        let _ = (fa.destroy)(&mut fa.obj);
    }
}
/// adddup2 vs addopen, measured instead of asserted. Two questions:
///  * does the parent leak a descriptor per dispatch (the argument for `adddup2`), and
///  * is `openat("/dev/null", O_RDWR)` in the child the same price as `dup3(cached_fd)`?
/// A child that execs a failing binary is included: that is the path where a
/// parent-side open would be stranded with nobody left to close it.
unsafe fn openfd_mode(iters: usize, tgt: &Target) {
    let spawn_fn = match resolve_posix_spawn() { Some(f) => f, None => { println!("OPENFD posix_spawn unavailable"); return; } };
    let init = match sym(b"posix_spawn_file_actions_init\0") { Some(f) => core::mem::transmute::<_, ActionsInit>(f), None => { println!("OPENFD no init"); return; } };
    let destroy = match sym(b"posix_spawn_file_actions_destroy\0") { Some(f) => core::mem::transmute::<_, ActionsDestroy>(f), None => { println!("OPENFD no destroy"); return; } };
    let adddup2 = match sym(b"posix_spawn_file_actions_adddup2\0") { Some(f) => core::mem::transmute::<_, ActionsAdddup2>(f), None => { println!("OPENFD no adddup2"); return; } };
    let addopen = match sym(b"posix_spawn_file_actions_addopen\0") { Some(f) => f, None => { println!("OPENFD no addopen (need API 28+)"); return; } };
    let addopen = core::mem::transmute::<_, unsafe extern "C" fn(*mut *mut libc::c_void, libc::c_int, *const libc::c_char, libc::c_int, libc::mode_t) -> libc::c_int>(addopen);

    let argv = tgt.argv();
    let pathp = tgt.path.as_ptr() as *const libc::c_char;
    let devnull = std::fs::File::open("/dev/null").unwrap();
    let nullpath = std::ffi::CString::new("/dev/null").unwrap();
    let bad = std::ffi::CString::new("/system/bin/no-such-binary").unwrap();
    let bad_argv: Vec<*const libc::c_char> = vec![bad.as_ptr(), std::ptr::null()];

    for (label, use_open, failing) in [("adddup2 (cached fd)", false, false), ("addopen (path)", true, false), ("adddup2, exec FAILS", false, true), ("addopen, exec FAILS", true, true)] {
        let mut obj: *mut libc::c_void = std::ptr::null_mut();
        let _ = init(&mut obj);
        let mut r = Vec::new();
        for fd in 0..3 {
            let rc = if use_open { addopen(&mut obj, fd, nullpath.as_ptr(), libc::O_RDWR, 0) } else { adddup2(&mut obj, devnull.as_raw_fd(), fd) };
            r.push(rc);
        }
        let fds_before = fd_count();
        let mut samples = Vec::with_capacity(iters);
        let mut pids = Vec::with_capacity(iters);
        let (apath, aargv) = if failing { (bad.as_ptr(), bad_argv.as_ptr()) } else { (pathp, argv.as_ptr()) };
        for _ in 0..iters {
            let mut pid: libc::pid_t = 0;
            let t = std::time::Instant::now();
            let rc = spawn_fn(&mut pid, apath, &obj as *const _ as *const libc::c_void, std::ptr::null(), aargv, environ);
            samples.push(t.elapsed().as_micros() as f64);
            if rc == 0 { pids.push(pid); } else { println!("OPENFD {label}: spawn rc={rc}"); }
        }
        for pid in &pids { reap(*pid as u32); }
        let _ = destroy(&mut obj);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let fds_after = fd_count();
        stats(label, samples, &format!("action_rcs={:?} fds_before={fds_before} fds_after={fds_after} leaked={}", r, fds_after.saturating_sub(fds_before)));
    }
    println!("OPENFD note: no USEVFORK here; both variants take Bionic's fork() branch, so the absolute numbers are fork-class. The comparison that matters is the fd delta and the per-action cost.");
}
/// USEVFORK with adddup2 vs USEVFORK with addopen, interleaved in one process.
/// On the vfork path the child's work is inside the parent's suspended window, so
/// a child-side openat() (path walk + VFS + SELinux) is charged to the reactor
/// stall, while a dup3() of a fd the parent already holds is not. This is the
/// measurement that decides which file action the shipping backend uses.
unsafe fn vfopen_mode(iters: usize, tgt: &Target) {
    let spawn_fn = match resolve_posix_spawn() { Some(f) => f, None => { println!("VFOPEN posix_spawn unavailable"); return; } };
    let (mut at, b) = match (SpawnAttr::resolve(), FileActions::resolve()) {
        (Some(a), Some(f)) => (a, f), _ => { println!("VFOPEN actions/attr unavailable"); return; }
    };
    let ao = match sym(b"posix_spawn_file_actions_addopen\0") {
        Some(x) => core::mem::transmute::<_, unsafe extern "C" fn(*mut *mut libc::c_void, libc::c_int, *const libc::c_char, libc::c_int, libc::mode_t) -> libc::c_int>(x),
        None => { println!("VFOPEN no addopen"); return; }
    };
    let devnull = std::fs::File::open("/dev/null").unwrap();
    let np = std::ffi::CString::new("/dev/null").unwrap();
    let argv = tgt.argv();
    let pathp = tgt.path.as_ptr() as *const libc::c_char;
    println!("VFOPEN target={} iters={iters} parent_rss={} MB", if std::ptr::eq(tgt.path, CMD_PATH) { "cmd" } else { "true" }, rss_mb() / 1024);
    // Interleaved rounds, so a slow device drifts both columns equally.
    for round in 0..3 {
        for use_open in [false, true] {
            let label = if use_open { "USEVFORK + addopen" } else { "USEVFORK + adddup2" };
            let _ = (at.init)(&mut at.obj);
            let _ = (at.setflags)(&mut at.obj, POSIX_SPAWN_USEVFORK);
            let mut obj: *mut libc::c_void = std::ptr::null_mut();
            let _ = (b.init)(&mut obj);
            for fd in 0..3 {
                if use_open { let _ = ao(&mut obj, fd, np.as_ptr(), libc::O_RDWR, 0); }
                else { let _ = (b.adddup2)(&mut obj, devnull.as_raw_fd(), fd); }
            }
            let mut bench = Bench::new(iters);
            for _ in 0..iters {
                let t0 = std::time::Instant::now();
                let mut pid: libc::pid_t = 0;
                let rc = spawn_fn(&mut pid, pathp, &obj as *const _ as *const libc::c_void,
                    &at.obj as *const _ as *const libc::c_void, argv.as_ptr(), environ);
                if rc != 0 { println!("VFOPEN {label} rc={rc}"); break; }
                bench.push(t0, pid, true);
            }
            let _ = (b.destroy)(&mut obj);
            let _ = (at.destroy)(&mut at.obj);
            bench.rows(label, &format!("round={round}"));
            use std::io::Write as _; let _ = std::io::stdout().flush();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "spawnprobe".into());
    let iters: usize = match args.get(1) {
        None => 400,
        Some(raw) => raw.parse().unwrap_or_else(|_| {
            eprintln!("F iters must be a number, got {raw:?}");
            usage(&prog)
        }),
    };
    let tname = args.get(2).map(String::as_str).unwrap_or("true");
    if tname != "true" && tname != "cmd" {
        eprintln!("F unknown target {tname:?} -- want true or cmd");
        usage(&prog);
    }
    let tgt = target_from(tname);
    let mode = args.get(3).map(String::as_str).unwrap_or("ladder");

    // A mistyped mode used to fall through to the default ladder, which is how a
    // run of one suite got published as another. Anything unexpected is fatal.
    if args.len() > 4 && mode != "rss" {
        eprintln!("F mode {mode} takes no 4th argument, got {:?}", args[4]);
        usage(&prog);
    }
    if mode != "rss" && mode != "sweep" && mode != "ipc" && iters > REAL_TARGET_MAX_ITERS && tname == "cmd"
        && std::env::var_os("SPAWNPROBE_FORCE").is_none()
    {
        eprintln!("F refusing {iters} dispatches against the real kill target.");
        eprintln!("F each one is a Binder round trip into system_server that dumps an AMS");
        eprintln!("F exception into its log. The published tables use n=60. Re-run with");
        eprintln!("F n<={REAL_TARGET_MAX_ITERS}, or set SPAWNPROBE_FORCE=1 if you really mean it.");
        std::process::exit(2);
    }

    match mode {
        "ladder" => {
            confounds("ladder", "3.1 (and the empty/1/2/3-action decomposition)", iters);
            unsafe { backend_ladder(iters, &tgt) };
        }
        "vf" => {
            confounds("vf", "3.1 and 3.5", iters);
            unsafe { vfork_suite(iters, &tgt) };
        }
        "vfsweep" => {
            // The decisive table: every backend, re-measured inside THIS process at
            // rising resident footprint, so "does it pay the page-table copy?" never
            // becomes a cross-run subtraction again.
            confounds("vfsweep", "3.2", iters);
            unsafe {
                for mb in [0usize, 256, 1024] {
                    reserve_and_touch(mb);
                    println!("R ==== parent rss {} MB ====", rss_mb() / 1024);
                    vfork_suite(iters, &tgt);
                }
            };
        }
        "env" => {
            confounds("env", "3.3", iters);
            unsafe { env_suite(iters, &tgt) };
        }
        "vfopen" => {
            confounds("vfopen", "3.4", iters);
            unsafe { vfopen_mode(iters, &tgt) };
        }
        "openfd" => {
            confounds("openfd", "3.4", iters);
            unsafe { openfd_mode(iters, &tgt) };
        }
        "fdcheck" => {
            confounds("fdcheck", "3.4 and 6.3", iters);
            unsafe { fdcheck_mode() };
        }
        "execfail" => {
            confounds("execfail", "6.2", iters);
            unsafe { execfail_mode() };
        }
        "sweep" => {
            confounds("sweep", "2 (the cheap-parent principle)", iters);
            unsafe { rss_sweep() };
        }
        "ipc" => {
            confounds("ipc", "5.1", iters);
            unsafe { ipc_pingpong(iters as u32) };
        }
        "rss" => {
            let mb: usize = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(0);
            confounds("rss", "2", mb);
            unsafe { rss_mode(mb) };
        }
        other => {
            eprintln!("F unknown mode {other:?}");
            usage(&prog);
        }
    }
}

/// The default ladder: the five ways to dispatch, timed in one process at one
/// footprint, plus the file-actions-count decomposition. Extracted from `main` so
/// that argument parsing and the guardrails live in exactly one place.
unsafe fn backend_ladder(iters: usize, tgt: &Target) {
    let argv = tgt.argv();
    let pathp = tgt.path.as_ptr() as *const libc::c_char;
    let bin = if std::ptr::eq(tgt.path, CMD_PATH) { "/system/bin/cmd" } else { "/system/bin/true" };
    let mk = || {
        let mut c = Command::new(bin);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for a in tgt.extra { c.arg(a); }
        c
    };
    println!("R target={bin} extra_args={:?} iters={iters}", tgt.extra);
    let spawn_fn = resolve_posix_spawn();
    println!("R posix_spawn_resolved={:?}", spawn_fn.is_some());

    // Warm-up, and prove each backend really produces a running child.
    for _ in 0..3 { let _ = mk().spawn(); }

    // 1. std::process::Command, exactly as the kill path uses it today.
    want(iters);
    let mut s1 = Vec::with_capacity(iters);
    let mut max_fd_std = 0;
    for _ in 0..iters {
        let before = fd_count();
        let t = Instant::now();
        let child = match mk().spawn() {
            Ok(c) => c,
            Err(e) => {
                println!("R std::process::Command spawn failed: {e}");
                break;
            }
        };
        let dt = t.elapsed().as_secs_f64() * 1e6;
        let during = fd_count();
        s1.push(dt);
        max_fd_std = max_fd_std.max(during);
        let _ = before;
        reap(child.id());
    }
    stats("std::process::Command::spawn", s1, &format!("fds_during_spawn={max_fd_std}"));

    // 2. fork + execve by hand. No socketpair, no $PATH search, no allocation.
    want(iters);
    let mut s2 = Vec::with_capacity(iters);
    let devnull = open_devnull();
    for _ in 0..iters {
        let t = Instant::now();
        let pid = unsafe { fork_exec(devnull.as_raw_fd(), pathp, argv.as_ptr()) };
        let dt = t.elapsed().as_secs_f64() * 1e6;
        s2.push(dt);
        reap_wait(pid);
    }
    stats("libc::fork + execve", s2, "child: dup2 /dev/null, execve, _exit");

    // 3. Bionic's posix_spawn (CLONE_VM|CLONE_VFORK on API 28+), NULL file_actions.
    match spawn_fn {
        Some(spawn) => {
            want(iters);
            let mut s3 = Vec::with_capacity(iters);
            let mut max_fd_ps = 0;
            for _ in 0..iters {
                let before = fd_count();
                let t = Instant::now();
                let mut pid: libc::pid_t = 0;
                let rc = unsafe {
                    spawn(&mut pid, pathp, core::ptr::null(), core::ptr::null(), argv.as_ptr(), environ)
                };
                let dt = t.elapsed().as_secs_f64() * 1e6;
                let during = fd_count();
                if rc != 0 { panic!("posix_spawn failed rc={rc}"); }
                s3.push(dt);
                max_fd_ps = max_fd_ps.max(during);
                reap_wait(pid);
                let _ = before;
            }
            stats("posix_spawn (NULL file_actions)", s3, &format!("fds_during={max_fd_ps}"));
        }
        None => println!("R posix_spawn unavailable on this libc"),
    }

    // 4. The production shape: posix_spawn with dup2 actions onto a cached
    //    O_RDWR|O_CLOEXEC /dev/null, i.e. what Gate 4 would actually run.
    if let (Some(spawn), Some(mut fa)) = (spawn_fn, FileActions::resolve()) {
        let devnull = unsafe {
            libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC)
        };
        if devnull < 0 { panic!("open /dev/null failed"); }
        want(iters);
        let mut s4 = Vec::with_capacity(iters);
        let mut rc_seen = 0;
        for _ in 0..iters {
            let t = Instant::now();
            let mut pid: libc::pid_t = 0;
            let r_init = unsafe { (fa.init)(&mut fa.obj) };
            if r_init != 0 { panic!("file_actions_init rc={r_init}"); }
            for fd in 0..3 {
                let r = unsafe { (fa.adddup2)(&mut fa.obj, devnull, fd) };
                if r != 0 { panic!("adddup2 {fd} rc={r}"); }
            }
            let rc = unsafe {
                spawn(&mut pid, pathp, &fa.obj as *const _ as *const libc::c_void, core::ptr::null(), argv.as_ptr(), environ)
            };
            let _ = unsafe { (fa.destroy)(&mut fa.obj) };
            let dt = t.elapsed().as_secs_f64() * 1e6;
            if rc != 0 { rc_seen += 1; }
            s4.push(dt);
            reap_wait(pid);
        }
        stats("posix_spawn + dup2 actions", s4, &format!("devnull_fd={devnull} nonzero_rc={rc_seen}"));
    } else {
        println!("R posix_spawn_file_actions_* unresolved -> skipped");
    }

    // 4b/4c. Is the actions penalty fixed, or per dup2? Empty actions and 1 action.
    if let (Some(spawn), Some(mut fa)) = (spawn_fn, FileActions::resolve()) {
        let devnull = unsafe { libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC) };
        for (label, n_actions) in [("posix_spawn + EMPTY actions", 0usize), ("posix_spawn + 1 dup2 action", 1), ("posix_spawn + 2 dup2 actions", 2)] {
            let mut fails = 0usize;
            want(iters);
            let mut v = Vec::with_capacity(iters);
            for _ in 0..iters {
                let t = Instant::now();
                let mut pid: libc::pid_t = 0;
                unsafe { (fa.init)(&mut fa.obj) };
                for fd in 0..n_actions as i32 { unsafe { (fa.adddup2)(&mut fa.obj, devnull, fd) }; }
                let rc = unsafe { spawn(&mut pid, pathp, &fa.obj as *const _ as *const libc::c_void, core::ptr::null(), argv.as_ptr(), environ) };
                let _ = unsafe { (fa.destroy)(&mut fa.obj) };
                let dt = t.elapsed().as_secs_f64() * 1e6;
                if rc != 0 { fails += 1; } else { v.push(dt); }
                reap_wait(pid);
            }
            stats(label, v, &format!("nonzero_rc={fails}"));
        }
    }

    // 5. Baseline: the timed window with no child at all, to show harness overhead.
    want(0); // a harness baseline may legitimately have any count; do not flag it
    let mut s4 = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        std::hint::black_box(&t);
        s4.push(t.elapsed().as_secs_f64() * 1e6);
    }
    stats("Instant::now() baseline", s4, "");
}

// --- raw clone(CLONE_VM|CLONE_VFORK): a spawner that calls no libc at all ---
//
// The child is hand-written aarch64 assembly: three dup3 syscalls, one execve,
// one exit_group, and nothing else. It runs on its own 4 KB stack, reads one
// static argument block, writes two timestamps, and never calls a function that
// could allocate, take a lock, or trip a stack canary. That is precisely the
// property Bionic's posix_spawn cannot rely on for its own child -- which calls
// calloc/strdup/free and carries a __stack_chk_fail frame -- and is why Bionic
// downgrades to fork() the moment file actions are present.
#[cfg(target_arch = "aarch64")]
mod raw {
    use core::arch::asm;

    pub const CLONE_VM: usize = 0x0000_0100;
    pub const CLONE_VFORK: usize = 0x0000_4000;
    pub const SIGCHLD: usize = 17;

    const NR_DUP3: usize = 24;
    const NR_CLOCK_GETTIME: usize = 113;
    const NR_CLONE: usize = 220;
    const NR_EXECVE: usize = 221;
    const NR_EXIT_GROUP: usize = 94;
    const CLOCK_MONOTONIC: usize = 1;

    /// 4 KiB, 16-byte aligned as the aarch64 ABI requires. The stack grows down,
    /// so newsp = base + 4096. The parent never touches it.
    pub static mut CHILD_STACK: [u8; 4096] = [0; 4096];

    /// One block of shared memory serves as (a) the child's argument source and
    /// (b) its clock readings, so the child needs exactly one live register: the
    /// base pointer. Offsets are fixed by the asm below; do not reorder.
    ///   [0] devnull fd   [1] path   [2] argv   [3] envp
    ///   [4] entry tv_sec     [5] entry tv_nsec
    ///   [6] pre-execve tv_sec [7] pre-execve tv_nsec
    const A_DEVNULL: usize = 0;
    const A_PATH: usize = 8;
    const A_ARGV: usize = 16;
    const A_ENVP: usize = 24;
    const A_T_ENTRY: usize = 32;
    const A_T_EXEC: usize = 48;

    /// The `asm!` string bakes these numbers in as immediates (an inline asm
    /// literal cannot name a Rust constant), so bind them here: a change to a
    /// constant that disagrees with the string is a compile error, and the types
    /// document which literal belongs to which offset.
    const _: [usize; 11] = [
        NR_DUP3, NR_CLOCK_GETTIME, NR_CLONE, NR_EXECVE, NR_EXIT_GROUP, CLOCK_MONOTONIC,
        A_DEVNULL, A_PATH, A_ARGV, A_ENVP, A_T_ENTRY,
    ];
    // A_T_EXEC is the 48 in `add x1, x19, 48`; the loop above zeroes [4..7].
    const _: usize = A_T_EXEC;

    #[repr(C, align(16))]
    pub struct Args([u64; 8]);
    pub static mut ARGS: Args = Args([0; 8]);

    /// Parent-side reading of the same clock the child stamps with.
    pub fn mono_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
    }

    /// Clone flags used by the most recent call, for the report line.
    pub static mut LAST_FLAGS: usize = 0;

    /// Spawn `path` with `argv`/`envp`, fd 0/1/2 pointed at `devnull`, via
    /// clone(CLONE_VM|CLONE_VFORK|SIGCHLD). In the parent this returns the child
    /// pid (or a negative errno); the child never returns.
    ///
    /// `no_fork` drops CLONE_VFORK, keeping the cheap shared address space but
    /// letting the parent run on. That is unsound in general -- the child reads
    /// parent memory while the parent mutates it -- and exists only as a control
    /// to price what the suspension itself costs.
    ///
    /// Safety: `path`, `argv` and `envp` must stay valid and unmodified until the
    /// child reaches execve, because with CLONE_VM the child reads the parent's
    /// live memory and the parent is suspended, not running.
    pub unsafe fn spawn(
        devnull: i32,
        path: *const libc::c_char,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
        no_fork: bool,
    ) -> libc::pid_t {
        let flags = CLONE_VM | SIGCHLD | if no_fork { 0 } else { CLONE_VFORK };
        let base = core::ptr::addr_of_mut!(ARGS).cast::<u64>();
        unsafe {
            core::ptr::write_volatile(base.add(A_DEVNULL / 8), devnull as u64);
            core::ptr::write_volatile(base.add(A_PATH / 8), path as u64);
            core::ptr::write_volatile(base.add(A_ARGV / 8), argv as u64);
            core::ptr::write_volatile(base.add(A_ENVP / 8), envp as u64);
            for i in 0..4 {
                core::ptr::write_volatile(base.add((A_T_ENTRY / 8) + i), 0);
            }
            core::ptr::write_volatile(core::ptr::addr_of_mut!(LAST_FLAGS), flags);
        }
        let stack_top = unsafe { core::ptr::addr_of_mut!(CHILD_STACK).add(4096) };
        let ret: isize;
        unsafe {
            asm!(
                "svc #0",
                // Parent: x0 = child pid. Child: x0 = 0, on its own stack.
                "cbz x0, 9f",
                "b   8f",
                // ---- child: no libc, no heap, no locks, parent suspended ----
                "9:",
                // clock_gettime(CLOCK_MONOTONIC, &ARGS[A_T_ENTRY])
                "mov x8, 113",
                "mov x0, 1",
                "add x1, {base}, 32",
                "svc #0",
                // dup3(devnull, fd, 0) for fd 0, 1, 2. dup3 returns EINVAL when
                // oldfd == newfd, so each redirect is skipped if it matches.
                "mov x8, 24",
                "ldr x2, [{base}]",
                "mov x0, x2",
                "mov x1, 0",
                "mov x2, 0",
                "cmp x0, x1",
                "b.eq 1f",
                "svc #0",
                "1:",
                "mov x8, 24",
                "ldr x0, [{base}]",
                "mov x1, 1",
                "mov x2, 0",
                "cmp x0, x1",
                "b.eq 2f",
                "svc #0",
                "2:",
                "mov x8, 24",
                "ldr x0, [{base}]",
                "mov x1, 2",
                "mov x2, 0",
                "cmp x0, x1",
                "b.eq 3f",
                "svc #0",
                "3:",
                // clock_gettime(CLOCK_MONOTONIC, &ARGS[A_T_EXEC])
                "mov x8, 113",
                "mov x0, 1",
                "add x1, {base}, 48",
                "svc #0",
                // execve(ARGS[A_PATH], ARGS[A_ARGV], ARGS[A_ENVP])
                "mov x8, 221",
                "ldr x0, [{base}, 8]",
                "ldr x1, [{base}, 16]",
                "ldr x2, [{base}, 24]",
                "svc #0",
                // execve failed -> exit_group(127), the shell's convention.
                "mov x0, 127",
                "mov x8, 94",
                "svc #0",
                // exit_group never returns; spin rather than fall into Rust.
                "4:",
                "b 4b",
                "8:",
                // The only register the child needs to survive the syscalls (any
                // callee-saved one; the kernel restores it with pt_regs). x0-x17
                // and x30 are declared dead below, so the allocator must pick
                // from x18..x29, and the child reads its arguments only through
                // this pointer.
                base = in(reg) base as usize,
                in("x1") stack_top as usize,
                in("x2") 0usize,
                in("x3") 0usize,
                in("x4") 0usize,
                inlateout("x8") NR_CLONE => _,
                inlateout("x0") flags => ret,
                out("x5") _, out("x6") _, out("x7") _,
                out("x9") _, out("x10") _, out("x11") _, out("x12") _,
                out("x13") _, out("x14") _, out("x15") _, out("x16") _,
                out("x17") _, out("x30") _,
                options(nostack)
            );
        }
        ret as libc::pid_t
    }

    /// Child clock readings from the last spawn, as microseconds after the
    /// parent's pre-clone reading. Zero means the child never got there.
    pub unsafe fn child_offsets(t0: u64) -> (f64, f64) {
        let base = core::ptr::addr_of!(ARGS).cast::<u64>();
        let rd = |i: usize| unsafe { core::ptr::read_volatile(base.add(i)) };
        let entry = rd(4) * 1_000_000_000 + rd(5);
        let exec = rd(6) * 1_000_000_000 + rd(7);
        let us = |v: u64| {
            if v == 0 || v < t0 {
                0.0
            } else {
                (v - t0) as f64 / 1e3
            }
        };
        (us(entry), us(exec))
    }
}

/// One raw dispatch plus the three-way timing decomposition: parent stall, and
/// how much of the child's path to execve it covers.
#[cfg(target_arch = "aarch64")]
unsafe fn raw_vfork_once(
    devnull: i32,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    no_fork: bool,
) -> (libc::pid_t, [f64; 3]) {
    let t0 = raw::mono_ns();
    let pid = unsafe { raw::spawn(devnull, path, argv, envp, no_fork) };
    let t1 = raw::mono_ns();
    let (entry, exec) = unsafe { raw::child_offsets(t0) };
    let stall = (t1 - t0) as f64 / 1e3;
    (pid, [stall, entry, exec])
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn raw_vfork_once(
    _devnull: i32,
    _path: *const libc::c_char,
    _argv: *const *const libc::c_char,
    _envp: *const *const libc::c_char,
    _no_fork: bool,
) -> (libc::pid_t, [f64; 3]) {
    (-1, [0.0, 0.0, 0.0])
}

/// posix_spawnattr_t, resolved the same way as the file_actions symbols.
/// Bionic's signature is (posix_spawnattr_t*, short) -- flags is a `short`, not
/// an `int`, so passing the wrong width is a real ABI hazard.
type AttrInit = unsafe extern "C" fn(*mut *mut libc::c_void) -> libc::c_int;
type AttrSetFlags = unsafe extern "C" fn(*mut *mut libc::c_void, libc::c_short) -> libc::c_int;
type AttrGetFlags = unsafe extern "C" fn(*const *mut libc::c_void, *mut libc::c_short) -> libc::c_int;
type AttrDestroy = unsafe extern "C" fn(*mut *mut libc::c_void) -> libc::c_int;
const POSIX_SPAWN_USEVFORK: libc::c_short = 0x40;

struct SpawnAttr {
    init: AttrInit,
    setflags: AttrSetFlags,
    getflags: AttrGetFlags,
    destroy: AttrDestroy,
    obj: *mut libc::c_void,
}

impl SpawnAttr {
    fn resolve() -> Option<Self> {
        unsafe {
            let init = sym(b"posix_spawnattr_init\0")?;
            let setflags = sym(b"posix_spawnattr_setflags\0")?;
            let getflags = sym(b"posix_spawnattr_getflags\0")?;
            let destroy = sym(b"posix_spawnattr_destroy\0")?;
            Some(SpawnAttr {
                init: core::mem::transmute(init),
                setflags: core::mem::transmute(setflags),
                getflags: core::mem::transmute(getflags),
                destroy: core::mem::transmute(destroy),
                obj: core::ptr::null_mut(),
            })
        }
    }
}

fn reap(pid: u32) {
    reap_wait(pid as libc::pid_t);
}

fn reap_wait(pid: libc::pid_t) {
    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// fork + execve with only async-signal-safe calls on the child side.
unsafe fn fork_exec(devnull_fd: i32, path: *const libc::c_char, argv: *const *const libc::c_char) -> libc::pid_t {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::dup2(devnull_fd, 0);
            libc::dup2(devnull_fd, 1);
            libc::dup2(devnull_fd, 2);
            libc::execve(path, argv, environ);
            libc::_exit(127);
        }
    }
    pid
}
