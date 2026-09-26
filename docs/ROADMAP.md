# v1.5.0 Roadmap — Kill-Dispatch Latency

Status: **plan, no code landed.** This file records what we measured, which backend we
chose, and *why* — including the alternatives we rejected and the measurements that
rejected them. Every number below is from the reference device (Pixel-class API 34
emulator, `u:r:shell:s0`, 7.9 GB `MemTotal`, aarch64) against the real dispatch target
`/system/bin/cmd activity kill`. Where an earlier draft of this roadmap was wrong, the
wrong claim is kept, labelled and corrected rather than quietly deleted: that is the
cheapest way to stop someone re-litigating it. (The first version of this file was
overwritten by a stale-snapshot restore before it was committed, and rebuilt from the same
measurements with the audit corrections applied to §2, §3.1–3.5, §5.9 and §10.)

---

## 1. The problem we are actually fixing

`evaluate_reaping_pipeline()` dispatches evictions with `std::process::Command` on the
daemon's single reactor thread (the `for cand in candidates.into_iter().take(
self.config.max_kills_per_pass)` loop). That call does not return until the kernel has
finished with the child's address space. While it is inside, the reactor cannot read the
logcat pipe, cannot service inotify, and cannot make decisions. The fix shortens that
window and, more importantly, stops it depending on things we do not control.

Two things this is **not**, because both were asserted in earlier pitches and neither
survives measurement:

* **It is not an fd leak.** `std::process::Command::spawn` on Android opens nothing: the
  probe records `fds_during_spawn=4` across 60 dispatches — the daemon's own four
  descriptors, unchanged. The "secret socketpair doubles the FD count" story does not
  reproduce here; a syscall-filtered `strace` of the same path found `clone`, `execve`,
  `wait4` and 379 `rt_sigprocmask`, and **zero** `socket`, `socketpair` or `pipe2`.
* **It is not an animation-budget problem.** This daemon is not the UI thread. A dispatch
  stall delays *our next decision*, not a frame. Anyone quoting 16.6 ms frame budgets for
  this call site has the wrong thread.

The honest framing: **reactor tail latency, plus aligning runtime behaviour with the
documented single-threaded, zero-allocation, no-internal-state invariants**
(`docs/ARCHITECTURE.md` §2). We are paying 4.7 ms to learn nothing.

## 2. The mechanism: two independent costs, and only one of them is ours

Every backend's parent-side stall decomposes into exactly two parts. Separating them is
what the whole decision turns on.

**(a) The page-table copy — the `fork()` tax.** `clone()` without `CLONE_VM` asks the
kernel to duplicate the parent's page tables before the child runs. Its price is a
function of the parent's *resident* memory and nothing else: not the binary we exec, not
the arguments, not the environment. Bionic's `fork()` is 136 bytes of assembly around
`clone(0x01200011)` — `SIGCHLD | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID`, with **no
`CLONE_VM` bit**. Any path that forks pays the copy. This is the cheap-parent principle,
and it is why the daemon's own footprint is a latency input at this call site.

**(b) The suspension window — the `vfork` tax.** `clone(CLONE_VM|CLONE_VFORK)` shares the
address space (so (a) is zero) and halts the parent until the child's `execve` completes.
Its price is the child's trip through the kernel plus the dynamic linker loading the
target binary. It does **not** cover the child's whole life: the ~30 ms `cmd` spends
talking to ActivityManagerService happens *after* we wake.

`fork()`'s advantage over `vfork` is exactly (b), and it is bounded by the cost of loading
`/system/bin/cmd` — a fixed ~1.1 ms. `vfork`'s advantage over `fork` is (a), which the
sweep in §3.2 prices: the `fork` route costs 1,613 µs at our 2.8 MB shipping footprint
(1,885 µs measured in §3.2, less the 272 µs the same `clone` costs with no suspension in
§3.1 — a cross-run subtraction, so read it as "about a third of a millisecond's worth of
page tables we do not copy", not as a measured quantity) and **34,044 µs more** at
1,283 MB than at 3 MB, which is in-run and is the number that decides D1. The `CLONE_VM`
family pays neither: it is 1,375–1,537 µs at every footprint in that range.

## 3. Measurements

All from the probe crate now vendored at `tools/spawnprobe` (built for
`aarch64-linux-android`, pushed to `/data/local/tmp/spawnprobe`); commands in Appendix A.
`[parent]` is the number that stalls the reactor. `[whole]` is spawn plus `waitpid`, so it
includes the ~30 ms the child needs to do its actual job, and is quoted only so nobody
confuses the two.

### 3.1 Backends at shipping footprint (real `/system/bin/cmd` target, n=60, RSS 2.8 MB)

| backend | `[parent]` P50 | note |
|---|---|---|
| `std::process::Command` (today) | **4,681 µs** | strace-proven `clone()`, no `CLONE_VM`, no `socketpair` |
| `libc::fork` + 3 `dup2` + `execve` | 1,627 µs | this roadmap's original choice — see §5.4 |
| `posix_spawn`, NULL actions | 1,324 µs | fastest, **unshippable** — §5.3 |
| `posix_spawn` + 3 `dup2` actions | 1,844 µs | Bionic silently downgrades to `fork()` |
| **`posix_spawn` + 3 `dup2` + `POSIX_SPAWN_USEVFORK`** | **1,387 µs** | the choice: actions *and* vfork |
| raw `clone(CLONE_VM\|CLONE_VFORK)`, hand-written asm child | 1,272 µs | 8.3 % better, rejected — §5.5 |
| raw `clone(CLONE_VM)`, no suspension | 272 µs | **control only, unsafe** — prices (b) |

Against today: `fork()` is 2.88x cheaper, `USEVFORK` is **3.37x** cheaper. Against
`fork()`, `USEVFORK` is a further 1.17x — a number this table does not support on its own:
§9's reproducibility bullet records that same capture re-run twice, where the 1.17x came out
0.84x and then 1.22x. Treat this section as one device's state, and §3.2 as the evidence.

Two things to notice. The three `dup2` actions cost **+457 µs** when Bionic is allowed to
downgrade to `fork()` (1,844 without the flag, 1,387 with it) — the flag is not a
micro-optimisation, it is what keeps the call off the page-table path. And the raw clone
route is 8.3 % faster than D1 (1,272 vs 1,387 µs), which §5.5 declines to buy with
per-ABI assembly.

The unsafe control row is what makes (a) and (b) separable: deleting the suspension
entirely saves 1,115 µs. So of `USEVFORK`'s 1,387 µs, roughly 1.1 ms is Bionic's own
suspended wait for the linker and ~0.27 ms is the shared-address-space `clone`. There is
no version of "launch a real dynamically linked binary" much below ~1.2 ms on this device
— which is also why a pitch promising 60–110 µs is not credible.

### 3.2 The footprint sweep — the decisive table

One process, resident footprint raised by touching anonymous memory (one byte per page),
all backends re-timed at each step, real `cmd` target, n=30/step. `AnonHugePages` and
`Swap` pinned at 0 kB throughout (`/proc/self/smaps_rollup`), so this is neither THP nor
reclaim.

| backend | RSS 3 MB | RSS 259 MB | RSS 1,283 MB | growth |
|---|---|---|---|---|
| `std::process::Command` | 5,277 µs | 29,478 µs | **90,473 µs** | 17.1x |
| `fork` + dup2 + execve | 1,885 µs | 11,117 µs | 35,929 µs | 19.1x |
| `posix_spawn` + dup2, **no flag** | 1,963 µs | 10,502 µs | 36,062 µs | 18.4x |
| **`posix_spawn` + dup2 + `USEVFORK`** | 1,537 µs | 1,390 µs | **1,375 µs** | **0.9x** |
| raw `clone(CLONE_VM\|CLONE_VFORK)` | 1,393 µs | 1,275 µs | 1,237 µs | 0.9x |

Two conclusions, and the second one reversed this roadmap's earlier plan:

1. **The `fork` family scales with resident memory; the `CLONE_VM` family does not scale
   at all.** At 1.3 GB, `USEVFORK` is **26x** faster than `fork()` and **66x** faster than
   `std::process::Command`.
2. **`posix_spawn` with file actions and `fork`+`dup2` are the same cost curve** —
   1,963/10,502/36,062 against 1,885/11,117/35,929, within noise of each other at all
   three points. That is empirical confirmation of the disassembly in §6.3: handing Bionic
   a non-NULL `file_actions` silently buys you a `fork()`. The tie measured in an earlier
   round was real; it was just a tie *at every footprint*, which is the part we had wrong.

At the daemon's live 2,832 kB RSS the gap between `fork` and `USEVFORK` is modest. The
asymmetry is what we are buying: `fork`'s number is a function of a variable we do not
control (our own footprint, and the device's willingness to hand us pages), while
`USEVFORK`'s is a function of the size of `/system/bin/cmd`.

### 3.3 Environment size, measured in one process (n=150 per point)

Percentages are relative to the 5,407 B row, because that is what the daemon actually
inherits today; the 0 B row is the counterfactual.

| env | raw vfork | `fork`+execve | `std::process::Command` |
|---|---|---|---|
| 0 B (`envp={NULL}`) | 1,342 µs | 2,130 µs | 5,288 µs |
| 5,407 B (what `adb shell` hands us) | 1,433 µs | 2,043 µs | 5,123 µs |
| 36,697 B (125 vars) | **1,844 µs** (+29 %) | 2,185 µs (+7 %) | 5,917 µs (+15 %) |

Mechanism: with `CLONE_VM|CLONE_VFORK` the parent stays suspended while the kernel copies
argv *and envp* onto the child's new stack, so environment bytes land inside our visible
window. End to end (0 B → 36,697 B) raw vfork pays +502 µs, i.e. ~14 ns per environment
byte, and `std` +630 µs. The `fork` row is +55 µs end to end and is not even monotone —
its middle point sits *below* its first — so within this measurement `fork`'s cost is
noise-dominated by the page-table copy, exactly as (a) predicts. A cross-run comparison
earlier in this investigation appeared to show vfork *doubling* with env (+85 %); that was
device drift, and this in-process A/B supersedes it.

Two facts make this actionable: `/system/bin/cmd activity kill` reaches AMS with a
completely empty environment (`env -i` produces the identical `IllegalArgumentException:
Argument expected after "kill"`), and `execve` needs no `PATH` because we hand it an
absolute path. So dispatching with `envp = {NULL}` is safe *for this target on this device*
and would save ~90 µs at the inherited size, ~500 µs if the init environment ever grows
fat. It is **not** in v1.5.0 (§7 item 6): it is a second variable in a release that already
changes the launcher, and it only pays if the inherited environment is large, which depends
on how the module is started.

### 3.4 `adddup2` vs `addopen`, on the vfork path (n=40, 3 interleaved rounds)

| action | mean of 3 P50s | per-round delta |
|---|---|---|
| `adddup2(cached_devnull_fd)` | **1,419 µs** | −140, −87, −129 µs |
| `addopen("/dev/null", O_RDWR)` | 1,538 µs | — |

`adddup2` won all three rounds by ~8 %. Mechanism: on the vfork path the child's work is
*inside* the parent's suspended window, so three `openat()` calls (path walk, VFS, SELinux
check) are charged to our reactor, whereas three `dup3()`s of an already-resolved fd are
not. Without `USEVFORK` the same two variants measured equal (1,539 vs 1,546 µs), because
there the parent is not waiting for any of it.

**Both were verified to actually redirect.** A child exec'ing `toybox ls -l /proc/self/fd`,
with its output captured to a file, shows `0 -> /dev/null`, `1 -> <file>`, `2 -> <file>`
for `adddup2`+`USEVFORK`, for `addopen`+`USEVFORK`, and for `adddup2` without the flag. So
the vfork fast path is not bought with a descriptor leak — that was the last correctness
blocker on D1. Neither variant leaks a *parent* descriptor across 80 dispatches
(`fds_before=5 fds_after=5 leaked=0`), including the failed-exec case. **Correction:** an
earlier draft claimed `addopen` leaks the parent's fd and must therefore be avoided. That
claim was wrong, and the fd-leak argument against `addopen` is withdrawn; the ~8 % cost
argument above replaces it.

### 3.5 Where the 1.4 ms actually goes

From the n=60 `cmd` run in §3.1, whose raw-clone child took its own `clock_gettime`
readings: `clone`→child-entry 417 µs (kernel setup, parent already suspended),
child-entry→just before `execve` 17 µs (three `dup3` plus two `clock_gettime`), against a
1,272 µs parent window — leaving **~838 µs for `execve` and Bionic's dynamic linker**
loading a 46 KB `cmd`. That residual is the floor of "run a real binary" here, and it is
the only part of the chosen backend's cost that is not ours to optimise. The three file
redirects themselves are the 17 µs line, i.e. free, which is why D1 does not try to avoid
them.

The window is also **not** proportional to the binary being exec'd: the same raw backend
measured 1,272 µs for the 46 KB `cmd` (n=60, §3.1) and 1,342 µs for a 1.6 KB
`/system/bin/true` (n=400, the probe's `vf` mode) — 29x the file size, 6 % apart, and in
favour of the *larger* binary — while the two children's own lifetimes were 30.4 ms and
18.9 ms. That pair is cross-run, so read it as "no scaling is visible", not as a bound.
What is in-run and solid is the shape: the parent's window is ~1.3 ms while the child's job
takes 19–30 ms, so the parent is demonstrably not waiting for the child's work, only for
its `execve` (§2(b)).

## 4. Decisions

**D1 — Primary backend: `posix_spawn` with three `adddup2` file actions plus a
`posix_spawnattr` carrying `POSIX_SPAWN_USEVFORK` (0x40), all resolved by `dlsym` at
startup.**
*Why:* it is the only measured option that is simultaneously fast (1,387 µs), flat in our
own footprint (0.9x across 430x of RSS), stdio-correct (§3.4), and — decisively — **not our
assembly**. The `vfork` discipline stays Bionic's problem, in Bionic's code, which acquires
the allocator's locks before the clone and runs its child under the `returns_twice` contract
we cannot express in stable Rust. Cost: one `dlsym` and one flag.
*What would overturn it:* a device or Android release where the flag is ignored (§6.3), or a
`dlsym` failure — both degrade to `fork`, both still work, both are visible in the banner (D4).

**D2 — Fallback for API 24–27 and for any `dlsym` failure: `fork` + 3 `dup2` + `execve`,
child `_exit(127)` if `execve` returns.**
*Why:* `posix_spawn` cannot be linked below API 28 (§5.1), and `fork` is the right thing to
do when the fast path is absent — 2.88x better than today, with no `dlsym`, no flag and no
ABI dependency. It is no longer the *primary* choice (§5.4), but it stays the floor, and it
is what D1 silently becomes when it cannot be honoured.

**D3 — `MINI_LMK_SPAWN` is a measurement surface, not a user feature.** Accepted values
`std`, `fork`, `spawn`; unknown values resolve to `fork`, never to `spawn`.
*Why:* `std` must stay reachable or we cannot A/B the fix against the release already
shipped. Defaulting an unknown value to `fork` means a typo can never *add* the `dlsym` path
to a device we did not intend to test.

**D4 — The startup banner prints the resolved backend, and the reason if `spawn` was
requested but not taken.**
*Why:* D1 has two independent silent failure modes — a `dlsym` miss and an ignored flag — and
both degrade to `fork`, which still works. Without the banner we cannot A/B on a device we
cannot reflash, and an ignored `USEVFORK` is indistinguishable from a slow device. This is
also why the backend is resolved once at startup rather than per dispatch: the answer has to
be knowable before the first kill.

**D5 — Redirect stdio with `adddup2` from a cached `/dev/null` descriptor opened in
`DaemonState::new()`, not with `addopen`.**
*Why:* §3.4, ~8 % on the path that matters. The cost is one descriptor for the process
lifetime, opened before the reactor starts, so there is no allocation and no `open()` inside
an event. `addopen` is not unsafe — it was measured correct and leak-free — it is just
slower here, and the child's `openat` failure would be invisible to us whereas a cached-fd
`dup3` cannot fail if the fd is valid.

**D6 — Telemetry: replace the kill record's `spawned: bool` with `spawn_errno`, and say in
the docs exactly what it can prove (§6.2).**
*Why:* a boolean cannot distinguish "we did not try" (`--observe`, or the AMS guard skipping
a low-UID package) from "we tried and the kernel refused". The sentinel scheme is: `None` =
no dispatch attempted (observe mode), `-2` = skipped by the AMS guard, `0` = dispatched,
positive = the OS errno from `posix_spawn`/`clone`. It deliberately does **not** promise to
carry `ENOENT`.

**D7 — Do not change what gets dispatched.** Still `/system/bin/cmd activity kill --user all
<pkg>`, still only under `--act`, still behind the same AMS guard.
*Why:* delegation to AMS is a correctness device, not a performance one — see §5.6 for the
rejected "just SIGKILL them" shortcut. This roadmap changes only how the binary is launched.

## 5. Rejected — and what killed each one

### 5.1 Calling `posix_spawn` directly instead of through `dlsym`
Not a preference, a constraint. The NDK's per-API stub `libc.so` exports **zero**
spawn-family symbols at API 21–27 and 25 at API 28, on all four ABIs we ship. The API 24
`spawn.h` compiles the declarations out (`error: call to undeclared function
'posix_spawn'`), and a hand-written `extern "C"` block that bypasses the header still
fails to **link** with this repo's own linker:

```
ld.lld: error: undefined symbol: posix_spawn
ld.lld: error: undefined symbol: posix_spawn_file_actions_init
```

The identical source links cleanly against `aarch64-linux-android28-clang`. So the block
is the link-time symbol floor — not a language gap, not a missing declaration, not a
load-time failure, and not something a `cfg` can paper over. `dlsym` at startup is the
only way to have the fast path on API 28+ and still ship an API 24 binary.

### 5.2 Raising the minimum supported API from 24 to 28 to make the `dlsym` go away
Rejected as a product decision. README.md:3 and `docs/ARCHITECTURE.md`:3 advertise Android
7.0+ (API 24+), and `src/main.rs` implements genuine pre-29 paths (`dumpsys power` parsing
instead of `cmd deviceidle get screen`, HOME-category resolution instead of `RoleManager`,
3-token versus 4-token event formats). Dropping to 28 deletes Android 7.0–8.1 support to
avoid one `dlsym`. Now doubly not worth it: D1 resolves the symbol at runtime and needs no
floor change. If anyone proposes it again, the answer is "it costs a supported release and
buys nothing D1 does not already provide".

### 5.3 `posix_spawn` with NULL `file_actions`
It is the fastest number in §3.1 (1,324 µs) and it is unshippable: with no file actions the
child inherits the daemon's descriptors. Under `package/axmanager/service.sh` that is
invisible because stdout is redirected; in an interactive `--act` run at a terminal it means
`cmd activity kill` writes into the user's terminal and may consume its stdin. We saw the
concrete failure during measurement — an AMS Java stack trace landed on the probe's own
stdout. Correctness before 250 µs.

### 5.4 `fork` + `dup2` + `execve` as the primary backend
**This was this roadmap's original choice, and it is superseded.** It is still correct,
still 2.88x better than today, and still the fallback (D2) — but §3.2 shows its stall is a
function of our own resident memory (19.1x from 3 MB to 1.3 GB), so its worst case lands
exactly where the daemon is least able to pay for it. It stays in the tree precisely
because it has no ABI dependency at all.

### 5.5 Hand-written `clone(CLONE_VM|CLONE_VFORK)` with an inline-asm child
8.3 % faster than D1 (1,272 vs 1,387 µs) and rejected. Costs: aarch64-only, so every future
ABI needs its own trampoline; the child runs on a shared stack, so it may touch nothing but
its own buffer and kernel descriptors, and any Rust codegen that spills is a parent
corruption; and it re-implements the `returns_twice` contract that is precisely why Bionic's
own `fork`/`vfork` wrappers exist. We proved it works here (the device shell domain has
`Seccomp: 0`, `asm!` is stable, `panic = "abort"` already) — capability is not the reason for
rejecting it, the 8.3 % is. Measurement fragility is a second signal: in one sweep the asm path
silently produced no rows because a `OnceLock` availability probe cached a `false` under a load
average of 12.8. That trap is closed — the vendored harness never caches a failure, and a
missing row now shouts `R MISSING` — but it generalises: a backend that is *absent* from a table
is one careless summary away from being *wrong* in one. A backend whose failure mode is "quietly
not measured" is a backend whose failure mode is "quietly not shipped".

### 5.6 Root fast path: `libc::kill(pid, SIGKILL)` per PID instead of spawning `cmd`
Dead in the only deployment we ship. On the reference device `id` reports
`uid=2000(shell) gid=2000(shell) context=u:r:shell:s0`, and `kill -0` against both
`system_server` and an app PID fails with `Operation not permitted`.
`docs/ARCHITECTURE.md`:132 already documents that stock SELinux policy denies direct signal
delivery from `u:r:shell:s0` to third-party app domains — which is *why* eviction is
delegated to `cmd activity kill` in the first place. Three further reasons it would be wrong
even as root, all traceable to documented invariants (`docs/ARCHITECTURE.md`:128–136):
`--user all` is what covers secondary users, work profiles and cloned apps; AMS terminates
only cached or dormant processes, so saved instance state, notifications, push tokens and
scheduled alarms survive; and the fail-closed low-UID guard exists because AMS will
terminate cached low-UID system packages. Per-PID killing also converts a stale
`pkg_to_pids` set into a PID-namespace use-after-free (PID reuse, or a package promoted to
foreground while its `wm_resume_activity` line still sits in the pipe), and its completeness
is unprovable because those PID sets come from `am_proc_start` events the daemon may never
have observed.

### 5.7 A dedicated spawn helper (a zygote-style fork server)
Rejected. The residual prize is the ~1.0 ms Bionic suspension window in §3.5, and the cost
is a supervised second process:

1. **Nothing would notice it die.** `reap_terminated_children()` is a
   `waitpid(-1, WNOHANG)` loop that special-cases only `logcat_pid` and discards every other
   PID; `main()` installs handlers for SIGINT/SIGTERM and ignores SIGPIPE, and leaves SIGCHLD
   untouched — a child's death does not even wake the blocking `epoll_wait(-1)`.
2. **Its replacement has to be spawned by the daemon**, re-creating the stall we avoided on a
   memory-short device, at the moment `mem_critical_percent` pressure is what killed it.
3. **The worst case is silent:** writes to a dead helper return `EPIPE` against an ignored
   SIGPIPE, leaving eviction dead with only a stream of failed records as evidence.
4. A permanent second process (~2.8 MB RSS), a changed `service.sh` contract, and two more
   descriptors — for a daemon whose entire dispatch duty cycle is 0.11 %.
5. It forfeits the fork-safety argument in §6.1 (a second process does not share our stack,
   but it also does not share our `clone()` reasoning; the in-process version is what we can
   prove).

The measured case for it was never strong: the monitored tag `wm_resume_activity` produced 18
lines in 14 s of aggressive app switching (1.29 lines/s, ~167 B/s), so the kernel's 64 KB
logcat pipe absorbs ~392 s of events at that rate — a 4.94 ms stall risks 0.00126 % of one
buffer, and the 5-minute benchmark's 64 dispatches in 300 s is a 0.21 dispatches/s duty cycle.
**No monitored event can be dropped by a dispatch stall.** This release is about tail latency
and invariants, not about saving lost events.

### 5.8 A dispatcher thread inside the daemon
Rejected, and it is the reason §6.1 exists: a second thread destroys the single-threaded
property that makes an in-process `clone(CLONE_VM, CLONE_VFORK)` cheap and lock-safe, because
another thread would be free to move data under the child while the parent is suspended. It
buys the same latency D1 already buys, with a new class of bug.

### 5.9 "Keep the daemon's RSS small" as the latency plan
The cheap-parent principle in §2(a) is real and measured — the probe's `sweep` mode, which
pushes footprint higher than §3.2's `vfsweep`, gives `fork` P50 2,055 µs at 3.4 MB versus
94,167 µs at 1.5 GB, 46x, with `AnonHugePages` and `Swap` pinned at 0 kB so it is neither THP
nor reclaim. But it is a *backend property*, not a budget we should live under. Choosing D1
removes the dependency instead of managing it: the `CLONE_VM` family measured flat across the
same 430x of footprint. So §7 item 5 keeps reporting RSS, reframed — a footprint increase no
longer costs dispatch latency, so what we watch for is a *regression in the invariant*, not a
latency lever. For contrast, the device's `Zygote64` runs at `VmRSS 142,740 kB`,
`VmSize 17,241,128 kB`, `Threads: 6`: it deliberately pays ~11–18 ms per fork to preload an
ART heap, which is a rational trade we do not want. What Zygote protects at fork time is
thread count, not memory size.

## 6. Constraints that bind the implementation

### 6.1 The single-threaded invariant is load-bearing, not stylistic
`CLONE_VM` shares the address space, so the child reads `argv`, `envp` and the file-actions
object *from our own memory* while the parent is suspended. That is safe only if nothing else
in the process can write them in that window. Today nothing can: the daemon has one thread
(`Threads: 1` observed on device) and the dispatch staging buffers are touched only by the
reactor. **Any future change that adds a thread, a Tokio runtime, or a lock held across the
spawn must be reviewed against this section.** The same rule is why `libc::vfork()` is not an
option in Rust: correct codegen for a returns-twice function depends on the C
`returns_twice` attribute, `#[ffi_returns_twice]` is unstable and absent from `libc::vfork`,
and under `lto = "fat"` / `opt-level = "z"` LLVM may spill into the shared stack. D1 avoids
the problem by letting Bionic's own wrappers keep that contract.

