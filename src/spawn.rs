//! How the daemon launches `/system/bin/cmd activity kill`, and why that deserves a module.
//!
//! Until now the dispatch was a `std::process::Command`, which costs 2.9-4.7 ms of reactor stall
//! on the reference device - the full spread that three captures of the same backend produced
//! (roadmap 3.1, 9), quoted as a spread because §9's own rule is that `std` may not be quoted as a
//! single number. `docs/ROADMAP.md` measured three ways out of that and this file ships one of
//! them:
//!
//!   * [`Backend::Fork`] - `fork` plus three `dup2` plus `execve`. One syscall we are certain
//!     about, then work confined to a child that owns its own copy of our address space. Measured
//!     1.6 ms against std's 4.7 ms at the daemon's live footprint (roadmap 3.1) - one capture, and
//!     roadmap 9 records what three captures of the same mode did to both numbers, so the gap to
//!     `Std` (always ≥ 2x) is the claim and these absolutes are not.
//!   * [`Backend::Std`] - exactly what shipped in v1.4.0, kept reachable through `MINI_LMK_SPAWN`
//!     so a release can be A/B'd against its predecessor on a device we cannot reflash (D3).
//!
//! A third option was built, measured on device, and deleted. `posix_spawn` carrying three
//! `adddup2` actions with `POSIX_SPAWN_USEVFORK` measured 240 us - 1.17x - cheaper than `fork` at
//! the shipping footprint, and flat in resident set where `fork` scales. What it could not be was
//! the only path: `posix_spawn_file_actions_adddup2` and `posix_spawnattr_setflags` are
//! `__INTRODUCED_IN(28)` while this project links at API 24, so honouring it meant shipping
//! *both* backends forever - eight `dlsym` lookups, an `RTLD_DEFAULT` that has to be right on four
//! ABIs, hand-sized storage for two opaque types (`sizeof(posix_spawnattr_t)` is 336 bytes on the
//! host CI builds and pointer-width on Bionic, so the size cannot be checked at compile time on
//! the target where it matters), and a POSIX-deprecated flag whose behaviour has to be re-verified
//! against every Bionic release. For a saving whose sign was not reproducible across captures
//! (roadmap 9), that trade is not worth making. Roadmap 5.4 keeps the measurements that killed it,
//! and `tools/spawnprobe` keeps a working implementation of it that CI still builds.
//!
//! Three rules govern what is left. Exec failure is *asynchronous*: `fork` returns success for a
//! binary that then fails to `exec`, so a missing `/system/bin/cmd` surfaces as the child's exit
//! status 127 and never reaches the recorded errno (6.2) - which is why that errno is documented
//! as pre-clone only. The child's stdio must be redirected, because without the three `dup2`s a
//! Java stack trace from AMS lands on the service's stdout. And the child's descriptor table must
//! be *swept*, because `fork` copies ours wholesale - including descriptors our launcher left
//! unmarked, which `std::process::Command` closes and we would otherwise hand to `cmd` (10).
//!
//! Dropping `posix_spawn` also removed the hardest constraint the roadmap carried. `USEVFORK`
//! forks with `CLONE_VM`, so the child read our `argv`, `envp` and actions object out of our own
//! memory while we were suspended, and the daemon's single-threaded invariant was load-bearing for
//! that reason. `fork` hands the child a copy: nothing in the child can observe a concurrent writer
//! here, and this module no longer has to keep a table alive across a suspension.

use std::ffi::{CStr, OsStr};
use std::io::Error as IoError;
use std::os::raw::{c_char, c_int};
use std::os::unix::ffi::OsStrExt;

/// The package-kill argv, fixed at compile time so the dispatch allocates no strings.
const CMD_PATH: &CStr = c"/system/bin/cmd";
const KILL_ARGS: [&CStr; 4] = [c"activity", c"kill", c"--user", c"all"];

/// `spawn_kill`'s return value when the caller never asked for a dispatch at all.
///
/// The telemetry field this feeds distinguishes four states a boolean cannot: not attempted
/// (observe mode, reported as JSON `null`), skipped by the AMS guard (this value), attempted and
/// launched (0), and attempted and refused (an errno, or -1 from this module). Kept at -2 so it can
/// never collide with a positive OS errno or with -1, which
/// [`Spawner::spawn_kill`] uses for "refused, but not by the kernel".
pub const NOT_DISPATCHED: c_int = -2;

