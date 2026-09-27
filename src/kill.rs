// Copyright (C) 2026 sysretq0
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-only

//! The eviction pipeline: pick idle candidates, dispatch each through the
//! root SIGKILL fast path or the AMS fallback, and record what happened.

use crate::daemon::DaemonState;
use crate::procfs::{check_mem_critical, proc_is_app_uid, read_oom_score_adj, read_statm_rss_kb};
use crate::telemetry::{escape_json, tabular_row};
use crate::{spawn, Candidate, SIGKILL_MIN_ADJ};
use std::cmp::Reverse;

/// One kill decision, in the shape the log records it.
///
/// A struct rather than a 12-slot `format!` inside the dispatch loop because the interesting part
/// of this record is *which of its four states* `spawn_errno` is in, and that deserves a test of
/// its own instead of being asserted by reading a format string.
pub(crate) struct KillRecord<'a> {
    ts: u64,
    event: &'static str,
    pkg: &'a str,
    pids: &'a [u32],
    rss_freed_est_kb: u64,
    reason: &'static str,
    idle_sec: u64,
    lru_pos: usize,
    /// `None` = this build cannot dispatch (observe mode). `Some(NOT_DISPATCHED)` = the guard
    /// declined. `Some(0)` = launched. `Some(n > 0)` = refused, synchronously.
    spawn_errno: Option<libc::c_int>,
    oom_score_adj: i32,
    ams_protected: bool,
    /// Which dispatch the decision chose: `"ams"` (`cmd activity kill`) or `"sigkill"`
    /// (root fast path). Recordable in `--observe` too, where it states what `--act` would do.
    method: &'static str,
}

impl KillRecord<'_> {
    /// The v1.4.0 field, which consumers still read. Derived from [`KillRecord::spawn_errno`]
    /// rather than carried beside it, because the two are the same fact and a stored pair can
    /// disagree while a derived one cannot:
    ///
    /// ```text
    /// spawn_errno      spawn_skipped      meaning
    /// None             true               observe mode: this build cannot dispatch
    /// Some(-2)         true               a candidate was chosen, the AMS guard declined it
    /// Some(0 | n > 0)  false              a dispatch was attempted
    /// ```
    ///
    /// The `None` row is why the obvious rule `== Some(NOT_DISPATCHED)` is wrong: it reports `false`
    /// in exactly the mode this daemon ships in by default.
    fn spawn_skipped(&self) -> bool {
        matches!(self.spawn_errno, None | Some(spawn::NOT_DISPATCHED))
    }
    fn to_json(&self) -> String {
        // v1.4.0 wrote a `spawned` boolean here. It is gone rather than kept alongside, because the
        // two would disagree in exactly the case that matters: a child that launched and then
        // failed to `exec` reports success to us and exit 127 to nobody (roadmap 6.2).
        let spawn_errno = match self.spawn_errno {
            None => "null".to_string(),
            Some(v) => v.to_string(),
        };
        format!(
            r#"{{"ts":{},"event":"{}","pkg":"{}","pids":{:?},"rss_freed_est_kb":{},"reason":"{}","idle_sec":{},"lru_pos":{},"spawn_errno":{},"oom_score_adj":{},"ams_protected":{},"spawn_skipped":{},"method":"{}"}}"#,
            self.ts,
            self.event,
            escape_json(self.pkg),
            self.pids,
            self.rss_freed_est_kb,
            self.reason,
            self.idle_sec,
            self.lru_pos,
            spawn_errno,
            self.oom_score_adj,
            self.ams_protected,
            self.spawn_skipped(),
            self.method,
        )
    }
}

/// Kill-decision telemetry naming, shared by the dispatch path and its test.
/// Returns `(json_event, columnar_tag, detail_suffix)`.
///
/// The v1.4.0 `spawn_skipped` boolean used to be a fourth element here. It is now derived from
/// `spawn_errno` by [`KillRecord::spawn_skipped`], which is the same fact stated once: this
/// function's `!act_mode || ams_protected` and that match's `None | Some(NOT_DISPATCHED)` cover
/// exactly the same four cases, and a copy that can drift out of its twin is not worth the
/// second field in the record.
fn kill_telemetry_parts(act_mode: bool, ams_protected: bool) -> (&'static str, &'static str, &'static str) {
    let (event_name, tag, sim_suffix) = match (act_mode, ams_protected) {
        (true, false) => ("kill", "KILL", ""),
        (true, true) => ("kill_skipped", "KILL_SKIP", " (ams_protected: spawn skipped)"),
        (false, false) => ("simulated_kill", "SIM_KILL", " (simulated)"),
        (false, true) => ("simulated_kill", "SIM_KILL", " (simulated, ams_protected: spawn skipped)"),
    };
    (event_name, tag, sim_suffix)
}