### 6.2 Exec failure is asynchronous, so `spawn_errno` cannot report `ENOENT`
Measured with an `execfail` probe mode: spawning a nonexistent binary with `posix_spawn`
returned **`rc=0` and a valid PID**, both with and without `USEVFORK`; the failure surfaced
only as child exit status `0x7f00` (i.e. `WIFEXITED`, `WEXITSTATUS == 127`) after `waitpid`.
`fork`+`execve` behaves the same way (that is where the `exit 127` convention comes from). So:

* `spawn_errno` carries **pre-clone** errors only — `EAGAIN`, `ENOMEM`, `EINVAL`, and our own
  `EINVAL` for a package name containing a NUL. A missing `/system/bin/cmd` is invisible to it.
* Capturing `127` would mean either waiting the child's full ~30 ms AMS round trip — which is
  ~9x the stall this release removes — or a SIGCHLD handler the daemon deliberately does not
  have (§5.7). Neither is in scope.
* Corollary that must not be forgotten: `cmd activity kill` children stay **zombies** until
  some unrelated event wakes the `waitpid(-1, WNOHANG)` loop. That is pre-existing behaviour,
  not a v1.5.0 regression, but a reviewer will find it and ask.

The docs must state this precisely, because the original pitch ("so `ENOENT` and `EAGAIN` are
distinguishable") is a promise this design cannot keep.

### 6.3 `POSIX_SPAWN_USEVFORK` is a Bionic extension and must be re-verified per release
It is deprecated in POSIX and a documented **no-op on glibc ≥ 2.24** (glibc's `posix_spawnp`
already uses `clone3(CLONE_VM|CLONE_VFORK)` by default, which is why the Kobzol article that
validated our cheap-parent numbers reports `USEVFORK` changing nothing on Linux — on *that*
libc `std::process::Command` is already on the vfork path, and our strace shows it is not
here). On Bionic it is honoured, and the evidence is three independent pieces: the branch,
the flag readback, and the shape of the numbers.

```
# Bionic libc.so, aarch64, inside the static posix_spawn() helper, +0x8c..0xb4
# (offsets from objdump of the device's /apex/com.android.runtime/lib64/bionic/libc.so;
#  they will move with the build — the structure of the test is the point, not the address)
6cf2c: tbz   w8, #0x6, 0x6cf38    # POSIX_SPAWN_USEVFORK not set -> check actions/flags
6cf30: b     0x6cf50              # set -> go straight to the vfork call
6cf40: cbnz  x23, 0x6cf5c         # file_actions != NULL  -> fork
6cf44: cbnz  w26, 0x6cf5c         # any other flag set     -> fork
6cf54: bl    vfork@plt            # CLONE_VM|CLONE_VFORK
6cf5c: bl    fork@plt             # the page-table copy
```

Bit 6 is tested *before* the `file_actions` downgrade, so setting it is what lets us have both
file actions and the shared-address-space clone. Bionic's own child stub still applies the
actions afterwards, which §3.4 confirms empirically (`0 -> /dev/null`). `posix_spawnattr_getflags`
reads back `0x40`. The two conclusions that follow: **we must log the resolved backend** (D4),
because a release that ignores the flag silently costs us 36 ms at 1.3 GB rather than
1.4 ms — and **the flag's meaning is not portable**, so no host-Linux test may assert on it.

### 6.4 FFI details we already got wrong once
* `libc` 0.2.189 **does** define `RTLD_DEFAULT` for Android, per ABI and correctly:
  `b32/mod.rs:195` is `-1isize as *mut c_void` (matching Bionic's LP32) and `b64/mod.rs:154` is
  `ptr::null_mut()` (matching LP64). An earlier draft of this roadmap claimed it was undefined
  and hand-rolled the constant, then used the imagined LP32 mismatch as an argument against D1.
  Both claims are withdrawn; use `libc::RTLD_DEFAULT`.
* `libc` declares **no** `environ` for the Android target, so a `fork`+`execve` path needs the
  hand-written binding: `unsafe extern "C" { static environ: *const *const libc::c_char; }`.
  This is mandatory, not decoration — `libc::environ` is `error[E0425]` on
  `aarch64-linux-android`.
* `libc` for Android types `posix_spawn_file_actions_t` as `*mut c_void` and declares none of
  the spawn functions, so all of
  `posix_spawn`, `posix_spawn_file_actions_{init,adddup2,destroy}` and
  `posix_spawnattr_{init,setflags,getflags,destroy}` are `dlsym`'d, and the actions object is an
  *opaque pointer-sized handle whose size we cannot see from the header*. Size the buffer for the
  larger glibc layout (`[usize; 32]` = 128 B on LP64 glibc, 8 B on Bionic; glibc's real object is
  80 B) so host-Linux tests exercise the same code as the 32-bit ABIs.
* `libc::WIFEXITED` / `libc::WEXITSTATUS` are `pub const fn`, callable from inside an
  `unsafe` block; `libc::kill` and `libc::geteuid` are generated as unsafe functions.

### 6.5 The host test suite is load-bearing, not a convenience
CI runs `cargo test` on an Ubuntu host, so the `dlsym` dispatcher, the `fork` path and the
actions-buffer sizing are all exercised against glibc, which is the only place a `posix_spawn`
ABI mistake can be caught without a device. Two rules follow: tests that would dispatch a real
`cmd activity kill` must be gated `#[cfg(not(target_os = "android"))]` (on device
`/system/bin/cmd` exists, so a "missing binary" assertion would kill something), and the spawn
tests must not rely on fd-number stability across threads — the daemon is single-threaded but
`cargo test` is not, so a `Spawner` whose `Drop` closes the cached `/dev/null` descriptor lets
a concurrent test recycle that number before another test's redirect runs. That flake
(`test_fork_backend_executes_and_reaps`) is a harness artifact, not a daemon bug — but it must
be handled in the harness (`--test-threads=1` for the spawn module's tests, or a `static`
descriptor that is never closed) rather than by weakening the assertion.

## 7. What v1.5.0 actually changes

1. **New module `src/spawn.rs`** owning the dispatch: a `Backend { Std, Fork, PosixSpawn }`
   enum and a `Spawner` that holds the resolved function pointers, the cached `/dev/null`
   descriptor, the retained `CString` staging and a fixed-width `argv`. `spawn_kill(pkg)`
   returns a `c_int`: `0` dispatched, positive OS errno, `-1` other, and a `NOT_DISPATCHED`
   sentinel (`-2`) for the AMS-guard skip. Resolution happens once, in `DaemonState::new()`.
2. **`src/main.rs`** gains `mod spawn;`, a `spawner` field on `DaemonState` (both
   `new()` and the test-only `daemon_for_test()`), the D4 banner, and the dispatch loop calls
   `self.spawner.spawn_kill(pkg)` instead of building a `Command`.
3. **Telemetry**: `"spawned": bool` becomes `"spawn_errno": i32` with the §6.2 semantics
   documented in `docs/ARCHITECTURE.md`, not just in the code.
4. **Tests**: `Backend::parse` coverage including the unknown-value-goes-to-`fork` rule; the
   fork path executing and being reaped on the host; the child's stdio actually redirected
   (via `readlink /proc/$$/fd/N` in a shell — *not* `/proc/self/fd/N`, which reports the
   `readlink` child's own descriptors and silently passes for the wrong reason); NUL-in-name
   rejection; `posix_spawn` `ENOENT` on a missing binary where available; and the staging
   invariant that `argv` never reallocates.
5. **`scripts/benchmark.sh` reports the daemon's `VmRSS`** beside the existing `/proc/<pid>/fd`
   census, and `docs/ARCHITECTURE.md` states the one sentence that explains why (§2(a)). This
   is a regression watch, not a latency lever, and §5.9 says so.
6. **Deferred, deliberately:** the empty-`envp` change (§3.3) is a measured ~90 µs today and a
   ~500 µs win only if the inherited environment is large. It is a second variable in a release
   that already changes the launcher, and its safety argument is per-target ("`cmd` reaches AMS
   with `env -i` on this device"), which is the weakest kind. Revisit with a device-side matrix
   once the backend is settled.

## 8. Acceptance criteria

* On the reference device with the real target, the kill-dispatch `[parent]` P50 is under
  1,600 µs at shipping footprint **and** under 2,000 µs at 1 GB of daemon RSS. The second
  clause is the point of the release; a backend that passes only the first is a regression
  dressed as a win.
* `MINI_LMK_SPAWN=std` still works and lands in the 2.9–5.4 ms band that §9 documents for
  absolute `[parent]` P50s of the std row (2,868 / 3,736 / 4,681 / 5,386 µs across four
  captures of the same device), so the A/B is honest without promising a reproducibility
  that this device does not offer.
* The startup banner distinguishes `spawn` (fast path live) from `fork` (fallback, with reason).
* No new descriptor, no new thread, no allocation inside the dispatch loop: the `/proc/<pid>/fd`
  census and `Threads: 1` must read as they do today.
* `cargo test`, `cargo clippy --release --all-targets` and `cargo fmt` stay at their baselines
  (119 fmt hunks).

## 9. Measurement hygiene, so the next reader does not repeat our mistakes

Every number here comes from the probe crate at `tools/spawnprobe` (Appendix A) — vendored into
the repo after this file shipped reproduction commands for a crate that existed only in `/tmp`.
The ways it lied to us, in order of how long each cost us:

* **Cross-run comparisons are worthless on this device.** Two rows from different processes
  differ by more than the effect we were chasing; the "+85 % from environment size" claim in
  §3.3 was an artefact of that, and every A/B since is in-process and interleaved. Where this
  document does quote across runs (§3.5's target-size pair, §5.9's `sweep` mode) it says so.
* **A silently absent row gets read as an absent effect.** At `load average 12.8` the probe's
  one-shot `OnceLock<bool>` availability cache latched `false`, deleting every raw-`clone` row
  from a sweep that then looked like "raw clone does not scale". The vendored harness never
  caches a negative, and a row that collected nothing now prints `R MISSING` while a truncated
  one says `!! SHORT: n of N samples survived` — because the dangerous failure here was never a
  crash, it was a column nobody noticed was gone.
* **Another tenant deleted our on-device log mid-run.** Pull results off the device
  immediately: pipe `adb shell` straight to the build machine, never leave the only copy in
  `/data/local/tmp`.
* **Never benchmark the real target at 400 iterations.** Each dispatch against
  `/system/bin/cmd activity kill` is a genuine AMS Binder round trip that dumps an exception
  stack into `system_server`'s log. n≈30–60 per step, and `AnonHugePages`/`Swap` pinned.
* **Quote the P50 of the parent stall, not the mean of "spawn + waitpid".** The child's own
  30 ms dominates the latter, and mixing the two produced this roadmap's first wrong headline.
* **`/tmp` on the build machine is not storage.** Every raw capture behind the tables below was
  reclaimed mid-investigation, twice; this file's own predecessor was overwritten by a stale
  snapshot restore. The probe crate is now in git, so the *harness* survives; the captures do
  not, which is why the tables here remain the record. Transcribe, then commit.
* **Absolutes do not reproduce — and neither does one published ranking.** Three captures of
  the *same mode* on the same device (`60 cmd vf`, API 34, `Threads: 1`, `AnonHugePages` and
  `Swap` 0 kB, load average 12.8–14.7; the first with the pre-vendoring binary, the next two
  minutes apart with this crate): every `[parent]` P50 landed at 0.53–0.80x of §3.1's published
  value in the first re-run and 0.40–0.61x in the second. `fork` went 1,627 → 854 → 785 µs,
  `USEVFORK` 1,387 → 1,019 → 646 µs, `std` 4,681 → 3,736 → 2,868 µs, the VM-only control
  272 → 167 → 137 µs. More damaging than the drift: the ratio §3.1 quotes, "`USEVFORK` is a
  further 1.17x over `fork`", measured 0.84x then 1.22x. At shipping footprint those two
  backends are within device noise of each other, and their *order* is not a reproducible fact.
  What survived all three captures is the ≥ 2x gap to `std` and the §3.2 footprint sweep
  (`fork` 19.1x, `USEVFORK` flat) — which is where D1's rationale actually sits, and why §3.1
  is context rather than evidence. Two rules: do not promote a same-footprint ranking of two
  backends less than ~1.2x apart, and never quote `std` as a single number (the same capture has
  P50 2,868 µs against P99 6,644 µs). Read any fresh absolute as "this device, right now" —
  which is why every run now prints its own load average, RSS, thread count, API level,
  `AnonHugePages`, `Swap` and `Seccomp` beside the rows.

## 10. Claims that must not be repeated

Kept because they were in a draft of this file, or in a pitch, and are now known to be false:

| claim | why it is false |
|---|---|
| "`fork` is 2.55x cheaper than `std`, so `fork` is the backend" | the ratio is 2.88x at shipping footprint, and the decision it justified is wrong: `fork`'s stall is a function of our own RSS (19.1x from 3 MB to 1.3 GB) while `USEVFORK`'s is not a function of anything we control |
| "`posix_spawn` ties `fork`, so it is not worth it" | the tie was Bionic secretly running `fork()`; with `USEVFORK` it is 1,375–1,537 µs at every footprint vs 1,885–35,929 µs |
| "`std::process::Command` on Android is vfork-based / RSS-immune" | strace shows plain `clone` with no `CLONE_VM`; in-process it grows 5,277 → 90,473 µs |
| "`Command::spawn` doubles the FD count via a socketpair" | zero `socket`/`socketpair` in a syscall-filtered strace; `fds_during_spawn=4` |
| "`addopen` leaks a parent descriptor" | `fds_before=5 fds_after=5` over 80 dispatches, including the failed-exec case; it loses on cost, not correctness |
| "`RTLD_DEFAULT` is missing from `libc` for Android and is wrong on LP32" | `libc` defines it per ABI, correctly (`b32` `-1`, `b64` `null`) |
| "`spawn_errno` will let us tell `ENOENT` from `EAGAIN`" | exec failure is asynchronous (`rc=0` + exit `127`); see §6.2 |
| "the vfork window scales with the child binary" | 1,272 µs for the 46 KB `cmd` vs 1,342 µs for a 1.6 KB `true` — 29x the size, 6 % apart, larger one faster, while the children's own lifetimes were 18.9 ms and 30.4 ms |
| "POSIX_SPAWN_USEVFORK does nothing" (from the glibc article) | true on glibc ≥ 2.24, false on Bionic, where the branch is before the actions check |
| "a 60–110 µs dispatch is achievable" | below the cost of `exec`-ing any dynamic binary here; the no-suspension control is 272 µs |
| "this fixes dropped logcat events / a 16.6 ms frame budget" | 0.21 dispatches/s against a pipe that holds ~392 s of events, and this is not the UI thread |

## Appendix A — reproducing the numbers

The harness lives in the repo at `tools/spawnprobe`, deliberately **not** a workspace member
(`[workspace] exclude` in the root `Cargo.toml`), so `cargo test`, `cargo clippy --all-targets`
and `cargo fmt` keep operating on the daemon alone and this crate's edition-2024 code, hand
written aarch64 `clone` assembly and `dlsym`'d Bionic symbols cannot leak into a product build.
Building it is a separate, explicit act:

```sh
cargo build --release --target aarch64-linux-android \
    --manifest-path tools/spawnprobe/Cargo.toml
adb push tools/spawnprobe/target/aarch64-linux-android/release/spawnprobe /data/local/tmp/

# one line per table in section 3; pipe the capture straight off the device
adb shell /data/local/tmp/spawnprobe 60  cmd vf       # 3.1 backend ladder, 3.5 window split
adb shell /data/local/tmp/spawnprobe 30  cmd vfsweep  # 3.2 the decisive footprint table = the §8 gate
adb shell /data/local/tmp/spawnprobe 40  cmd env      # 3.3 envp size, in-process A/B
adb shell /data/local/tmp/spawnprobe 40  cmd vfopen   # 3.4 adddup2 vs addopen, interleaved rounds
adb shell /data/local/tmp/spawnprobe 40  cmd openfd   # 3.4 the same pair without USEVFORK (control)
adb shell /data/local/tmp/spawnprobe 1   true fdcheck # 3.4/6.3 do the file actions really redirect?
adb shell /data/local/tmp/spawnprobe 1   true execfail # 6.2 exec failure is asynchronous
adb shell /data/local/tmp/spawnprobe 40  true ladder  # 3.1 plus the file-action-count decomposition
adb shell /data/local/tmp/spawnprobe 150 true sweep   # 2   fork() against the parent's own RSS
adb shell /data/local/tmp/spawnprobe 200 true ipc     # 5.7 the socketpair round trip a helper needs
```

Each run prints its own confounds, so a capture cannot be pasted into a claim without them:

```
R mode=vf -> docs/ROADMAP.md 3.1 and 3.5
R iters=60 rss=3 MB threads=1 api=34 loadavg=13.62 anon_hugepages=0 kB swap=0 kB seccomp=0
```

The previous version of this appendix gave `./spawnprobe 1 x fdcheck` (a target that no longer
parses, and a slot whose meaning used to change per mode) and a build line that worked only from
the probe's own directory. Both were instructions nobody could follow, which makes the tables
they pointed at unfalsifiable — the exact failure §9 exists to warn about. The CLI contract is
now enforced, each rule here because breaking it produced a run that was believed:

* `<iters> <true|cmd> <mode>`; only `rss` takes a fourth argument, and passing one to any other
  mode is fatal. `vf` and `env` used to read the same positional as "iterations" and "env size"
  respectively, which is how one suite got published as another.
* Unknown mode or unknown target is fatal, instead of falling through to the default ladder.
* More than 120 dispatches against the real `cmd` target is refused unless `SPAWNPROBE_FORCE=1`.
  Each one is a Binder round trip that dumps an AMS exception into `system_server`'s log; §3.1
  and §3.2 were built at n=60 and n=30 for that reason, not for want of patience.
* A row that collected nothing prints `R MISSING` and a truncated row says `!! SHORT: n of N
  samples survived`. Nothing may vanish quietly from a table that someone will quote.

One structural result the vendored `ladder` mode makes visible: the cost cliff is a **step** at
"you passed a non-NULL `file_actions` object", not a slope per action — empty 753, one `dup2`
755, two 727, three 747 µs, against 547 µs with `NULL` actions (`true`, n=40, contended device,
so read the pattern, not the absolutes: see §9's reproducibility bullet). That is §6.3's branch, and the
reason D5's three `adddup2` calls cost nothing next to the one that Bionic would have run.

Raw captures are still not archived — `/tmp` on this machine reclaims them, and `/tmp/vf_cmd.txt`
did vanish mid-write — but the asymmetry is gone: regenerating a table is now a `git clone` plus
one build command, not a reconstruction from whoever was in the room.
