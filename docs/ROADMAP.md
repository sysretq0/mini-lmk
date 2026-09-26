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
window. What it does *not* do is make the window independent of our own resident set - `fork`'s
cost is a function of the parent's RSS (§2, §3.2) - and this release accepts that trade on
purpose, because the daemon's footprint is bounded and §8 turns the bound into a measured
criterion instead of an assumption.

Two things this is **not**, because both were asserted in earlier pitches and neither
survives measurement:

* **It is not an fd leak.** `std::process::Command::spawn` on Android opens nothing: the
  probe records `fds_during_spawn=4` across 60 dispatches — the census count of that
  capture, unchanged across all of them. Read the 4 as "this capture's census", never as a
  property of a daemon: the count is a function of the build and the mode, and v1.5.0 adds
  one descriptor (the cached `/dev/null`) that v1.4.0 did not have. The finding is the
  *flatness*, which is the only part that survives a rebuild. The "secret socketpair doubles
  the FD count" story does not reproduce here; a syscall-filtered `strace` of the same path
  found `clone`, `execve`, `wait4` and 379 `rt_sigprocmask`, and **zero** `socket`,
  `socketpair` or `pipe2`.
* **It is not an animation-budget problem.** This daemon is not the UI thread. A dispatch
  stall delays *our next decision*, not a frame. Anyone quoting 16.6 ms frame budgets for
  this call site has the wrong thread.

The honest framing: **reactor tail latency, plus aligning runtime behaviour with the
documented single-threaded, zero-allocation, no-internal-state invariants**
(`docs/ARCHITECTURE.md` §2). We are paying 2.9–4.7 ms — §9's whole spread for one backend, and
the honest way to quote `std` — to learn nothing.

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
1,283 MB than at 3 MB. The `CLONE_VM` family pays neither: it is 1,375–1,537 µs at every
footprint in that range. Both halves of that sentence are true and §4 still does not buy the
`CLONE_VM` route, because the two curves only separate at footprints this daemon is
constructed not to reach (§3.2, §5.9, and the budget in §8), while at the footprint it does
occupy they are inside each other's noise (§9).

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
| **`libc::fork` + 3 `dup2` + `execve`** | 1,627 µs | **the shipped backend** (§4 D1) |
| `posix_spawn`, NULL actions | 1,324 µs | fastest, **unshippable** — §5.3 |
| `posix_spawn` + 3 `dup2` actions | 1,844 µs | Bionic silently downgrades to `fork()` |
| **`posix_spawn` + 3 `dup2` + `POSIX_SPAWN_USEVFORK`** | **1,387 µs** | measured, not shipped (§4 D2, §5.4) |
| raw `clone(CLONE_VM\|CLONE_VFORK)`, hand-written asm child | 1,272 µs | 8.3 % better, rejected — §5.5 |
| raw `clone(CLONE_VM)`, no suspension | 272 µs | **control only, unsafe** — prices (b) |

Against today: `fork()` is 2.88x cheaper, `USEVFORK` is **3.37x** cheaper. Against
`fork()`, `USEVFORK` is a further 1.17x — a number this table does not support on its own:
§9's reproducibility bullet records that same capture re-run twice, where the 1.17x came out
0.84x and then 1.22x. Treat this section as one device's state, and §3.2 as the evidence.

Two things to notice. The three `dup2` actions cost **+457 µs** when Bionic is allowed to
downgrade to `fork()` (1,844 without the flag, 1,387 with it) — the flag is not a
micro-optimisation, it is what keeps the call off the page-table path. And the raw clone
route is 8.3 % faster than the best measured alternative (1,272 vs 1,387 µs), which §5.5 declines to buy with
per-ABI assembly.

The unsafe control row is what makes (a) and (b) separable: deleting the suspension
entirely saves 1,115 µs. So of `USEVFORK`'s 1,387 µs, roughly 1.1 ms is Bionic's own
suspended wait for the linker and ~0.27 ms is the shared-address-space `clone`. There is
no version of "launch a real dynamically linked binary" much below ~1.2 ms on this device
— which is also why a pitch promising 60–110 µs is not credible.

### 3.2 The footprint sweep — the table that decided D1, and why D1 changed

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

At the daemon's live 2,832 kB RSS the gap between `fork` and `USEVFORK` is 240 µs of a
1,500 µs stall, and §9 measured that gap's *sign* flipping between two consecutive captures.
The asymmetry in the table is real, so the honest question is not "does `fork` scale" but
"can this process ever be big enough for the scaling to matter" — and the answer is no: the
daemon's resident set is bounded by three fixed-cardinality tables plus a rotating log
(§5.9), it has measured 3,876–4,020 kB under live app-switch traffic, and §3.2's first column
is 2.8 MB while its second is 259 MB. Paying for the flat curve means paying a second
dispatch path, a `dlsym` and a deprecated flag to insure against a footprint the design
forbids, which is why D1 is `fork` and this table is now evidence *about the mechanism*, not
a purchase order.

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