/// The sentinel's whole purpose is not colliding with anything, and "not colliding" is arithmetic
/// on constants - so check it at compile time, where a change cannot slip past a test run.
const _: () = assert!(NOT_DISPATCHED < -1);

/// `cmd activity kill --user all <pkg>` needs 7 slots (path + 4 + pkg + NULL); the eighth is
/// headroom, and [`Spawner::spawn_path`] refuses a list that does not fit rather than handing the
/// child a truncated argument.
const ARGV_SLOTS: usize = 8;
const ARGV_FIXED: usize = 2; // path and NULL terminator
/// The runtime check in [`Spawner::spawn_path`] refuses an over-long argv; this refuses it *at
/// compile time*. `ARGV_FIXED` already covers the path and the `NULL`, so the remaining `+ 1` is
/// the dynamic package slot: `2 + 4 + 1 <= 8`, written as a strict `<` because clippy reads the
/// two as the same fact. Asserting `ARGV_SLOTS > ARGV_FIXED` instead would have held forever while
/// a fifth `KILL_ARGS` entry made every real dispatch fail silently with `Other`.
const _: () = assert!(ARGV_FIXED + KILL_ARGS.len() < ARGV_SLOTS);

// `libc` declares `environ` for Linux but not for Android, where omitting it is an `error[E0425]`
// (roadmap 6.4). One hand-written binding is correct on both: it declares the same C symbol rather
// than defining it, and declaring it non-`mut` is what `libc` itself does for glibc, which also
// keeps the `static_mut_refs` lint out of the way. (`//`, not `///`: rustdoc generates no
// documentation for an extern block, and an unused doc comment is a warning.)
extern "C" {
    static environ: *const *const c_char;
}

/// Which launcher a dispatch uses. Chosen once, at startup, and printed in the banner (D4) so the
/// log states which kill path is live before the first kill has to imply it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Std,
    Fork,
}

impl Backend {
    /// `MINI_LMK_SPAWN` is a measurement surface, not a user feature (D3). Anything unrecognised -
    /// including unset, which is the normal case - resolves to `Fork`. The unknown set now includes
    /// `"spawn"`, which selected the deleted backend: a typo or a leftover environment must not be
    /// able to ask for a path this binary no longer contains.
    pub fn parse(value: Option<&str>) -> Backend {
        match value {
            Some("std") => Backend::Std,
            _ => Backend::Fork,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Std => "std",
            Backend::Fork => "fork",
        }
    }
}

/// Was `MINI_LMK_SPAWN` set to something that is not a backend? `None` for unset and for every
/// recognised value; `Some(text)` echoing the operator's own spelling otherwise.
///
/// This cannot be reported by [`Spawner::new`]: by the time a `Backend` has been constructed the
/// spelling is already gone, so a request for the deleted `spawn` path would arrive as a perfectly
/// healthy `fork` and the banner would read like the operator had asked for nothing. An ignored
/// request has to be as loud as a failing one (roadmap 9).
pub fn unrecognised_backend(value: Option<&str>) -> Option<String> {
    match value {
        None => None,
        Some(text) if Backend::parse(Some(text)).name() == text => None,
        Some(text) => Some(text.to_string()),
    }
}

/// Why a dispatch did not happen. `Errno` carries a *pre-clone* OS error; `Other` is a refusal by
/// this module's own checks (an argument list that does not fit `ARGV_SLOTS`).
#[derive(PartialEq, Eq, Debug)]
pub enum SpawnError {
    Errno(c_int),
    Other,
}

/// The chosen launcher, its cached `/dev/null` descriptor, and the one buffer whose contents are
/// not a compile-time constant. Construction is the only place that allocates or opens a
/// descriptor; a steady-state dispatch does neither.
pub struct Spawner {
    backend: Backend,
    note: Option<&'static str>,
    devnull: c_int,
    /// The errno from a failed `/dev/null` open, or 0. While it is nonzero the `fork` path has no
    /// descriptor to `dup2`, so it refuses itself rather than launching a child that would die in
    /// `dup2` and be recorded as a success. `Std` is not affected: it opens its own.
    devnull_errno: c_int,
    envp: Vec<*const c_char>,
    /// Owned storage for the package name, the only argument whose text is not static. It grows
    /// only when a longer name than any seen before arrives, so the steady state allocates nothing
    /// which is the claim roadmap 8 makes about this module, executed by
    /// `tests::staged_argv_is_exact_and_a_shorter_name_never_reallocates`.
    name_buf: Vec<u8>,
}

