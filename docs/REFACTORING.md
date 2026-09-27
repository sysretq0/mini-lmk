# Semantic Refactoring Plan (post-split audit)

Goal: catch unintended AI-generated behaviour. The structural split is done; this
pass audits *what the code does*, one finding at a time, with the operator
confirming intent before anything changes.

Method: every finding gets an ID, a one-line question, and a verdict recorded
here (`INTENDED` / `BUG` / `FIXED`). No code changes before the verdict.

## Findings

| ID | Location | Suspicion | Question | Verdict |
|----|----------|-----------|----------|---------|
| F1 | `config.rs` parse_str | `min_oom_score_adj` silently clamped to 500..=900 | Is this clamp range intended, or should out-of-range be rejected/logged? | open |
| F2 | `config.rs` parse_str | Unparseable values silently keep the previous value (fail-open) | Intended? Or should a bad line warn once? | open |
| F3 | `kill.rs` scan_candidates | `lru_pos` sentinel is `99`; a user setting `fg_lru_max_depth > 99` makes even packages *absent* from the LRU permanently protected | Bug (config can silently disable harvesting)? | open |
| F4 | `kill.rs` scan_candidates | Screen-on with fg session > 300s forces `t_idle = 60s`, ignoring the user's `t_idle_sec` entirely | Intended hard override? | open |
| F5 | `kill.rs` dispatch | Candidate whose PIDs all return unreadable adj gets `oom_adj = 0` → `ams_protected = true` → never killed | Intended conservative fail-safe? | open |
| F6 | `events.rs` on_proc_start | `pkg_to_uid` is first-writer-wins; a subprocess racing the main process with a different uid wins the slot | Acceptable approximation? | open |
| F7 | `daemon.rs` run loop | inotify event contents ignored — any event in config dir triggers full reload | Intended (cheap, rare)? | open |
| F8 | `procfs.rs` check_mem_critical | Reads PSI, falls back to meminfo each call — two file reads per pipeline pass | Fine (once per pass) or cache? | open |

## Loop-optimization candidates (all need verdicts too)

| ID | Candidate | Assessment |
|----|-----------|------------|
| L1 | `scan_candidates`: `pids.retain(|p| live.contains(p))` is O(n²) | n = pids per package, ≤ ~10. Not worth it. |
| L2 | `seed_initial_state`: serial /proc scan | One-shot at startup. Not worth it. |
| L3 | `detect_system_components`: 5 serial `cmd` subprocesses at startup | Could parallelize; startup-only, adds threads. Likely skip. |

## Status

- Structural split: DONE (main.rs 2088 → 5 modules, 64 tests green).
- Semantic audit: F1-F8 open, awaiting operator verdicts.

## Pass 2 findings (append-only; Pass 1 verdicts above are final)