**D1 — The shipped backend is `fork` + three `dup2` + `execve`, child `_exit(127)` if `execve`
returns.** One path, no `dlsym`, no ABI dependency, no deprecated flag.
*Why:* this release exists to stop paying `std::process::Command`'s tax inside the reactor's
window, and that is the whole measured win — 2.88x cheaper than `std` at the shipping footprint
(§3.1), and still ≥2x cheaper in the three captures §9 holds up as the reproducible set.
Everything beyond that is second-order, and the one second-order claim that carried the
alternative — RSS independence — is a function of a resident set between 259 MB and 1.3 GB in a
process that measures 3,876–4,020 kB and is *constructed* not to grow (its three tables are
bounded by device process and package counts; README targets < 4 MB). At the footprint we
actually occupy, the alternative was 240 µs better in one capture and 208 µs worse in the next
(§9), and §5.4 keeps the rest of the argument.
*What would overturn it:* the daemon's footprint ceasing to be bounded — a change of design, not
a drift, which is why §8 makes the bound an acceptance criterion instead of an assumption — or a
device where `fork` itself is the slow one. Both are visible: the first in `benchmark.sh`'s RSS
and fd columns, the second by A/B-ing `MINI_LMK_SPAWN=std` on the device.

**D2 — `posix_spawn` + `adddup2` + `POSIX_SPAWN_USEVFORK` is not shipped, and is not deleted.**
It is implemented, measured and CI-compiled inside the harness at `tools/spawnprobe` (`vf`,
`vfsweep`, `ladder`, `vfopen`, `openfd`, `fdcheck`, `execfail`), which is where §3's tables come
from and where the code stays runnable if D1 is ever overturned. What the product declines to
carry is a second live dispatch path.
*Why this is a reversal:* the first draft of this section made `posix_spawn` D1 and `fork` its
fallback, and listed `fork`-as-primary as a rejected option. Nothing about the measurements
changed; the daemon's real footprint budget got put next to §3.2's columns instead of being
assumed, and the decision inverted. §5.4 is the record.

**D3 — `MINI_LMK_SPAWN` is a measurement surface, not a user feature.** Accepted values `std` and
`fork`; anything else resolves to `fork` **and the startup line says so**, naming the value that
was ignored.
*Why:* `std` must stay reachable or we cannot A/B the fix against the release already shipped. A
value that used to be valid is not the same as an unknown one: `spawn` selected a backend that
this file spent three days measuring, and honouring the default while the operator believes their
choice took effect is the quiet-fallback class that has burned this project twice (§9). So the
ignore is announced, with its reason. An unrecognised value still cannot put a device onto a path
nobody intended to test.

**D4 — The startup banner prints the backend actually in use, and the reason whenever it is not
the one implied.**
*Why:* `fork` has one silent downgrade left — `/dev/null` unavailable, which moves the daemon to
`std` — and the other case worth an operator seeing is D3's ignored request. Resolved once at
startup rather than per dispatch: the answer has to be knowable before the first kill, and
per-dispatch resolution would mean a `getenv` and a string compare inside the window this release
is trying to shrink.

**D5 — Redirect the child's stdio with three `dup2`s from a cached `/dev/null` descriptor opened
in `Spawner::new()`, and do not run `fork` without one.**
*Why:* §5.3 is the failure mode of not redirecting — `cmd activity kill` writing into the
operator's terminal and possibly consuming its stdin, observed concretely as an AMS Java stack
trace landing on the probe's stdout. The descriptor is opened before the reactor starts, so there
is no `open()` and no allocation inside an event. If it cannot be opened, the backend becomes
`std`, whose own `Stdio::null()` does the job correctly; dispatching with `dup2(-1, fd)` would
turn every kill into a silent `exit(127)` *and record it as a success*, which is the one failure
mode this design cannot afford.
*Correction:* the first draft justified the cached descriptor with §3.4's ~8 %
`adddup2`-over-`addopen` margin. That margin was priced on the `vfork` path, where the child's
`openat` calls land inside the parent's suspended window; under `fork` the parent waits for none
of it, so the 8 % does not transfer and is withdrawn here. The cached descriptor stays for the
reason above, not for latency.

**D6 — Telemetry: replace the kill record's `spawned: bool` with `spawn_errno`, and say in
the docs exactly what it can prove (§6.2).**
*Why:* a boolean cannot distinguish "we did not try" (`--observe`, or the AMS guard skipping
a low-UID package) from "we tried and the kernel refused". The sentinel scheme is: `None` =
no dispatch attempted (observe mode), `-2` = skipped by the AMS guard, `0` = dispatched,
positive = the OS errno from `clone` (via `fork`) or from this module's own refusals. It deliberately does **not** promise to
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
load-time failure, and not something a `cfg` can paper over. `dlsym` at startup was the only
way to have that fast path on API 28+ and still ship an API 24 binary — which is precisely the
tax §5.4 refuses to pay: a resolver, a probe of its own results, and a complete `fork` fallback
that is the *only* path on the API 24–27 devices. The harness still works this way, so its
numbers stay reproducible on old hardware.