/// Dispatch method for an unprotected candidate: direct SIGKILL only for root and
/// fully cached processes, AMS for everything else.
fn kill_method(is_root: bool, ams_protected: bool, oom_adj: i32) -> &'static str {
    if is_root && !ams_protected && oom_adj >= SIGKILL_MIN_ADJ {
        "sigkill"
    } else {
        "ams"
    }
}

impl DaemonState {
    /// The scan half of the pipeline: decide *who* is evictable. Pure bookkeeping —
    /// no signals, no dispatch. Takes `&mut self` only for the dead-PID/package
    /// reconciliation it performs while scanning. Also reconciles the process tables against PIDs and
    /// packages that died since the last pass.
    fn scan_candidates(&mut self, now_epoch: u64, is_game: bool, is_low_mem: bool) -> Vec<Candidate> {
        // F16: ceiling, not override — a user who set depth 0 keeps depth 0 with the
        // screen off; harvest may only tighten protection, never increase it.
        let effective_lru_depth = if !self.screen_on && self.config.screen_off_harvest {
            self.config.lru_protect_depth.min(1)
        } else {
            self.config.lru_protect_depth
        };

        // F19: same rule as F4/F15 — even the emergency path may only tighten the user's
        // baseline, never loosen it. For any t_idle_sec >= 10 this is still exactly 10s.
        let t_idle_effective_sec: u64 = if is_game || is_low_mem {
            self.config.t_idle_sec.min(10)
        } else if !self.screen_on && self.config.screen_off_harvest {
            let off_dur_sec = self
                .screen_off_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or_default();
            if off_dur_sec > 60 {
                // F15: same rule as F4 — a ceiling, not an override. Screen-off can only
                // tighten the user's baseline, never loosen it below what they set.
                self.config.t_idle_sec.min(30)
            } else {
                self.config.t_idle_sec.min(60)
            }
        } else {
            let fg_dur_sec = self
                .current_fg
                .as_deref()
                .and_then(|p| self.alive_apps.get(p))
                .map(|&anchor| now_epoch.saturating_sub(anchor) / 1000)
                .unwrap_or_default();
            if fg_dur_sec > 300 {
                // F4: a relaxation, not an override — ceilings the configured idle time at
                // 60s instead of replacing it, so a preset below 60 is never silently slowed.
                self.config.t_idle_sec.min(60)
            } else {
                self.config.t_idle_sec
            }
        };

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut dead_pids = Vec::new();
        let mut dead_pkgs = Vec::new();

        for (pkg, &last_active) in &self.alive_apps {
            if self.is_excluded(pkg) {
                continue;
            }

            // F3: sentinel for "absent from the LRU". MAX, not 99 — `lru_protect_depth` is
            // unclamped, so any finite sentinel collides with a depth set above it and
            // permanently protects every package that is not in the LRU at all.
            let lru_pos = self.fg_lru.iter().position(|x| x == pkg).unwrap_or(usize::MAX);
            if lru_pos < effective_lru_depth {
                continue;
            }

            let idle_sec = now_epoch.saturating_sub(last_active) / 1000;
            if idle_sec < t_idle_effective_sec {
                continue;
            }

            let mut total_rss_kb = 0u64;
            let mut live_pids = Vec::new();
            if let Some(pids) = self.pkg_to_pids.get(pkg) {
                for &pid in pids {
                    let rss = read_statm_rss_kb(pid, self.page_size_kb);
                    if rss > 0 {
                        total_rss_kb += rss;
                        live_pids.push(pid);
                    } else {
                        dead_pids.push(pid);
                    }
                }
            }

            if live_pids.is_empty() || total_rss_kb == 0 {
                dead_pkgs.push(pkg.clone());
                continue;
            }

            candidates.push(Candidate {
                pkg: pkg.clone(),
                live_pids,
                total_rss_kb,
                idle_sec,
                lru_pos,
            });
        }

        // Opportunistic reconciliation: purge dead PIDs and exited packages
        for pid in &dead_pids {
            self.pid_to_pkg.remove(pid);
        }
        for pkg in &dead_pkgs {
            self.alive_apps.remove(pkg);
            self.pkg_to_pids.remove(pkg);
        }
        for cand in &candidates {
            if let Some(pids) = self.pkg_to_pids.get_mut(&cand.pkg) {
                pids.retain(|p| cand.live_pids.contains(p));
            }
        }

        candidates.sort_by_key(|cand| Reverse(cand.total_rss_kb));
        candidates
    }

