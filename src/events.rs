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

//! Logcat event handlers: the bookkeeping state machine that turns lifecycle
//! events into foreground tracking, LRU positions, game sessions and summaries.

use crate::daemon::DaemonState;
use crate::parser::{ProcDiedEvent, ProcStartEvent, ResumeActivityEvent};
use crate::telemetry::{escape_json, tabular_row, SessionStats};
use crate::procfs::read_statm_rss_kb;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lower bound (2020-09-13) separating an unsynchronized boot RTC from NTP-synced wall clock.
pub(crate) const RTC_SYNC_FLOOR_MS: u64 = 1_600_000_000_000;

impl DaemonState {
    pub(crate) fn emit_bg_summary(&mut self, now_epoch: u64, interval_sec: u64) {
        if self.session_stats.bg_spawns == 0 && self.session_stats.bg_deaths == 0 {
            return;
        }
        self.telemetry.emit_with(
            || {
                // F20: this is the spawn-side sum only — deaths are not subtracted (a
                // death-time statm read races PID reuse), so the label must not claim a delta.
                let rss_mb = (self.session_stats.spawn_rss_kb.unsigned_abs() + 512) / 1024;
                let detail = format!(
                    "interval={}s  spawns={}  deaths={}  spawn_rss={}MB",
                    interval_sec, self.session_stats.bg_spawns, self.session_stats.bg_deaths, rss_mb
                );
                tabular_row(now_epoch, "BG_SUMMARY", "--", &detail)
            },
            || format!(
                r#"{{"ts":{},"event":"bg_summary","interval_sec":{},"spawns":{},"deaths":{},"spawn_rss_kb":{}}}"#,
                now_epoch, interval_sec, self.session_stats.bg_spawns, self.session_stats.bg_deaths, self.session_stats.spawn_rss_kb
            ),
        );
        self.session_stats = SessionStats::default();
        self.session_start = now_epoch;
    }