### 5.2 Raising the minimum supported API from 24 to 28 to make the `dlsym` go away
Rejected as a product decision. README.md:3 and `docs/ARCHITECTURE.md`:3 advertise Android
7.0+ (API 24+), and `src/main.rs` implements genuine pre-29 paths (`dumpsys power` parsing
instead of `cmd deviceidle get screen`, HOME-category resolution instead of `RoleManager`,
3-token versus 4-token event formats). Dropping to 28 deletes Android 7.0–8.1 support to
avoid one `dlsym`. If anyone proposes it again, the answer is that it costs a supported release
and buys nothing: D1 is `fork`, which every libc has exported forever, so there is no floor to
raise and no symbol to resolve.

### 5.3 `posix_spawn` with NULL `file_actions`
It is the fastest number in §3.1 (1,324 µs) and it is unshippable: with no file actions the
child inherits the daemon's descriptors. Under `package/axmanager/service.sh` that is
invisible because stdout is redirected; in an interactive `--act` run at a terminal it means
`cmd activity kill` writes into the user's terminal and may consume its stdin. We saw the
concrete failure during measurement — an AMS Java stack trace landed on the probe's own
stdout. Correctness before 250 µs.

### 5.4 `posix_spawn` + `adddup2` + `POSIX_SPAWN_USEVFORK` as the primary backend
**This was D1 of the first draft; it is now the rejected option, and it is the one the product
does not run.** Nothing about it is broken — it is implemented, measured and RSS-immune, and it
still lives in `tools/spawnprobe`, where §3.2's flat 0.9x row is generated from it on demand. What
killed it for the daemon is that its whole advantage is a function of the parent's resident set,
and this daemon's resident set is bounded:

* At 3 MB it measured 1,387 µs against `fork`'s 1,627 µs; the next two captures of the same mode
  on the same device put the pair at 0.84x and 1.22x — the order is not a reproducible fact (§9).
  There is no ship-worthy margin at the footprint we occupy, only at the footprints we cannot
  reach.
* `posix_spawn_file_actions_adddup2`, `posix_spawnattr_setflags` and `posix_spawnattr_getflags`
  are `__INTRODUCED_IN(28)` in the NDK while the shipped floor is API 24 — set not in
  `Cargo.toml` (which carries no SDK key) but by the `*-android24-clang` linker wrappers in
  `.cargo/config.toml`, for all four ABIs (§5.1, §5.2). So the
  backend can never be alone: it carries eight `dlsym` lookups *and* a complete `fork` fallback,
  and on API 24–27 devices the fallback is the only thing that runs. Two code paths, one of which
  is the code we already have.
* It rests on a flag that is deprecated in POSIX and a no-op on glibc ≥ 2.24 (§6.3), so it needs
  re-verifying against every Android release — and the verification is a lie if you read it from
  the API: `posix_spawnattr_setflags` *stores* 0x40 even where the flag is ignored, so the
  readback proves nothing and the only honest evidence is the latency shape, which §9 shows is
  noisy to the point of flipping sign.
* The host suite — the only place an ABI mistake can be caught automatically — could not test it.
  glibc's `posix_spawn` accepted three `adddup2` actions and then ignored them: the child's fd 1
  and fd 2 stayed on the test harness's capture pipe while the `fork` path in the same binary
  redirected all three to `/dev/null`. A backend whose correctness test passes on one libc and
  silently no-ops on another has decoration for a test.

### 5.5 Hand-written `clone(CLONE_VM|CLONE_VFORK)` with an inline-asm child
8.3 % faster than `USEVFORK` (1,272 vs 1,387 µs) and rejected. Costs: aarch64-only, so every future
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
`docs/ARCHITECTURE.md` §1.5 ("SELinux Permission Boundary") already documents that stock SELinux
policy denies direct signal delivery from `u:r:shell:s0` to third-party app domains — which is
*why* eviction is delegated to `cmd activity kill` in the first place. Three further reasons it
would be wrong even as root, all in the same §1.5 invariant list (cited by section, not by line
number, because line numbers rot the first time an unrelated bullet is added):
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

### 6.1 The single-threaded invariant: load-bearing for `vfork`, ordinary hygiene for `fork`
`CLONE_VM` shares the address space, so a `vfork`-style child reads `argv`, `envp` and the
file-actions object *out of our own memory* while the parent is suspended. That is safe only if
nothing else in the process can write them in the window, which is why §5.4 and §5.5 each carry
this section as a precondition.