    /// Signal or AMS-kill one confirmed candidate, emit its record, and update the
    /// `alive_apps` anchor. The OOM gate re-read is deliberately here and not in the
    /// scan: a package promoted to foreground between scan and dispatch must not be
    /// signalled, so the check has to be as close to `kill(2)` as possible.
    fn dispatch_candidate(
        &mut self,
        cand: &Candidate,
        now_epoch: u64,
        oom_adj: i32,
        ams_protected: bool,
        is_game: bool,
        is_low_mem: bool,
    ) {
        let mut method = kill_method(self.is_root, ams_protected, oom_adj);

        let reason = if is_game {
            "game_mode_escalation"
        } else if is_low_mem {
            "low_memory_escalation"
        } else {
            "idle_expired"
        };

        let rss_mb = cand.total_rss_kb / 1024;

        // D6: an errno where v1.4.0 had a boolean. Only *pre-clone* failures can land here:
        // `fork` reports success for a binary that then fails to `exec`, so a missing
        // `/system/bin/cmd` is a child exit 127 that never reaches this field (roadmap 6.2).
        // Reading a `0` here as "the kill happened" is the lie D6 removes.
        let spawn_errno = if !self.act_mode {
            None
        } else if ams_protected {
            Some(spawn::NOT_DISPATCHED)
        } else if method == "sigkill" {
            // SIGKILL has no AMS gate, so the OOM check is ours and it is re-done
            // here, immediately before signalling: a package promoted to foreground
            // since the scan must not be signalled. Every live PID must revalidate
            // at or above SIGKILL_MIN_ADJ — an unreadable adj is unverified, not
            // exempt — and still belong to an app uid (F24): a PID that died and was
            // recycled by the kernel before am_proc_died drained would otherwise be
            // signalled by uid-blind adj alone. ESRCH after signal = exited in the
            // gap = a successful kill.
            let confirmed = cand
                .live_pids
                .iter()
                .all(|&p| proc_is_app_uid(p)
                    && read_oom_score_adj(p).is_some_and(|adj| adj >= SIGKILL_MIN_ADJ));
            let signalled = confirmed
                && cand.live_pids.iter().all(|&pid| {
                    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    rc == 0
                        || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                });
            if signalled {
                Some(0)
            } else {
                // Unverified adj or the kernel refused the signal (confined root,
                // EPERM): fall back to AMS, which stops the package without the
                // rapid respawn a direct signal would cause.
                method = "ams";
                Some(self.spawner.spawn_kill(&cand.pkg))
            }
        } else {
            Some(self.spawner.spawn_kill(&cand.pkg))
        };

        let (event_name, tag, sim_suffix) = kill_telemetry_parts(self.act_mode, ams_protected);

        let record = KillRecord {
            ts: now_epoch,
            event: event_name,
            pkg: &cand.pkg,
            pids: &cand.live_pids,
            rss_freed_est_kb: cand.total_rss_kb,
            reason,
            idle_sec: cand.idle_sec,
            lru_pos: cand.lru_pos,
            spawn_errno,
            oom_score_adj: oom_adj,
            ams_protected,
            method,
        };
        self.telemetry.emit_with(
            || {
                let detail = format!(
                    "rss={}MB  idle={}s  adj={}  lru={} [{}] {}{}",
                    rss_mb,
                    cand.idle_sec,
                    oom_adj,
                    cand.lru_pos,
                    reason,
                    sim_suffix,
                    if method == "sigkill" { "  [sigkill]" } else { "" }
                );
                tabular_row(now_epoch, tag, &cand.pkg, &detail)
            },
            || record.to_json(),
        );
        self.telemetry.flush();

        if ams_protected {
            if let Some(anchor) = self.alive_apps.get_mut(&cand.pkg) {
                *anchor = now_epoch;
            }
        } else {
            self.alive_apps.remove(&cand.pkg);
        }
    }

    /// The full pass: scan, then dispatch the heaviest `max_kills_per_pass` survivors.
    pub(crate) fn evaluate_reaping_pipeline(&mut self, now_epoch: u64) {
        let is_game = self
            .current_fg
            .as_ref()
            .map(|p| self.games.contains(p))
            .unwrap_or(false);
        let is_low_mem = check_mem_critical(self.config.mem_critical_percent);

        let mut candidates = self.scan_candidates(now_epoch, is_game, is_low_mem);

        for cand in candidates.drain(..).take(self.config.max_kills_per_pass) {
            let oom_adj = cand
                .live_pids
                .iter()
                .map(|&p| read_oom_score_adj(p).unwrap_or(0))
                .min()
                .unwrap_or(0);
            let ams_protected = oom_adj < self.config.min_oom_score_adj;
            self.dispatch_candidate(&cand, now_epoch, oom_adj, ams_protected, is_game, is_low_mem);
        }
    }