impl Spawner {
    /// Resolve the backend for `requested`, degrading to `Std` if not even a descriptor can be
    /// obtained, and recording why. Never panics: a daemon that cannot open `/dev/null` still has
    /// to evict something, and `Std` is the path that was always there.
    pub fn new(requested: Backend) -> Spawner {
        let mut spawner = Spawner {
            backend: requested,
            note: None,
            devnull: -1,
            devnull_errno: 0,
            envp: Vec::new(),
            name_buf: Vec::new(),
        };

        // The environment is copied once, as a pointer table; the strings stay owned by libc.
        // Roadmap 7.6 defers shrinking or emptying it: worth about 90 us today, and its safety
        // argument is per-target, which is the weakest kind there is.
        let mut envp = Vec::new();
        let mut cursor: *const *const c_char = unsafe { environ };
        if !cursor.is_null() {
            while unsafe { !(*cursor).is_null() } {
                envp.push(unsafe { *cursor });
                cursor = unsafe { cursor.add(1) };
            }
        }
        envp.push(std::ptr::null());
        spawner.envp = envp;

        // One descriptor for the process lifetime, opened before the reactor exists (D5). It is
        // `O_CLOEXEC` so the cached descriptor itself never reaches the child; `dup2` clears the
        // flag on the three descriptors we deliberately point at it.
        let fd =
            unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0o666) };
        if fd < 0 {
            spawner.devnull_errno = IoError::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EMFILE);
            spawner.note = Some("cannot open /dev/null; falling back to std::process::Command");
            spawner.backend = Backend::Std;
            return spawner;
        }
        spawner.devnull = fd;
        spawner
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Why the requested backend was not taken, if it was not.
    pub fn note(&self) -> Option<&'static str> {
        self.note
    }

    /// The cached `/dev/null` descriptor, for the tests that check what a child actually got.
    #[cfg(test)]
    fn devnull(&self) -> c_int {
        self.devnull
    }

    /// Dispatch `/system/bin/cmd activity kill --user all <pkg>`.
    ///
    /// Returns the errno to record: `0` when the child was launched, a positive OS errno when the
    /// kernel refused *synchronously*, -1 for a refusal by this module. Exec failure is not in that
    /// set: it is asynchronous (6.2) and surfaces only as the child's exit status 127, which this
    /// call site never waits for - waiting is the whole cost we are trying to avoid.
    pub fn spawn_kill(&mut self, pkg: &str) -> c_int {
        match self.spawn_path(CMD_PATH, &KILL_ARGS, Some(pkg)) {
            Ok(_) => 0,
            Err(SpawnError::Errno(errno)) => errno,
            Err(SpawnError::Other) => -1,
        }
    }

    /// Launch `path` with `args` plus one trailing dynamic argument, returning the child pid.
    ///
    /// Separate from [`Spawner::spawn_kill`] so the dispatcher can be tested against any binary
    /// instead of only against `cmd`, which on a device would be a real kill.
    pub fn spawn_path(
        &mut self,
        path: &CStr,
        args: &[&CStr],
        tail: Option<&str>,
    ) -> Result<c_int, SpawnError> {
        // Rejected before anything is launched: a NUL cannot survive an `execve` argv, so a caller
        // that slipped one through would otherwise get a *successful* dispatch of a truncated
        // argument. `Command` used to return `InvalidInput` for us here.
        if let Some(text) = tail {
            if text.as_bytes().contains(&0) {
                return Err(SpawnError::Errno(libc::EINVAL));
            }
        }
        let slots_needed = ARGV_FIXED + args.len() + usize::from(tail.is_some());
        if slots_needed > ARGV_SLOTS {
            return Err(SpawnError::Other);
        }
        if self.backend == Backend::Std {
            // std does its own redirection; that is the behaviour v1.4.0 shipped, reproduced here
            // unchanged so the A/B compares dispatchers and not semantics. It is also the path
            // that still works when our own `/dev/null` open failed, which is why this check
            // belongs below and not above it.
            return self.spawn_std(path, args, tail);
        }
        if self.devnull_errno != 0 {
            return Err(SpawnError::Errno(self.devnull_errno));
        }
        let mut argv: [*const c_char; ARGV_SLOTS] = [std::ptr::null(); ARGV_SLOTS];
        self.stage(&mut argv, path, args, tail);
        self.spawn_fork(&argv)
    }

    /// Fill `argv`. Takes nothing to return because the only failure it could have - not fitting -
    /// is checked by the caller before any state is touched.
    fn stage(
        &mut self,
        argv: &mut [*const c_char; ARGV_SLOTS],
        path: &CStr,
        args: &[&CStr],
        tail: Option<&str>,
    ) {
        argv[0] = path.as_ptr();
        for (i, arg) in args.iter().enumerate() {
            argv[1 + i] = arg.as_ptr();
        }
        let mut used = 1 + args.len();
        if let Some(text) = tail {
            self.name_buf.clear();
            self.name_buf.extend_from_slice(text.as_bytes());
            self.name_buf.push(0);
            argv[used] = self.name_buf.as_ptr().cast();
            used += 1;
        }
        debug_assert!(used < ARGV_SLOTS);
        argv[used] = std::ptr::null();
    }

    /// D2, now the shipped path. The child does exactly three `dup2`s and one `execve`, all from
    /// its own copy of our stack, and `_exit(127)`s if any of them fails - a status, never an
    /// errno, because by then the parent has already been told the dispatch succeeded (6.2).
    fn spawn_fork(&self, argv: &[*const c_char; ARGV_SLOTS]) -> Result<c_int, SpawnError> {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(SpawnError::Errno(
                IoError::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EAGAIN),
            ));
        }
        if pid == 0 {
            for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                if unsafe { libc::dup2(self.devnull, fd) } < 0 {
                    unsafe { libc::_exit(127) };
                }
            }
            unsafe {
                close_inherited(self.devnull);
                libc::execve(argv[0], argv.as_ptr(), self.envp.as_ptr());
            }
            unsafe { libc::_exit(127) };
        }
        Ok(pid)
    }

    /// D3: v1.4.0's dispatch, byte for byte in behaviour, so a regression can be attributed to
    /// this module and not to a change in what we ask `cmd` to do.
    fn spawn_std(
        &self,
        path: &CStr,
        args: &[&CStr],
        tail: Option<&str>,
    ) -> Result<c_int, SpawnError> {
        let mut cmd = std::process::Command::new(OsStr::from_bytes(path.to_bytes()));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for arg in args {
            cmd.arg(OsStr::from_bytes(arg.to_bytes()));
        }
        if let Some(text) = tail {
            // No `OsStr::from_bytes` here, unlike the two above: `tail` is a `&str`, so the
            // bytes are already the OS string. The package name is validated for NUL before
            // it gets here (roadmap 6.5), and a `&str` cannot contain one anyway.
            cmd.arg(text);
        }
        match cmd.spawn() {
            Ok(child) => Ok(child.id() as c_int),
            Err(err) => Err(SpawnError::Errno(
                err.raw_os_error().unwrap_or(libc::EINVAL),
            )),
        }
    }
}