**D1 does not need it.** `fork` hands the child a copy, so a second thread could not corrupt the
child's arguments; what it could still do is the normal `fork`-in-a-threaded-process damage
(holding a lock the child's `dup2` or `execve` needs), which is why the child touches nothing but
three descriptors and one `execve` and then `_exit`s. The daemon is single-threaded today
(`Threads: 1` observed on device) and §8 keeps asserting it, for two reasons that are not about
address spaces: Bionic's `fork` runs `__bionic_atfork_run_prepare`/`_parent`/`_child` around the
`clone` — five PLT hops and an fdtrack/stack-guard reset in the child, priced in the parent's
window — and every measurement in §3 was taken under that invariant, so a change to it invalidates
the table as well as the reasoning.

The `returns_twice` note stays on file for whoever revisits `vfork` in Rust: correct codegen for a
returns-twice function depends on the C `returns_twice` attribute, `#[ffi_returns_twice]` is
unstable and absent from `libc::vfork`, and under `lto = "fat"` / `opt-level = "z"` LLVM may spill
into the shared stack. That is the reason §5.4 was attractive when it was D1 — it let Bionic keep
that contract — and one less reason to miss it now that it is not.

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
**Scope: `tools/spawnprobe`, and anyone revisiting §5.4. Not the shipped daemon, which never sets
this flag.** The section survives because the measurements below are how §3's `USEVFORK` rows were
validated and because the reasoning is expensive to redo.

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
  `posix_spawnattr_{init,setflags,getflags,destroy}` have to be `dlsym`'d, and the objects are
  *opaque handles whose size is invisible from the header*. Size any buffer for the largest
  implementation, not the one you are linking against: measured on an aarch64 glibc 2.43 host,
  `sizeof(posix_spawnattr_t)` is **336 B** and `sizeof(posix_spawn_file_actions_t)` is 80 B, while
  Bionic's are pointer-sized. The size varies by libc *and* ABI, so it cannot be pinned at compile
  time and the buffer is deliberately over-allocated (512 B). (An earlier bullet here asserted `[usize; 32]` was "256 B on LP64
  glibc"; LP64 `usize` is 8 B, so 32 of them are 256 B and the attribute object needs 336 — the
  buffer was 80 B short of its own comment.) This is what `tools/spawnprobe` had wrong: its objects
  were single `*mut c_void` slots, so `spawnprobe vf` on an x86-64 host wrote 336 B into 8 B of
  stack. Fixed to `[usize; 64]` (512 B) buffers, which the host `vf` run now executes inside.
  The shipped daemon allocates neither object, because D1 does not call `posix_spawn`.
* `libc::WIFEXITED` / `libc::WEXITSTATUS` are `pub const fn`, callable from inside an
  `unsafe` block; `libc::kill` and `libc::geteuid` are generated as unsafe functions.

### 6.5 The host test suite is load-bearing, not a convenience
CI runs `cargo test` on an Ubuntu host, so the `fork` dispatcher, the `execve` failure mode and the
stdio redirect are all exercised against glibc — the only place any of it can be checked without a
device. Three rules fall out, each paid for in a failed test:

* **Gate anything that could really kill something.** A "missing binary" assertion is a live
  `cmd activity kill` on a device, where `/system/bin/cmd` exists. Those tests are
  `#[cfg(not(target_os = "android"))]`, and the cfg is the safety device, not the organisation.
* **Wait for the `execve` before inspecting a child.** Reading `/proc/<pid>/fd` straight after
  `fork` shows the *parent's* descriptors, because the child has not reached its `dup2`s yet. With
  the redirect correct the test saw the harness's capture pipe on fd 1 and 2 and reported a leak;
  with the harness's stdin already `/dev/null`, fd 0 looked right. It could just as easily have
  passed on a broken redirect. The barrier is `/proc/<pid>/cmdline`, which `execve` rewrites and
  nothing else does — a `sleep` before the read is not a barrier, it is a coin flip.
* **Never take a process-wide census inside a parallel test binary.** The descriptor test asserted
  "startup costs exactly one fd" and then "twenty dispatches leaked nothing", and the second
  reading came out *lower* than the first: every test in the binary shares one fd table, and the
  unrelated churn
  around the measurement drained while it ran. Sampling the minimum over eight reads does not
  help, for the same reason. The in-process test now asserts *identity* — our descriptor is open,
  is not one of the std slots, and is the same number after twenty dispatches — and the drift
  census happens where the claim can be seen at all: a quiescent daemon under
  `scripts/benchmark.sh`, §7 item 5. Note the mode that script runs, because it changes what the
  census proves: by default it samples an `--observe` daemon, which never calls `spawn_kill()`, so
  the verdict covers the startup descriptor and the thread count and **not** a per-dispatch leak.
  The dispatch half needs `--daemon-mode act`, which really kills the apps under test and so is
  opt-in. Asserting "stable across dispatches" from an observe-mode census is the same mistake as
  the cross-run comparison above, made in the other direction.
  Also relevant: a `Spawner` whose `Drop` closes the cached `/dev/null` lets a concurrent test
  recycle that number under another test's redirect, which is the flake this section was written
  about the first time.

## 7. What v1.5.0 actually changes

1. **New module `src/spawn.rs`** owning the dispatch: a `Backend { Std, Fork }` enum and a
   `Spawner` holding the backend, the startup note if it was degraded, the cached `/dev/null`
   descriptor, the package-name buffer and the `envp` copied once at startup. `spawn_kill(pkg)`
   returns a `c_int`: `0` dispatched, positive OS errno, `-1` refused by this module, and the
   `NOT_DISPATCHED` sentinel (`-2`) for the AMS-guard skip. `spawn_path` stages `argv` into a
   fixed `[*const c_char; 8]` on the stack, so there is nothing to reallocate and nothing to
   overflow: an argument list that does not fit is refused, not truncated, and the compile-time
   guard for that is `ARGV_FIXED + KILL_ARGS.len() < ARGV_SLOTS` - the weaker `ARGV_SLOTS >
   ARGV_FIXED` it used to carry would have let a fifth fixed argument make *every* dispatch fail.
   The backend is chosen once, in `DaemonState::new()`. The child runs one extra step before
   `execve`: `close_inherited()` closes every descriptor above stdio, because `fork` copies the
   whole table -- including anything the *launcher* handed us that is not close-on-exec -- while
   `std::process::Command` closes those for us. It costs the reactor nothing (the parent has
   already returned from `fork`), and without it the shipped path would differ from v1.4.0 in
   exactly one way (§10). It has two routes: the `close_range(2)` syscall (Linux 5.9+; Bionic only
   exports the *function* from API 34, four major levels above this project's API 24 floor, so the
   number is issued
   directly), and a blind `close` walk bounded by `RLIMIT_NOFILE` and capped at 4096 for the older
   kernels still in service. The fast path is *checked*, not trusted: after the syscall, our own
   cached descriptor must be closed, and if it is not the sweep falls through to the walk. That
   condition is load-bearing -- with a deliberately wrong syscall number (one that also returns 0)
   the descriptor test passes with the check and fails without it, which is the only way this file
   can know a hardcoded number worked.
2. **`src/main.rs`** gains `mod spawn;`, a `spawner` field on `DaemonState` (both `new()` and the
   test-only `daemon_for_test()`), the D4 banner, and the dispatch loop calls
   `self.spawner.spawn_kill(&pkg)` instead of building a `Command`.
3. **Telemetry**: `"spawned": bool` becomes `"spawn_errno": i32` with the §6.2 semantics, built by
   a `KillRecord::to_json()` and documented in `docs/ARCHITECTURE.md`, not just in the code.
   `spawn_skipped` survives, but as a *derived* field rather than a second stored copy of the same
   fact: `KillRecord::spawn_skipped()` is true when `spawn_errno` is `None` (observe mode, no
   dispatch attempted) or `Some(NOT_DISPATCHED)` (the guard declined). The order matters here - the
   naive rule `== Some(NOT_DISPATCHED)` reports `false` in the mode the daemon ships in by default,
   which is the mistake this clause used to document as intentional. `kill_record_json_reports_which
   _of_its_four_states_spawn_errno_is` asserts the four states and checks the derivation against the
   pipeline's own `!act_mode || ams_protected` for all four combinations; `dispatch_outcomes_are_the
   _codes_the_telemetry_documents` is the separate claim that the *return codes* (`0`, `EINVAL`,
   `-1`) are what the field documentation says they are — and that `0` means the same thing on
   both backends, not only on the shipped one.
4. **Tests** (`src/spawn.rs`, 13 of them, 46 → 60 for the binary): `Backend::parse` accepting only
   the two real names; construction annotating only what it has to; the fork path launching,
   redirecting and being reaped; NUL-in-name refusal; `argv` staging exactness and the
   never-reallocates claim; the missing-binary case as a `cfg`-gated documentation of §6.2; the
   outcome codes being the ones the telemetry claims; a `/dev/null` open that failed degrading to
   `Std` instead of disabling dispatch forever; and the descriptor identity check that §6.5
   explains in full, plus the inherited-descriptor claim above (which fails, reporting `[3]`, if
   the sweep is removed). One more lives beside the record it tests, in `src/main.rs`:
   `kill_record_json_reports_which_of_its_four_states_spawn_errno_is`.
5. **`scripts/benchmark.sh`** samples the daemon's `VmRSS`, its `/proc/<pid>/fd` count and its
   `Threads:` on every iteration, and prints a **drift verdict** rather than a number:
   `STEADY-STATE CENSUS (observe mode): stable at N descriptors / 1 threads over M samples`, or
   `DRIFT -- fds 5..7` with a warning that every latency row above it is now suspect. Flatness is
   the property being asserted (D5's descriptor must appear exactly once and then never move),
   which is why the verdict compares min against max instead of quoting a mean. All three land in
   `--csv`, `--json` (`"drift":N`) and `--markdown`. The verdict **names the daemon mode it
   sampled**, because the default `--observe` never reaches `spawn_kill()`: what it proves is the
   startup descriptor and the thread count, and `--daemon-mode act` is the flag that turns it into
   a real dispatch census — deliberately not the default, because an `--act` benchmark kills the
   apps it is measuring. §6.5 is why the split exists at all, and `docs/ARCHITECTURE.md` gains the
   sentence explaining why RSS and descriptors are reported together (§2(a)).
6. **Deferred, deliberately:** the empty-`envp` change (§3.3) is a measured ~90 µs today and a
   ~500 µs win only if the inherited environment is large. Under D1 it is *not* inside a
   suspended parent, so the §3.3 slope argument mostly evaporates for the shipped path; it stays
   in the probe as a mode, and stays out of this release.

## 8. Acceptance criteria

* **Relative, not absolute, because absolutes do not reproduce (§9).** On the reference device
  with the real target, in one process, the kill-dispatch `[parent]` P50 selected by
  `MINI_LMK_SPAWN=fork` must be at least 2x below the `MINI_LMK_SPAWN=std` P50 of the same run —
  the gap §9 confirms survived all three captures. As a sanity band rather than a gate, the `fork`
  row should land inside 0.5–2.0 ms, which is where every capture of it has been (785, 854, 1,627
  µs from the three `60 cmd vf` runs; 1,885 µs is §3.2's 3 MB row, a different loop).
  *Why this replaced the old clause:* the first draft demanded "under 2,000 µs at 1 GB of daemon
  RSS" as the point of the release. That criterion is unsatisfiable by D1 — §3.2 measured
  35,929 µs at 1.28 GB — and was written when `USEVFORK` was D1. Keeping it would have made the
  shipped design fail its own acceptance test, so the requirement moved from the *spawn path* to
  the *footprint*, where it can be measured (next bullet).
* **The bound that makes that trade legal:** the daemon's `VmRSS` stays under 8 MB under the
  5-minute `benchmark.sh` app-switch traffic, reported beside the fd census (§7.5). D1 buys 2.88x
  over `std` and knowingly gives up RSS-independence: `fork`'s stall grows 19.1x between 3 MB and
  1.3 GB (§3.2). That is a good trade only while the footprint is bounded, so the bound is now
  something the release measures rather than something it assumes — and breaking it is the
  trigger to revisit §5.4, whose implementation is still runnable in `tools/spawnprobe`.
* `MINI_LMK_SPAWN=std` still works and lands in the 2.9–4.7 ms band that §9 documents for the
  `std` row against the real target: 4,681 µs (§3.1, the pre-vendoring capture, no archived
  output), 3,736 µs and 2,868 µs (both archived, `~/durable/vf_cmd_clean*.txt`). So the A/B is
  honest without promising a reproducibility this device does not offer. An earlier draft of this
  clause listed a fourth capture at 5,386 µs; no archived run contains that number, and §10 now
  records the rule it broke.
* The startup banner names the live backend, and announces in the same breath any `MINI_LMK_SPAWN`
  value it refused (`D3`) and any downgrade it took (`D4`): `fork` alone must not be able to mean
  "as designed", "because your request made no sense" and "because `/dev/null` was unavailable".
* Exactly one new descriptor for the process lifetime, opened in `DaemonState::new()` before the
  reactor starts and **stable across dispatches**; no new thread; no allocation in steady state
  inside the dispatch loop (the fixed `argv` cannot reallocate, and the package-name buffer grows
  only when a longer name than any seen before arrives). Two gates, because one script cannot
  cover both halves: the *per-dispatch* half is asserted in-process by
  `the_cached_descriptor_is_ours_and_survives_every_dispatch` (twenty real `fork`+`dup2`+`execve`
  dispatches, identity rather than a count, for the reason in §6.5), and the *long-run drift* half
  by `scripts/benchmark.sh`, whose descriptor count and `Threads:` must be **flat across every
  sample of the run** (`drift == 0`). The fd appears once at startup and the claim is about it
  never moving, so an absolute count would be a number nobody can interpret without knowing the
  config. Nothing in that sentence covers descriptors the child *inherits*: `fork` copies the
  launcher's table wholesale, so `close_inherited()` closes everything above stdio in the child
  before `execve` (item 1, §10) and
  `the_child_carries_no_descriptor_the_parent_did_not_mark_close_on_exec` asserts it with a raw
  non-`CLOEXEC` descriptor opened in the test process. (`benchmark.sh` runs `--observe` unless given `--daemon-mode act`, and an observe-mode
  verdict therefore says "steady-state", not "dispatch" — see §7.5; "No new descriptor" was the
  original wording of this bullet, which contradicted D5 and is gone.)
* `cargo clippy --release --all-targets` stays at 0 warnings, `cargo fmt` at or below its baseline
  (119 hunks committed, 116 after this change — the files this release touches had unformatted
  lines of their own before it, and `src/spawn.rs` contributes zero), and `cargo test` only ever gains tests (46 → 60).
* `sh scripts/check-benchmark-sh.sh` passes: the benchmark script's reporting half is executed
  under `dash` as well as bash and must print byte-identical reports, because a bash-only check
  had already passed a script that could not finish on the device (§9).

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
* **A knob whose set of valid values shrinks must say so.** `MINI_LMK_SPAWN=spawn` was a real
  backend for three days. After D2 it was not, and the code quietly resolved it to `fork` — so a
  device left with that value in `service.sh` would have reported "fork" in the banner, which is
  true, while the operator believed they were measuring the `USEVFORK` path, which is not. The fix
  is one line of output naming the ignored value (D3), and it is the same class as every other
  silent fallback this file has recorded: the failure is not a crash, it is a *reading you believed
  that the machine never made*.
* **Do not buy insurance against a footprint you have not measured.** §3.2's columns (3 MB, 259 MB,
  1.3 GB) sat in this file next to a decision before anyone put the daemon's actual resident set
  beside them: 3,876–4,020 kB under live traffic, P50 1,099 kB PSS, README budget < 4 MB, tables
  bounded by device process and package counts. The 26x figure that drove D1 is true at 1.3 GB and
  irrelevant at 4 MB. Whenever a measurement is used to justify a design, quote the operating point
  in the same breath as the curve.
* **Absolutes do not reproduce — and neither does one published ranking.** Three captures of
  the *same mode* on the same device (`60 cmd vf`, API 34, `Threads: 1`, `AnonHugePages` and
  `Swap` 0 kB, load average 12.8–14.7; the first with the pre-vendoring binary, the next two
  minutes apart with this crate): every `[parent]` P50 landed at 0.53–0.80x of §3.1's published
  value in the first re-run and 0.40–0.61x in the second. `fork` went 1,627 → 854 → 785 µs,
  `USEVFORK` 1,387 → 1,019 → 646 µs, `std` 4,681 → 3,736 → 2,868 µs, the VM-only control
  272 → 167 → 137 µs. More damaging than the drift: the ratio §3.1 quotes, "`USEVFORK` is a
  further 1.17x over `fork`", measured 0.84x then 1.22x. At shipping footprint those two
  backends are within device noise of each other, and their *order* is not a reproducible fact.
  What survived all three captures is the ≥ 2x gap to `std` — which is D1's entire rationale,
  and the reason §8's latency gate is written as a ratio to a same-run `std` measurement — and
  the §3.2 footprint sweep (`fork` 19.1x, `USEVFORK` flat), which is a true statement about
  mechanism and, at this daemon's bounded footprint, not a reason to buy anything. Two rules: do
  not promote a same-footprint ranking of two backends less than ~1.2x apart, and never quote
  `std` as a single number (the same capture has
  P50 2,868 µs against P99 6,644 µs). Read any fresh absolute as "this device, right now" —
  which is why every run now prints its own load average, RSS, thread count, API level,
  `AnonHugePages`, `Swap` and `Seccomp` beside the rows.

* **An example record is a claim, and it has to be checked against the derivation.** The
  `simulated_kill` row in `docs/ARCHITECTURE.md` carried `"spawn_errno": null` beside
  `"spawn_skipped": false` for a whole release cycle. That pair is not producible: `spawn_skipped` is
  derived from `spawn_errno`, and `null` means "observe mode, nothing attempted", which *is* skipped.
  The example had been correct under v1.4.0's two independent fields and went impossible when the
  fields were unified - the same trap as any hand-typed JSON in prose. Derive the example from the
  code, or assert it in a test (`kill_record_json_reports_which_of_its_four_states_spawn_errno_is`
  now pins all four rows).
* **A gate that accepts more than the target does is not a gate.** `scripts/benchmark.sh`
  begins `#!/system/bin/sh` — toybox ash on the device — and every verification round up to the
  last one checked it with `bash -n`, which tests syntax while the shells disagree about
  substitution at *run* time. The census verdict line used `${CENSUS_KIND^}`: bash upper-cases
  the first letter, ash aborts with `Bad substitution` (exit 2) — after all the measurement work
  was done, so an on-device run printed its tables and died before its verdict. `dash -n` catches
  nothing either (exit 0); the only check that sees this class of bug is *executing* the file
  under a shell that is not bash. `scripts/check-benchmark-sh.sh` now does that in CI: synthetic
  samples, all four output formats, both verdict arms, byte-identical output required from dash
  and bash. Against the pre-fix script it reports 13 failures; against this one, none. Worth
  recording: the review pass that found the duplicated table suggested rewriting that same line as
  `${CENSUS_KIND^^}`, which would have deepened the bug it was cleaning up. A simplification is
  only safe once someone has established what the target accepts.

## 10. Claims that must not be repeated

Kept because they were in a draft of this file, or in a pitch, and are now known to be false:

| claim | why it is false |
|---|---|
| "`fork` is 2.55x cheaper than `std`, therefore `fork` is the backend" | the ratio is wrong (2.88x at shipping footprint, and the *order* of the top two backends flips between captures, §9) and the reasoning was wrong in the other direction too — `fork`'s stall is a function of our own RSS (19.1x from 3 MB to 1.3 GB). D1 ships `fork` anyway, because the RSS term is bounded near 4 MB and §8 makes that bound a measured criterion |
| "`fork`+`dup2`+`execve` is a rejected option" (this file's own §5.4, first draft) | reversed by D1. The rejection was of `fork`-*as-primary-because-cheap*; what is actually rejected now is carrying it as a *fallback* to a `posix_spawn` primary (§5.4), which is how the same code ended up on both sides of the table |
| "`posix_spawn` ties `fork`, so it is not worth it" | as a measurement, false: the tie was Bionic secretly running `fork()`, and with `USEVFORK` it is 1,375–1,537 µs at every footprint vs 1,885–35,929 µs. As a conclusion it happened to be right, for a reason nobody had found yet — see the next row |
| "therefore `posix_spawn` + `USEVFORK` must be the primary backend" | the flat curve only pays where the parent is big; at 3 MB the gap to `fork` is inside capture-to-capture noise (§9), while the cost is a permanent `dlsym` + fallback pair (§5.4) for symbols that do not exist before API 28 |
| "`std::process::Command` on Android is vfork-based / RSS-immune" | strace shows plain `clone` with no `CLONE_VM`; in-process it grows 5,277 → 90,473 µs |
| "`Command::spawn` doubles the FD count via a socketpair" | zero `socket`/`socketpair` in a syscall-filtered strace; `fds_during_spawn=4` |
| "`addopen` leaks a parent descriptor" | `fds_before=5 fds_after=5` over 80 dispatches, including the failed-exec case; it loses on cost, not correctness |
| "`RTLD_DEFAULT` is missing from `libc` for Android and is wrong on LP32" | `libc` defines it per ABI, correctly (`b32` `-1`, `b64` `null`) |
| "`spawn_errno` will let us tell `ENOENT` from `EAGAIN`" | exec failure is asynchronous (`rc=0` + exit `127`); see §6.2 |
| "the vfork window scales with the child binary" | 1,272 µs for the 46 KB `cmd` vs 1,342 µs for a 1.6 KB `true` — 29x the size, 6 % apart, larger one faster, while the children's own lifetimes were 18.9 ms and 30.4 ms |
| "POSIX_SPAWN_USEVFORK does nothing" (from the glibc article) | true on glibc ≥ 2.24, false on Bionic, where the branch is before the actions check |
| "glibc and Bionic will behave the same in a `posix_spawn` test" | on the host, `posix_spawn` accepted three `adddup2` file actions and ignored them — the child kept the test harness's capture pipe on fd 1 and 2 while the `fork` path redirected all three correctly. The same call is a no-op and an implementation depending on which libc compiled it (D5, §5.4) |
| "`posix_spawnattr_getflags` reading back 0x40 proves the vfork path was taken" | the flag is *stored* even where it is ignored (verified on glibc, which no-ops it by design). The readback is a memory test; only the latency shape is evidence, and §9 shows that shape is noisy |
| "a 60–110 µs dispatch is achievable" | below the cost of `exec`-ing any dynamic binary here; the no-suspension control is 272 µs |
| "this fixes dropped logcat events / a 16.6 ms frame budget" | 0.21 dispatches/s against a pipe that holds ~392 s of events, and this is not the UI thread |
| "`fork`+`dup2`+`execve` is interchangeable with `std::process::Command`, so the backends differ only in cost" | no, and the difference is not in the latency column: `fork` copies the **entire descriptor table**, close-on-exec bits and all. Every descriptor the daemon opens itself is `O_CLOEXEC`/`EPOLL_CLOEXEC`/`IN_CLOEXEC` so those never reach `cmd`, but a descriptor inherited from the *launcher* is unmarked, and `Command` closes those in the child while our fork path handed them over — measured on the host, a raw `open()` fd appeared as fd 3 in the forked child and in no child of `Command`. v1.5.0 now sweeps the child's table before `execve`; the claim was false for the first draft of this release, which is why it is in this table rather than a footnote |
| "a capture is safe to quote because it came off a run" | no. One acceptance band here carried a *fourth* `std` capture, 5,386 µs, which appears in no archived output and in no earlier commit — exactly the ephemeral-`/tmp` provenance Appendix A warns about, reproduced in the section that exists to prevent it. Quote the file a number came from, or drop the number |

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
adb shell /data/local/tmp/spawnprobe 30  cmd vfsweep  # 3.2 the footprint sweep that bounds D1
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

One structural result the vendored `ladder` mode makes visible: the `posix_spawn` cost cliff is a
**step** at "you passed a non-NULL `file_actions` object", not a slope per action — empty 753, one
`dup2` 755, two 727, three 747 µs, against 547 µs with `NULL` actions (`true`, n=40, contended
device, so read the pattern, not the absolutes: see §9's reproducibility bullet). That is §6.3's
branch, and the reason §5.4's "we were paying for `fork()` anyway" objection is not a measurement
artefact: Bionic runs `fork()` the moment you ask for file actions, unless forced. `fork` pays that
cost knowingly, and with no deprecated flag left to re-verify.

Two things the harness no longer settles, so nobody should look for them here. It is not an A/B of
the daemon's own backends — D1 ships `fork`, and `MINI_LMK_SPAWN=std|fork` against the daemon is
that A/B. And its `posix_spawn` rows are §5.4's evidence, not a path the product takes. It builds
and runs on the host too -- `cargo run --release --manifest-path tools/spawnprobe/Cargo.toml -- 5
true vf` -- which is how §6.4's buffer-size bug surfaced: glibc's `posix_spawnattr_t` is 336 B
(measured, aarch64 glibc 2.43) and the probe had been handing itself 8. Off-device the `true`
target resolves to `/bin/true` and the `cmd` target is refused, because timing a path that cannot
exist would print failure latencies that read like a backend table.

Raw captures are still not archived — `/tmp` on this machine reclaims them, and `/tmp/vf_cmd.txt`
did vanish mid-write — but the asymmetry is gone: regenerating a table is now a `git clone` plus
one build command, not a reconstruction from whoever was in the room.