    /// Reap every child that exited without blocking; a dead logcat while still
    /// running is fatal, because the event stream the daemon is built on is gone.
    pub(crate) fn reap_terminated_children(&mut self) {
        let logcat_pid = self.logcat_child.as_ref().map(|c| c.id() as libc::pid_t).unwrap_or(-1);
        unsafe {
            loop {
                let mut status = 0;
                let reaped = libc::waitpid(-1, &mut status, libc::WNOHANG);
                if reaped <= 0 {
                    break;
                }
                if reaped == logcat_pid {
                    if crate::RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
                        self.fatal(format_args!(
                            "Persistent logcat stream died (reaped via WNOHANG). Exiting."
                        ));
                    }
                    break;
                }
            }
        }
        // F22: reaching here means waitpid found nothing left, so any dispatched child is reaped
        // and the reactor may go back to sleeping at -1.
        self.spawner.note_children_drained();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kill_telemetry_parts() {
        assert_eq!(kill_telemetry_parts(true, false), ("kill", "KILL", ""));
        assert_eq!(
            kill_telemetry_parts(true, true),
            ("kill_skipped", "KILL_SKIP", " (ams_protected: spawn skipped)")
        );
        assert_eq!(
            kill_telemetry_parts(false, false),
            ("simulated_kill", "SIM_KILL", " (simulated)")
        );
        assert_eq!(
            kill_telemetry_parts(false, true),
            ("simulated_kill", "SIM_KILL", " (simulated, ams_protected: spawn skipped)")
        );
    }