/// Close every descriptor the child inherited besides the three it was just
/// given.
///
/// `fork()` copies the entire descriptor table, CLOEXEC bits and all. The daemon
/// marks every descriptor *it* opens as close-on-exec -- the epoll and inotify
/// handles, the log, its `/proc` paths, our own cached `/dev/null` -- so those
/// would not reach `cmd` anyway. What it cannot control is what its launcher
/// handed it: a Magisk service pipe, an adb socket, whatever `sh` still had
/// open. Those arrive unmarked, and `std::process::Command` -- the `Std`
/// backend, and v1.4.0's only dispatch path -- closes them on our behalf.
/// Without this, the shipped fork path would differ from v1.4.0 in exactly one
/// way, and it is the way that can pin somebody else's socket open in the
/// daemon's name until the dispatched binary exits. Roadmap section 10.
///
/// Runs in the child before `execve`, so the reactor never waits for it: the
/// parent returns from `fork()` while this is still executing.
///
/// `probe` is our own cached `/dev/null` descriptor, used to *check* that a
/// fast path really closed things rather than to assume it. That is what makes
/// the hardcoded syscall number below safe: a number that named some other
/// syscall could still return 0, and would then be caught by the probe instead
/// of silently shipping a sweep that never happened.
///
/// # Safety
///
/// Called only in the forked child. `close`, `getrlimit` and `fcntl` are
/// async-signal-safe, and the descriptor numbers are this child's own copy of
/// the table.
unsafe fn close_inherited(_probe: c_int) {
    // One syscall wherever the kernel has it. close_range(2) is Linux 5.9+, and Bionic exports it
    // as a function only from API 34 -- well above the API 24 floor this project links at (the
    // *-android24-clang wrappers in .cargo/config.toml) -- so the number is issued directly: 436,
    // the asm-generic number, identical on aarch64 and x86_64.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        const SYS_CLOSE_RANGE: libc::c_long = 436;
        if libc::syscall(SYS_CLOSE_RANGE, 3_u32, u32::MAX, 0_u32) == 0
            && libc::fcntl(_probe, libc::F_GETFD) < 0
        {
            return;
        }
    }
    // Older kernel: walk the range this process may hold open, blindly (closing
    // a descriptor that was never open is a harmless EBADF), bounded by the hard
    // limit and further capped so a child can never make thousands of syscalls
    // before dispatching a kill.
    let mut lim: libc::rlimit = std::mem::zeroed();
    let hard = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
        lim.rlim_max as usize
    } else {
        256
    };
    for fd in 3..hard.min(4096) {
        libc::close(fd as libc::c_int);
    }
}