| ID | Location | Suspicion | Question | Verdict |
|----|----------|-----------|----------|---------|
| F9 | `parser.rs` parse_proc_start | Fast path accepts `pid = 0` (`[0,0,0,com.x,…]` parses); fallback path rejects `pid == 0` | Garbage events index swapper's PID into `pid_to_pkg`. Bug or accept? BUG — fast path now rejects `pid == 0` (matches fallback); regression test added |
| F10 | `parser.rs` | Three different package-name validation rules: `parse_resume_activity` (any ASCII letter, no `-`/`{`), `parse_proc_start` fast path (empty/`-` only), `is_proc_name` (strict charset) | Unify into one predicate, or leave (unifying risks dropping real vendor events)? PARTIAL — all three final-pkg acceptance checks route through `is_proc_name`; functions kept separate |
| F11 | `telemetry.rs` open_file/rotate | `chmod 0666` on log file — world-writable (parent dir still gates access) | Was another uid intended to *write* the log, or only read? Suggest `0644`. BUG — `0666` → `0644` on log and `.old` |
| F12 | `events.rs` on_resume_activity | <500ms trampoline eviction requires `prev_dur_ms > 0`, which is only set when the previous app already had an `alive_apps` anchor *and* verified uid — a first-visit trampoline (unknown uid, or anchor inserted fresh on exit) is never evicted from `fg_lru` | Bug or acceptable (first-visit duration genuinely unknown)? INTENDED — duration genuinely unknown, self-healing on next departure; left as-is |
| F13 | `daemon.rs` run loop | Logcat child death (EPOLLHUP / pipe EOF / reaped child) -> `fatal()` -> whole daemon exits, relying on the module supervisor | Intended design, or respawn logcat in place? | INTENDED — fail-fast + external supervision is the documented architecture (ARCHITECTURE.md); fresh process re-runs `seed_initial_state()`, in-place respawn cannot reconcile missed events. Verified `run-daemon.sh` supervises. |
| F14 | `daemon.rs` run loop | `epoll_wait(-1)` infinite timeout; dispatch children reaped only when epoll returns — on a quiet device a fresh `cmd` child stays a zombie until the next event (hours under Doze) | Intended, or bound with a timeout? | BUG — timeout `60_000` ms (chosen by wakeup cost, not urgency); `nfds == 0` path already runs the reap loop |
| F15 | `kill.rs` scan_candidates | Screen-off harvest hard-sets `t_idle_effective_sec` to 30/60, ignoring `t_idle_sec` — same class as F4 | Mode override or ceiling? | BUG — same class as F4; `t_idle_sec.min(30)`/`.min(60)` + regression test |
| F16 | `kill.rs` scan_candidates | Screen-off harvest forces `effective_lru_depth = 1`, ignoring `lru_protect_depth` | Mode override or ceiling? | BUG — `lru_protect_depth.min(1)`: depth 0 stays 0; intent-revealing, observationally equivalent for pos-0 (== current_fg, already excluded) |
| F17 | `config.rs` parse_str | `max_kills_per_pass = 0` silently floored to 1 (`num.max(1)`) — eviction cannot be disabled via config, and the floor is silent (F1 pattern: clamp without a word) | Was "minimum 1" intended, or should 0 be an off switch (`.take(0)` already handles it; `mem_critical_percent = 0` is documented as a disable)? INTENDED-FIXED — floor kept (0 would silence all kill telemetry and read like a dead pipeline; --observe is the pause); one-line log added like F1 + regression test. Live-pause gap noted as F18, deferred. |
| F18 | (capability gap, no code defect) | `act_mode` is constructor-time only — no way to pause eviction via live config without restarting and discarding `alive_apps`/`fg_lru`/`pkg_to_pids` state | Build a dedicated live-reloadable `eviction_enabled` key? | DEFERRED — must be its own explicitly-named boolean (SIGKILL_MIN_ADJ precedent: one control must not silently move another), surfaced in `[CONFIG] Active` and `config_reload`. Not ordered; recorded so the gap is not forgotten. |
| F19 | `kill.rs` scan_candidates | `if is_game || is_low_mem { 10 }` hard-sets `t_idle_effective_sec`, ignoring `t_idle_sec` — the fourth instance of the F4 pattern in the same expression chain (F4/F15 fixed, this branch missed). A user with `t_idle_sec = 5` gets *relaxed* to 10s during game/low-mem escalation, inverting the escalation | Same rule (`.min(10)`), or is a fixed 10s grace window during escalation semantically different from harvest? BUG — same rule as F4/F15/F16; `t_idle_sec.min(10)` + regression test. For any t_idle_sec >= 10 behavior is unchanged; the fixed-override reading fails because it relaxes aggressive presets under pressure | |
| F20 | `events.rs` emit_bg_summary | Tabular label says `rss_delta=` but `spawn_rss_kb` only accumulates on spawn — `on_proc_died` never subtracts, so deaths do not reduce it (JSON key `spawn_rss_kb` is honest; the tabular wording overstates) | Relabel tabular to `spawn_rss=`, or subtract death RSS (requires reading statm before the PID disappears — racy) BUG (label) — tabular relabeled to `spawn_rss=` matching the honest JSON key; dead sign branch removed. Subtraction skipped: death-time statm read races PID reuse (same hazard the SIGKILL revalidation exists for) | |
| F21 | `package/axmanager/customize.sh` | `chmod 777 "$MODPATH/mlmk/logs" 2>/dev/null || chmod 755 ...` — world-writable logs *directory*, worse than F11’s world-writable file: any uid can unlink/rename operations.log and substitute a forged one; the `|| 755` fallback shows the author was unsure | Was 777 for shell-uid dev runs (installer creates the dir root-owned; a shell-uid daemon could not create the log under 755)? If root is the only runtime uid, fix is 755, consistent with F11’s “nobody else writes” verdict FIXED — 755, consistent with F11; runtime is always root via service.sh. Related: run-daemon.sh argument-forwarding removed, `--act` hardcoded (the forward path was unused complexity) |

| F22 | `daemon.rs` run loop (amends F14) | F14's unconditional `60_000` ms timeout broke the documented "0 idle CPU wakeups" invariant: a deep-idle phone woke 1,440×/day with zero activity, purely to bound a zombie that the dispatched `cmd`'s own `am_proc_died` event already reaps in ~30 ms | Poll forever, or bound only the window a dispatch opened? BUG-FIXED — `Spawner::reaps_pending` counts dispatched children; `epoll_wait` sleeps 60 s only while it is nonzero, `-1` otherwise; `reap_terminated_children` drains it when `waitpid` returns <= 0. Bounded zombie reaping, zero idle wakeups restored |
| F23 | `config.rs` parse_str | F1/F17 clamp/floor warnings were unconditional `stderr()` writes inside `parse_str`, but `DaemonState::new()` loads `daemon.conf` twice (quiet preload + `reload_configs`) — every clamped value printed its `[CONFIG]` notice twice at startup, ignoring `quiet` | Thread the existing `quiet` flag through: `parse_str(text, quiet)`, warnings gated on `!quiet`, `load_from_file` passes its own `quiet` down. One notice per load path, preload silent |
| F24 | `kill.rs` SIGKILL revalidation | Revalidating `oom_score_adj >= 900` alone is uid-blind: if a tracked PID exited and the kernel recycled it before `am_proc_died` drained, the adj read describes the *new* holder. Reviewer proposed `fstat` on the open `oom_score_adj` fd — measured on device (KernelSU, Android 12): files inside `/proc/<pid>` are uid-0-owned even for a uid-10080 app, so that primitive checks root, not the app. The directory inode itself carries the process uid (fgres production gate, `stat /proc/<pid>` -> 10080) | BUG-FIXED — `procfs::proc_uid` (st_uid of `/proc/<pid>`) + `proc_is_app_uid` (uid >= 10000, same literal events.rs gates alive_apps on), ANDed into the per-PID SIGKILL revalidation; None (pid gone) refuses the fast path like an unreadable adj does |

## Pass 1 closure (recorded after the table above was frozen)

F1-F8 all verdicted and applied (commit `aa66226`): F1 INTENDED+log, F2 INTENDED,
F3 BUG (sentinel `99` → `usize::MAX` + regression test), F4 HALF BUG
(`60` → `t_idle_sec.min(60)`), F5/F6/F7 INTENDED, F8 INTENDED (measured:
`check_mem_critical` p50 = 6.4 µs, cheaper than one `oom_score_adj` read; no cache).
L1-L3 skipped as assessed. The Pass 1 table rows still read "open" — left as-is
per append-only policy; this section is the authoritative record.