    /// Build the record exactly the way the dispatch loop does, from the same two inputs.
    fn record_from_pipeline(act_mode: bool, ams_protected: bool, dispatched: libc::c_int) -> KillRecord<'static> {
        const NO_RECORD: KillRecord<'static> = KillRecord {
            ts: 0,
            event: "kill",
            pkg: "",
            pids: &[],
            rss_freed_est_kb: 0,
            reason: "idle_expired",
            idle_sec: 0,
            lru_pos: 0,
            spawn_errno: None,
            oom_score_adj: 0,
            ams_protected: false,
            method: "ams",
        };
        let mut record = NO_RECORD;
        record.event = kill_telemetry_parts(act_mode, ams_protected).0;
        record.spawn_errno = if !act_mode {
            None
        } else if ams_protected {
            Some(spawn::NOT_DISPATCHED)
        } else {
            Some(dispatched)
        };
        record.ams_protected = ams_protected;
        record
    }

    #[test]
    fn kill_record_json_reports_which_of_its_four_states_spawn_errno_is() {
        // This is why `KillRecord` is a struct and not a 12-slot `format!` in the middle of the
        // dispatch loop: the load-bearing information is *which state* the field is in, and a
        // format string cannot be tested without reading it as prose.
        let dispatched = record_from_pipeline(true, false, 0);
        let skipped = record_from_pipeline(true, true, 0);
        let observe = record_from_pipeline(false, false, 0);
        let refused = record_from_pipeline(true, false, libc::EBADF);

        assert!(dispatched.to_json().contains(r#""spawn_errno":0"#));
        assert!(skipped
            .to_json()
            .contains(&format!(r#""spawn_errno":{}"#, spawn::NOT_DISPATCHED)));
        assert!(observe.to_json().contains(r#""spawn_errno":null"#));
        assert!(
            refused.to_json().contains(r#""spawn_errno":9"#),
            "EBADF must be passed through as the number a consumer greps for, got {}",
            refused.to_json()
        );

        // The two states that mean "nothing was launched", and the two that mean it was.
        assert!(observe.spawn_skipped(), "observe mode attempted no dispatch");
        assert!(skipped.spawn_skipped(), "the guard declined this one");
        assert!(!dispatched.spawn_skipped(), "launched");
        assert!(!refused.spawn_skipped(), "the spawner refused, but it did try");

        // `spawn_skipped` is derived, so it must agree with the pipeline that used to carry it as
        // a second copy of the same fact - including in observe mode, where the naive rule
        // `spawn_errno == Some(NOT_DISPATCHED)` reports `false`.
        for (act, ams) in [(true, false), (true, true), (false, false), (false, true)] {
            let record = record_from_pipeline(act, ams, 0);
            assert_eq!(record.spawn_skipped(), !act || ams,
                "spawn_skipped disagrees with the pipeline for act={act} ams={ams}");
        }

        // The record the v1.4.0 consumer reads: `spawn_skipped` is still there, and no `spawned`
        // boolean came back alongside it to disagree.
        let json = skipped.to_json();
        assert!(json.contains(r#""spawn_skipped":true"#), "{json}");
        assert!(!json.contains("spawned"), "the v1.4.0 boolean must not return: {json}");
        assert!(
            json.starts_with(r#"{"ts":0,"event":"kill_skipped","pkg":"","pids":[],"rss_freed_est_kb":0,"reason":"idle_expired","idle_sec":0,"lru_pos":0,"spawn_errno":-2,"oom_score_adj":0,"ams_protected":true,"spawn_skipped":true,"method":"ams"}"#),
            "field order is the log format: {json}"
        );
    }

    #[test]
    fn test_scan_candidates_absent_from_lru_is_not_protected_by_large_depth() {
        // F3 regression: with a finite lru_pos sentinel, lru_protect_depth above the
        // sentinel protected packages that are not in the LRU at all — the daemon
        // silently stopped reclaiming anything while looking healthy.
        let (mut d, _log) = crate::daemon::test_support::daemon_for_test("scanlru", 1_700_000_000_000);
        let pkg = "com.example.victim";
        let pid = std::process::id() as u32; // a pid that certainly exists: ours
        d.alive_apps.insert(pkg.into(), 1); // idle far beyond any threshold
        d.pkg_to_pids.entry(pkg.into()).or_default().insert(pid);
        d.pkg_to_uid.insert(pkg.into(), 10_001);
        d.config.lru_protect_depth = 100;

        let cands = d.scan_candidates(1_700_000_060_000, false, false);
        assert!(
            cands.iter().any(|c| c.pkg == pkg),
            "an absent-from-LRU package must stay harvestable at any configured depth"
        );
    }

    #[test]
    fn test_f15_screen_off_harvest_never_loosens_user_idle() {
        // F15 regression: screen-off harvest used to hard-set t_idle to 30/60, which for
        // a user configured below that (aggressive preset) made screen-off LESS
        // aggressive than screen-on. Harvest may only tighten, never loosen.
        let (mut d, _log) = crate::daemon::test_support::daemon_for_test("screentidle", 1_700_000_000_000);
        let pkg = "com.example.victim";
        let pid = std::process::id() as u32;
        d.alive_apps.insert(pkg.into(), 1); // idle 60s > any threshold
        d.pkg_to_pids.entry(pkg.into()).or_default().insert(pid);
        d.pkg_to_uid.insert(pkg.into(), 10_001);
        d.config.t_idle_sec = 10;
        d.config.screen_off_harvest = true;
        d.screen_on = false;
        d.screen_off_start = Some(1_700_000_000_000); // off for 10s: old code forced t_idle = 60

        let cands = d.scan_candidates(1_700_000_060_000, false, false);
        assert!(
            cands.iter().any(|c| c.pkg == pkg),
            "a package idle past the user's t_idle_sec must stay harvestable with the screen off"
        );
    }

    #[test]
    fn test_f19_low_mem_escalation_never_loosens_user_idle() {
        // F19 regression: the emergency branch hard-set t_idle to 10s, relaxing an
        // aggressive preset (5s) when pressure hits — the inverse of escalation.
        let (mut d, _log) = crate::daemon::test_support::daemon_for_test("lowmemidle", 1_700_000_000_000);
        let pkg = "com.example.victim";
        let pid = std::process::id() as u32;
        d.alive_apps.insert(pkg.into(), 1); // idle 7s: >= user t_idle 5, < emergency 10
        d.pkg_to_pids.entry(pkg.into()).or_default().insert(pid);
        d.pkg_to_uid.insert(pkg.into(), 10_001);
        d.config.t_idle_sec = 5;

        let cands = d.scan_candidates(1_700_000_007_000, false, true); // is_low_mem = true
        assert!(
            cands.iter().any(|c| c.pkg == pkg),
            "under memory pressure a preset faster than the 10s emergency window must stay in force"
        );
    }

    #[test]
    fn kill_method_is_sigkill_only_for_root_and_fully_cached() {
        // The whole root fast path decision in four rows: signal only what AMS would
        // consider safely killable anyway (cached, adj >= 900), delegate the rest.
        assert_eq!(kill_method(true, false, 950), "sigkill");
        assert_eq!(kill_method(true, false, 900), "sigkill");
        assert_eq!(kill_method(true, false, 899), "ams");
        assert_eq!(kill_method(false, false, 950), "ams");
        assert_eq!(kill_method(true, true, 950), "ams");
    }
}