impl Drop for Spawner {
    fn drop(&mut self) {
        if self.devnull >= 0 {
            unsafe { libc::close(self.devnull) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both binaries exist on every target that runs these tests: `/bin/true` and `/bin/sleep` on
    // the CI host, their `/system/bin` equivalents on Android. A test never touches `cmd`, because
    // on a device that would be a real kill.
    #[cfg(not(target_os = "android"))]
    const TRUE: &CStr = c"/bin/true";
    #[cfg(not(target_os = "android"))]
    const SLEEP: &CStr = c"/bin/sleep";
    #[cfg(target_os = "android")]
    const TRUE: &CStr = c"/system/bin/true";
    #[cfg(target_os = "android")]
    const SLEEP: &CStr = c"/system/bin/sleep";

    /// Block until the child has replaced its image with `binary`.
    ///
    /// Right after `fork` the child is still wearing the parent's descriptors, so
    /// any inspection of `/proc/<pid>/fd` races the three `dup2`s and the sweep
    /// that closes what they inherit - and because the test harness's own stdin is
    /// already `/dev/null`, that race can report either a leak that never happened
    /// or a fix that landed one syscall later. `cmdline` is only rewritten by
    /// `execve`, so watching for the binary we asked for is a real barrier, not a
    /// sleep (roadmap 6.5).
    fn wait_for_exec(pid: c_int, binary: &CStr) {
        let cmdline = format!("/proc/{pid}/cmdline");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::read(&cmdline) {
                Ok(bytes)
                    if bytes.split(|b| *b == 0).next().unwrap_or_default() == binary.to_bytes() =>
                {
                    return;
                }
                _ if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => panic!(
                    "child {pid} never exec'd {}: {other:?}",
                    binary.to_str().unwrap()
                ),
            }
        }
    }

    /// Reap `pid` and return its raw wait status. Every test that launches a child has to reap it,
    /// or the suite leaves zombies behind in a process that outlives the test.
    fn reaped(pid: c_int) -> c_int {
        let mut status: c_int = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
            if IoError::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        status
    }

    fn exited_with(status: c_int) -> Option<c_int> {
        if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        }
    }

    #[test]
    fn backend_parse_treats_anything_unrecognised_as_fork() {
        // D3. The name of the deleted backend belongs in this list on purpose: `spawn` must land on
        // `Fork` rather than on a variant that no longer exists, and if a future change reintroduces
        // it, this test is where the accident gets caught.
        assert_eq!(Backend::parse(Some("std")), Backend::Std);
        assert_eq!(Backend::parse(Some("fork")), Backend::Fork);
        assert_eq!(Backend::parse(Some("spawn")), Backend::Fork);
        for junk in [
            Some(""),
            Some("SPAWN"),
            Some("fork "),
            Some(" posix_spawn"),
            Some("std "),
        ] {
            assert_eq!(
                Backend::parse(junk),
                Backend::Fork,
                "{junk:?} must not upgrade"
            );
        }
        assert_eq!(Backend::parse(None), Backend::Fork);
    }

    #[test]
    fn each_backend_name_is_the_value_that_selects_it() {
        // The startup banner prints `name()`. If a name stopped round-tripping through `parse` the
        // banner would be describing a configuration nobody can ask for, which is worse than
        // silence: an operator reading "fork" must be able to request it.
        for value in ["std", "fork"] {
            let backend = Backend::parse(Some(value));
            assert_eq!(backend.name(), value);
            assert_eq!(Backend::parse(Some(backend.name())), backend);
        }
    }

    #[test]
    fn construction_notes_nothing_it_does_not_have_to() {
        // Both reachable backends are available on every supported target, so a note here would
        // mean a device where the shipped path silently changed under the operator.
        for backend in [Backend::Fork, Backend::Std] {
            let spawner = Spawner::new(backend);
            assert_eq!(
                spawner.backend(),
                backend,
                "{} must not degrade",
                backend.name()
            );
            assert_eq!(
                spawner.note(),
                None,
                "{} should not need a caveat",
                backend.name()
            );
        }
    }

    #[test]
    fn dispatch_launches_a_child_that_exits_cleanly() {
        let mut spawner = Spawner::new(Backend::Fork);
        let pid = spawner
            .spawn_path(TRUE, &[], None)
            .expect("fork dispatch must succeed for a real binary");
        assert!(pid > 0, "fork returned a non-positive pid {pid}");
        assert_eq!(
            exited_with(reaped(pid)),
            Some(0),
            "the child ran the wrong thing"
        );
    }

    #[test]
    fn child_stdio_is_redirected_away_from_the_daemon() {
        // The reason the three `dup2`s exist at all: without them an AMS Java stack trace lands on
        // the service's stdout. Read from the parent through `/proc/<pid>/fd` rather than by asking
        // a shell to `readlink /proc/self/fd/1`, because `self` there is the `readlink` process, not
        // the child we spawned - a probe written that way passes whatever the daemon does (7.4).
        let mut spawner = Spawner::new(Backend::Fork);
        let pid = spawner
            .spawn_path(SLEEP, &[c"5"], None)
            .expect("spawn sleep");
        wait_for_exec(pid, SLEEP);
        let mut observed = Vec::new();
        for fd in ["0", "1", "2"] {
            let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
                .unwrap_or_else(|e| panic!("/proc/{pid}/fd/{fd}: {e}"));
            observed.push(link.to_string_lossy().into_owned());
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = reaped(pid);
        assert_eq!(
            observed,
            vec!["/dev/null".to_string(); 3],
            "the fork backend leaked our stdio"
        );
        assert!(
            spawner.devnull() > 2,
            "cached descriptor must not be a std slot"
        );
    }

    #[test]
    fn the_child_carries_no_descriptor_the_parent_did_not_mark_close_on_exec() {
        // `fork` copies the whole descriptor table, CLOEXEC bits and all. Everything the daemon
        // opens for itself is marked close-on-exec -- epoll, inotify, the log, `/proc`, our own
        // cached `/dev/null` -- so those would not reach `cmd` regardless. What is not marked is
        // whatever the *launcher* handed the daemon: a Magisk service pipe, an adb socket.
        // `std::process::Command` closes those in the child, which is why v1.4.0 never leaked one;
        // this is the single behavioural difference between the shipped fork path and v1.4.0, and
        // `close_inherited` is what erases it.
        let probe = c"/proc/self/status";
        let fd = unsafe { libc::open(probe.as_ptr(), libc::O_RDONLY) };
        assert!(
            fd > 2,
            "raw open returned {fd}, expected a descriptor above stdio"
        );
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags >= 0 && flags & libc::FD_CLOEXEC == 0,
            "the descriptor this test opens must be inherited, not close-on-exec"
        );

        let mut spawner = Spawner::new(Backend::Fork);
        let pid = spawner
            .spawn_path(SLEEP, &[c"5"], None)
            .expect("spawn sleep");
        wait_for_exec(pid, SLEEP);
        let mut survived: Vec<c_int> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| entry.file_name().to_str()?.parse::<c_int>().ok())
                    .filter(|number| *number > 2)
                    .collect()
            })
            .unwrap_or_default();
        survived.sort_unstable();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = reaped(pid);
        unsafe { libc::close(fd) };
        assert!(
            survived.is_empty(),
            "the dispatch child held descriptors {survived:?} open beyond the stdio it was given"
        );
    }

    #[test]
    fn a_nul_in_the_dynamic_argument_is_refused_before_any_child_exists() {
        // `execve` would truncate the argument at the NUL and *succeed*, recording a dispatch of a
        // package nobody asked to kill. Shared by both backends, so the guard lives above them.
        for backend in [Backend::Std, Backend::Fork] {
            let mut spawner = Spawner::new(backend);
            assert_eq!(
                spawner.spawn_path(TRUE, &[], Some("com.\0evil")),
                Err(SpawnError::Errno(libc::EINVAL)),
                "{} accepted an argument that cannot survive execve",
                backend.name()
            );
        }
    }

    #[test]
    fn staged_argv_is_exact_and_a_shorter_name_never_reallocates() {
        // Two claims at once. The first is correctness: the child must see exactly what we staged,
        // NUL-terminated, with the package name in the last slot. The second is roadmap 8's
        // "no allocation inside the dispatch loop": the buffer is sized by the longest name ever
        // seen, so a long package followed by short ones must not reallocate again.
        let mut spawner = Spawner::new(Backend::Fork);
        let mut argv: [*const c_char; ARGV_SLOTS] = [std::ptr::null(); ARGV_SLOTS];
        let longest = "com.example.a_rather_long_package_name_for_sizing";
        spawner.stage(&mut argv, TRUE, &[c"-n"], Some(longest));
        assert_eq!(
            unsafe { CStr::from_ptr(argv[0]) }.to_str().unwrap(),
            "/bin/true"
        );
        assert_eq!(unsafe { CStr::from_ptr(argv[1]) }.to_str().unwrap(), "-n");
        assert_eq!(
            unsafe { CStr::from_ptr(argv[2]) }.to_str().unwrap(),
            longest
        );
        assert!(argv[3].is_null(), "argv must be NUL terminated");
        assert_eq!(
            spawner.name_buf.last(),
            Some(&0),
            "staged name must be NUL terminated for execve"
        );

        let capacity = spawner.name_buf.capacity();
        for len in 0..longest.len() {
            let name = "a".repeat(len); // strictly shorter than `longest`, every time
            spawner.stage(&mut argv, TRUE, &[], Some(&name));
            assert_eq!(
                unsafe { CStr::from_ptr(argv[1]) }.to_str().unwrap(),
                name,
                "staging a short name wrote the wrong text"
            );
            assert_eq!(
                spawner.name_buf.capacity(),
                capacity,
                "a shorter name reallocated the buffer after dispatch {len}"
            );
        }
    }

    #[test]
    fn an_argument_list_that_does_not_fit_is_refused_not_truncated() {
        let mut spawner = Spawner::new(Backend::Fork);
        let args = [TRUE; ARGV_SLOTS]; // path + 8 args + NUL overflows the 8 available slots
        assert_eq!(
            spawner.spawn_path(TRUE, &args, None),
            Err(SpawnError::Other),
            "overflowing ARGV_SLOTS must never be truncated into a child"
        );
    }

    #[test]
    fn a_missing_binary_reports_synchronously_only_where_the_libc_can() {
        // Roadmap 6.2, encoded as a test instead of a comment because it decides what `spawn_errno`
        // is allowed to mean. `fork` cannot report an exec failure to the parent at all: the parent
        // is told the dispatch succeeded and the child dies with status 127, which this daemon
        // deliberately never waits for (waiting is the cost being removed). std inherits whatever
        // its platform's spawn does - glibc passes the child's errno back through shared memory,
        // Bionic does not - so the synchronous ENOENT below is asserted on the host only.
        let missing = c"/nonexistent/mini-lmk-spawn-probe";
        let mut spawner = Spawner::new(Backend::Fork);
        let pid = spawner
            .spawn_path(missing, &[], None)
            .expect("fork accepts any path");
        assert_eq!(
            exited_with(reaped(pid)),
            Some(127),
            "fork must report exec failure as exit 127, never as an errno"
        );

        let mut spawner = Spawner::new(Backend::Std);
        #[cfg(not(target_os = "android"))]
        assert_eq!(
            spawner.spawn_path(missing, &[], None),
            Err(SpawnError::Errno(libc::ENOENT)),
            "glibc reports exec failure synchronously"
        );
        #[cfg(target_os = "android")]
        if let Ok(pid) = spawner.spawn_path(missing, &[], None) {
            assert_eq!(exited_with(reaped(pid)), Some(127));
        }
    }

    #[test]
    fn dispatch_outcomes_are_the_codes_the_telemetry_documents() {
        // D6's contract, executed instead of restated. The three codes a dispatch can produce must
        // be the ones the field documentation claims, and must stay distinct from the sentinel
        // (whose width is enforced by the `const` assertion above, not by this test):
        //   0    launched        -1  refused by this module    positive errno  refused by the OS
        let mut spawner = Spawner::new(Backend::Fork);
        assert_eq!(
            spawner.spawn_path(TRUE, &[TRUE; ARGV_SLOTS], None),
            Err(SpawnError::Other),
            "an argv that does not fit must be refused, not truncated"
        );
        assert_eq!(
            spawner.spawn_kill("com.example.\0evil"),
            libc::EINVAL,
            "a NUL in the package name must come back as EINVAL"
        );

        // Off-device, where `/system/bin/cmd` does not exist, this is roadmap 6.2 in one line: the
        // dispatch is reported as *successful* even though nothing can run, because `fork` cannot
        // tell its parent about an `exec` failure. The status side of the same fact is covered by
        // `a_missing_binary_reports_synchronously_only_where_the_libc_can`, which reaps the child;
        // this call leaves its child to be reaped at process exit rather than draining the child
        // table, which in a parallel test binary would steal other tests' children.
        #[cfg(not(target_os = "android"))]
        assert_eq!(
            spawner.spawn_kill("com.example.absent"),
            0,
            "an exec failure must never be reported as a dispatch failure"
        );

        // The success code, from the other backend: `Std` returns the child's pid, and the
        // telemetry turns any non-negative into 0. Asserted here so the "0 launched" row of the
        // contract is pinned for both dispatchers, not only for the fork one.
        let mut std_spawner = Spawner::new(Backend::Std);
        #[cfg(not(target_os = "android"))]
        assert!(
            std_spawner.spawn_path(TRUE, &[c"-n"], None).is_ok(),
            "the std backend must also report a launched child as success"
        );
    }

    #[test]
    fn a_failed_devnull_open_downgrades_to_std_rather_than_disabling_dispatch() {
        // The bug this test exists for: the `/dev/null` guard used to sit in front of the `Std`
        // branch, so a daemon that degraded to `Std` at startup then refused every dispatch with
        // the startup errno - forever - even though `Std` opens `/dev/null` itself. `Spawner::new`
        // documents the opposite ("a daemon that cannot open /dev/null still has to evict
        // something"), and D4 counts this as one of fork's silent downgrades.
        let mut spawner = Spawner::new(Backend::Fork);
        spawner.devnull = -1;
        spawner.devnull_errno = libc::EMFILE;
        spawner.backend = Backend::Std;
        assert!(
            spawner.spawn_path(SLEEP, &[c"1"], None).is_ok(),
            "Std must still dispatch when our own /dev/null open failed"
        );

        // The same spawner, on the path that actually needs the descriptor, must refuse with that
        // errno rather than launch a child that dies in `dup2` and is recorded as a success.
        spawner.backend = Backend::Fork;
        assert_eq!(
            spawner.spawn_path(SLEEP, &[c"1"], None),
            Err(SpawnError::Errno(libc::EMFILE)),
            "fork without a /dev/null descriptor must report the refusal, not the attempt"
        );
    }

    #[test]
    fn the_cached_descriptor_is_ours_and_survives_every_dispatch() {
        // Roadmap 8's descriptor clause, in the only form a unit test can carry it.
        //
        // What is *not* asserted here is "the process holds exactly one more descriptor than
        // before": the census is process-wide while this suite runs 57 tests in parallel, so by the
        // time the twenty dispatches finish the unrelated churn around us has drained and the count
        // falls - which reads as a negative leak and fails for reasons that have nothing to do with
        // this module. An earlier version of this test did exactly that, and passed or failed
        // depending on scheduling. The real census belongs where the claim lives, on a quiescent
        // daemon: `scripts/benchmark.sh` samples `/proc/<pid>/fd` and `Threads:` once per
        // iteration and fails the run if min != max (roadmap 7.5). Note that harness runs the
        // daemon in --observe unless given --daemon-mode act, and --observe never reaches this
        // code path, so its default verdict covers the startup descriptor and the thread count
        // only; the per-dispatch half is the identity assertion below.
        //
        // What can be pinned race-free is identity: our descriptor exists, is not one of the three
        // std slots, is stable across dispatches, and still points at `/dev/null`.
        let mut spawner = Spawner::new(Backend::Fork);
        let ours = spawner.devnull();
        assert!(
            ours > 2,
            "the cached descriptor must not occupy a std slot (got {ours})"
        );
        assert_eq!(
            std::fs::read_link(format!("/proc/self/fd/{ours}"))
                .expect("cached descriptor must be open")
                .to_string_lossy(),
            "/dev/null",
            "the cached descriptor must point at /dev/null"
        );
        for _ in 0..20 {
            let pid = spawner
                .spawn_path(TRUE, &[], None)
                .expect("dispatch of a real binary");
            let _ = reaped(pid);
            assert_eq!(spawner.devnull(), ours, "a dispatch changed our descriptor");
            assert!(
                std::path::Path::new(&format!("/proc/self/fd/{ours}")).exists(),
                "a dispatch closed our descriptor"
            );
        }
    }
}