    pub(crate) fn on_resume_activity(&mut self, ev: &ResumeActivityEvent, now_epoch: u64) {
        let pkg = ev.pkg;
        let component = ev.component;

        let mut prev_dur_ms = 0u64;

        let prev_pkg_opt = self.current_fg.take();
        if let Some(ref prev_pkg) = prev_pkg_opt {
            if prev_pkg != pkg {
                // Guard: Only user/app packages with verified UID >= 10000 may enter alive_apps.
                // Fail-closed: If UID cannot be resolved, assume protected/system and DO NOT insert.
                if self.pkg_to_uid.get(prev_pkg).is_some_and(|&uid| uid >= 10000) {
                    if let Some(anchor) = self.alive_apps.get_mut(prev_pkg) {
                        prev_dur_ms = now_epoch.saturating_sub(*anchor);
                        *anchor = now_epoch;
                    } else {
                        self.alive_apps.insert(prev_pkg.clone(), now_epoch);
                    }
                }
            }
        }

        let interval_sec = now_epoch.saturating_sub(self.session_start) / 1000;
        self.emit_bg_summary(now_epoch, interval_sec);

        // If the departed package was only in foreground for < 500ms (e.g. trampoline, chooser, auth pulse),
        // evict it from fg_lru so it does not displace real user applications in the LRU protection window.
        if prev_dur_ms > 0 && prev_dur_ms < 500 {
            if let Some(pos) = self.fg_lru.iter().position(|x| x == prev_pkg_opt.as_deref().unwrap()) {
                self.fg_lru.remove(pos);
            }
        }

        if let Some(pos) = self.fg_lru.iter().position(|x| x == pkg) {
            if pos > 0 {
                let existing = self.fg_lru.remove(pos).unwrap();
                self.fg_lru.push_front(existing);
            }
        } else {
            self.fg_lru.push_front(pkg.to_string());
            if self.fg_lru.len() > self.config.fg_lru_max_depth {
                self.fg_lru.pop_back();
            }
        }

        let was_gaming = self.is_gaming;
        let is_game = self.games.contains(pkg);
        self.is_gaming = is_game;

        if is_game && !was_gaming {
            self.game_session_start = Some(now_epoch);
            self.game_intrusion_count = 0;
            self.telemetry.emit_with(
                || tabular_row(now_epoch, "GAME_START", pkg, "game_mode=active"),
                || format!(r#"{{"ts":{},"event":"game_session_start","pkg":"{}"}}"#, now_epoch, escape_json(pkg)),
            );
        } else if !is_game && was_gaming {
            let duration_sec = self.game_session_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            self.telemetry.emit_with(
                || {
                    let detail = format!("duration={}s  intrusions={}", duration_sec, self.game_intrusion_count);
                    tabular_row(now_epoch, "GAME_END", "--", &detail)
                },
                || format!(
                    r#"{{"ts":{},"event":"game_session_end","duration_sec":{},"intrusions":{}}}"#,
                    now_epoch, duration_sec, self.game_intrusion_count
                ),
            );
            self.game_session_start = None;
        }

        if prev_pkg_opt.as_deref() == Some(pkg) {
            self.current_fg = prev_pkg_opt.clone();
        } else {
            self.current_fg = Some(pkg.to_string());
        }

        self.telemetry.emit_with(
            || {
                let prev_detail = if let Some(ref prev) = prev_pkg_opt {
                    let dur_s = (prev_dur_ms as f64) / 1000.0;
                    format!("prev={} ({:.1}s){}", prev, dur_s, if is_game { " [GAME]" } else { "" })
                } else {
                    format!("cold_boot{}", if is_game { " [GAME]" } else { "" })
                };
                tabular_row(now_epoch, "FG_SWITCH", pkg, &prev_detail)
            },
            || format!(
                r#"{{"ts":{},"event":"fg_switch","pkg":"{}","component":"{}","prev_dur_ms":{},"is_game":{}}}"#,
                now_epoch, escape_json(pkg), escape_json(component), prev_dur_ms, is_game
            ),
        );

        if prev_pkg_opt.as_deref() != Some(pkg) {
            self.evaluate_reaping_pipeline(now_epoch);
        }
    }

    pub(crate) fn on_proc_start(&mut self, ev: &ProcStartEvent, now_epoch: u64) {
        let pid = ev.pid;
        let uid = ev.uid;
        let raw_proc_name = ev.proc_name;
        let spawn_type = ev.spawn_type;
        let pkg = ev.pkg;

        let initial_rss_kb = read_statm_rss_kb(pid, self.page_size_kb);

        let is_fg = self.current_fg.as_deref() == Some(pkg);
        if !is_fg {
            self.session_stats.bg_spawns += 1;
            self.session_stats.spawn_rss_kb += initial_rss_kb as i64;
        }

        if self.is_gaming && spawn_type != "top-activity" && spawn_type != "next-top-activity" {
            self.game_intrusion_count += 1;
            let excluded = self.is_excluded(pkg);
            self.telemetry.emit_with(
                || {
                    let rss_mb = (initial_rss_kb + 512) / 1024;
                    let detail = format!("type={}  rss={}MB  excluded={}", spawn_type, rss_mb, excluded);
                    tabular_row(now_epoch, "GAME_INTRUDE", pkg, &detail)
                },
                || format!(
                    r#"{{"ts":{},"event":"game_intrusion","pid":{},"uid":{},"pkg":"{}","proc":"{}","type":"{}","rss_kb":{},"excluded":{}}}"#,
                    now_epoch, pid, uid, escape_json(pkg), escape_json(raw_proc_name), escape_json(spawn_type), initial_rss_kb, excluded
                ),
            );
        }

        let pkg_owned = pkg.to_string();
        self.pid_to_pkg.insert(pid, pkg_owned.clone());
        self.pkg_to_pids.entry(pkg_owned.clone()).or_default().insert(pid);
        if !self.pkg_to_uid.contains_key(pkg) {
            self.pkg_to_uid.insert(pkg_owned.clone(), uid);
        }

        // Respawn tracking (proc_died -> proc_start within 120s)
        if let Some(death_time) = self.recent_deaths.remove(pkg) {
            let gap_ms = now_epoch.saturating_sub(death_time);
            if gap_ms <= 120_000 {
                self.telemetry.emit_with(
                    || {
                        let detail = format!("gap={}ms  pid={}  type={}", gap_ms, pid, spawn_type);
                        tabular_row(now_epoch, "RESPAWN", pkg, &detail)
                    },
                    || format!(
                        r#"{{"ts":{},"event":"respawn","pkg":"{}","gap_ms":{},"pid":{},"uid":{},"type":"{}"}}"#,
                        now_epoch, escape_json(pkg), gap_ms, pid, uid, escape_json(spawn_type)
                    ),
                );
            }
        }

        if uid >= 10000 {
            self.alive_apps.entry(pkg_owned).or_insert(now_epoch);

            let is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity";
            if !is_fg_launch {
                self.evaluate_reaping_pipeline(now_epoch);
            }
        }
    }

    pub(crate) fn on_proc_died(&mut self, ev: &ProcDiedEvent, now_epoch: u64) {
        let pid = ev.pid;

        if let Some(pkg) = self.pid_to_pkg.remove(&pid) {
            if let Some(pids) = self.pkg_to_pids.get_mut(&pkg) {
                pids.remove(&pid);
                if pids.is_empty() {
                    self.alive_apps.remove(&pkg);
                }
            }
            let is_fg = self.current_fg.as_deref() == Some(&pkg);
            if !is_fg {
                self.session_stats.bg_deaths += 1;
            }

            self.recent_deaths.insert(pkg, now_epoch);
            if self.recent_deaths.len() > 64 {
                self.recent_deaths.retain(|_, death_time| now_epoch.saturating_sub(*death_time) <= 120_000);
            }
        }
    }

    pub(crate) fn on_screen_toggled(&mut self, state: bool, now_epoch: u64) {
        if self.screen_on == state {
            return;
        }

        if !state {
            let active_sec = self.screen_on_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            self.telemetry.emit_with(
                || {
                    let detail = format!("active_session={:.1}s", active_sec as f64);
                    tabular_row(now_epoch, "SCREEN_OFF", "--", &detail)
                },
                || format!(
                    r#"{{"ts":{},"event":"screen_state","state":"OFF","active_duration_sec":{}}}"#,
                    now_epoch, active_sec
                ),
            );
            self.emit_bg_summary(now_epoch, active_sec);

            self.screen_on = false;
            self.screen_on_start = None;
            self.screen_off_start = Some(now_epoch);
        } else {
            let duration_sec = self.screen_off_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            self.telemetry.emit_with(
                || {
                    let detail = format!("sleep={}s", duration_sec);
                    tabular_row(now_epoch, "SCREEN_ON", "--", &detail)
                },
                || format!(
                    r#"{{"ts":{},"event":"screen_state","state":"ON","off_duration_sec":{}}}"#,
                    now_epoch, duration_sec
                ),
            );
            self.emit_bg_summary(now_epoch, duration_sec);

            self.screen_on = true;
            self.screen_on_start = Some(now_epoch);
            self.screen_off_start = None;
        }
    }

    /// Signed wall-clock step between two consecutive samples, or 0 for a continuous stream.
    ///
    /// Only genuine `settimeofday()`/NTP steps qualify: a backward step, or the one-time
    /// forward jump out of the pre-sync boot RTC band. Ordinary quiet gaps between events
    /// (reading, screen off, Doze) are real elapsed time and must always yield 0. A `0`
    /// `prev_epoch` means the clock was unavailable at bootstrap and yields 0.
    ///
    /// Known limitation: a *forward* step whose starting value is already at or above
    /// `RTC_SYNC_FLOOR_MS` is indistinguishable from a quiet gap and returns 0. Only the
    /// magnitude is observable, and a legitimate Doze gap can be arbitrarily large, so any
    /// threshold that caught such a step would also freeze real idle age. Anchors are
    /// therefore assumed to be stamped from a clock that is either correct or pre-2020.
    #[inline(always)]
    pub(crate) fn clock_jump_ms(prev_epoch: u64, now_epoch: u64) -> i64 {
        if prev_epoch == 0 {
            0
        } else if now_epoch < prev_epoch {
            -((prev_epoch - now_epoch) as i64)
        } else if prev_epoch < RTC_SYNC_FLOOR_MS && now_epoch >= RTC_SYNC_FLOOR_MS {
            (now_epoch - prev_epoch) as i64
        } else {
            0
        }
    }

    /// Shifts every stored timestamp anchor by a detected clock step, so elapsed ages
    /// (idle, respawn TTL, screen-state, game session, summary interval) survive the
    /// correction unchanged. Records are never re-derived from the wall clock elsewhere.
    pub(crate) fn apply_clock_jump(&mut self, jump_ms: i64) {
        if jump_ms == 0 {
            return;
        }
        for anchor in self.alive_apps.values_mut() {
            *anchor = anchor.saturating_add_signed(jump_ms);
        }
        for death in self.recent_deaths.values_mut() {
            *death = death.saturating_add_signed(jump_ms);
        }
        for anchor in [
            &mut self.screen_on_start,
            &mut self.screen_off_start,
            &mut self.game_session_start,
        ] {
            *anchor = anchor.map(|t| t.saturating_add_signed(jump_ms));
        }
        self.session_start = self.session_start.saturating_add_signed(jump_ms);
    }

    #[inline(always)]
    pub(crate) fn get_epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_jump_ms() {
        // First event after bootstrap: no previous sample, nothing to compensate.
        assert_eq!(DaemonState::clock_jump_ms(0, 1_700_000_000_000), 0);

        // Quiet periods are real elapsed time, never a clock step (regression guard).
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_700_000_060_000), 0);
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_800_000_000_000), 0);
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_700_000_000_000), 0);

        // Backward settimeofday() correction of 10s.
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_010_000, 1_700_000_000_000), -10_000);

        // Unsynchronized boot RTC jumping forward to NTP-synced time.
        assert_eq!(DaemonState::clock_jump_ms(15_000, 1_700_000_000_000), 1_699_999_985_000);
    }

    #[test]
    fn test_bootstrap_clock_step_rebases_bootstrap_anchors() {
        // A dead coin cell leaves the RTC in the pre-2020 band; the first event of the
        // stream arrives after the clock has been corrected. Anchors stamped from the boot
        // clock have to be rebased, or every interval derived from them spans decades.
        let boot_ms = 1_234_567_890_000u64; // 2009, below RTC_SYNC_FLOOR_MS
        let first_event_ms = 1_777_998_045_123u64; // post-sync
        let idle = "com.example.idle";
        let (mut d, _) = crate::daemon::test_support::daemon_for_test("bootstep", boot_ms);
        d.alive_apps.insert(idle.into(), boot_ms);
        d.recent_deaths.insert("com.example.dead".into(), boot_ms);

        let jump = DaemonState::clock_jump_ms(d.last_event_epoch, first_event_ms);
        assert!(jump > 0, "pre-sync boot stamp is not a quiet gap");
        d.apply_clock_jump(jump);

        // Bootstrap-stamped ages collapse to ~0 instead of ~56 years.
        assert_eq!(d.session_start, first_event_ms);
        assert_eq!(d.screen_on_start, Some(first_event_ms));
        assert_eq!(d.alive_apps[idle], first_event_ms);
        assert_eq!(d.recent_deaths["com.example.dead"], first_event_ms);

        // A correct boot clock followed by an ordinary quiet gap stays untouched.
        let (mut d2, _) = crate::daemon::test_support::daemon_for_test("bootsync", first_event_ms);
        d2.alive_apps.insert(idle.into(), first_event_ms);
        let later = first_event_ms + 3_600_000;
        let jump2 = DaemonState::clock_jump_ms(d2.last_event_epoch, later);
        assert_eq!(jump2, 0, "silence is elapsed time, not a step");
        d2.apply_clock_jump(jump2);
        assert_eq!(d2.session_start, first_event_ms);
        assert_eq!(d2.screen_on_start, Some(first_event_ms));
        assert_eq!(later - d2.alive_apps[idle], 3_600_000);
    }
}
