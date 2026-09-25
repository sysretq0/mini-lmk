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

//! Zero-allocation, resilient event log parser supporting Android 7.0+ (API 24+).
//!
//! Slices directly from borrowed string buffers with zero heap allocation.

#[derive(Debug, PartialEq, Eq)]
pub struct ResumeActivityEvent<'a> {
    pub pkg: &'a str,
    pub component: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProcStartEvent<'a> {
    pub proc_name: &'a str,
    pub pkg: &'a str,
    pub spawn_type: &'a str,
    pub pid: u32,
    pub uid: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct ProcDiedEvent {
    pub pid: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LogcatEvent<'a> {
    ResumeActivity(ResumeActivityEvent<'a>),
    ProcStart(ProcStartEvent<'a>),
    ProcDied(ProcDiedEvent),
    ScreenToggled(bool),
    DeviceIdleLightStep,
}

/// Tokenize comma-separated payload into a fixed-size stack array without heap allocations.
/// Respects nested `{...}` braces so internal commas (e.g. in component intent extras)
/// do not fragment tokens.
/// Returns the number of tokens collected.
#[inline(always)]
fn tokenize<'a, const N: usize>(inner: &'a str, out: &mut [&'a str; N]) -> usize {
    let mut count = 0;
    let bytes = inner.as_bytes();
    let mut start = 0;
    let mut brace_depth = 0usize;

    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b',' if brace_depth == 0 => {
                if count < N {
                    out[count] = inner[start..i].trim();
                    count += 1;
                }
                start = i + 1;
                if count >= N {
                    break;
                }
            }
            _ => {}
        }
    }

    if count < N && start <= inner.len() {
        out[count] = inner[start..].trim();
        count += 1;
    }

    count
}

/// Parse `wm_resume_activity` and `am_resume_activity` payloads.
///
/// Standard format (API 29+):
/// `[<user_id>, <token>, <task_id>, <component_name>]`
/// e.g. `[0,1234567,12,com.google.android.calculator/com.android.calculator2.Calculator]`
///
/// Resilient extraction:
/// In Android, component names are strictly `<package>/<activity>`.
/// We scan tokens for the component containing `/`. If no token has `/`,
/// we fall back to standard positional index (index 3).
///
/// Resilient extraction:
/// - Strips enclosing curly braces `{...}` and whitespace.
/// - Handles Intent-wrapped components like `{act=... cmp=pkg/act flg=...}`.
/// - Validates that the package name is non-empty, contains letters, and does not start with hyphens or braces.
pub fn parse_resume_activity(payload: &str) -> Option<ResumeActivityEvent<'_>> {
    let inner = payload.trim().trim_matches(['[', ']']).trim();
    let mut tokens = [""; 8];
    let count = tokenize(inner, &mut tokens);
    if count == 0 {
        return None;
    }

    let mut raw_comp = "";
    for &tok in &tokens[..count] {
        if tok.contains('/') {
            raw_comp = tok;
            break;
        }
    }

    if raw_comp.is_empty() && count >= 4 {
        raw_comp = tokens[3];
    }

    if raw_comp.is_empty() {
        return None;
    }

    // Strip enclosing braces and whitespace
    let mut clean_comp = raw_comp.trim().trim_matches(['{', '}']).trim();

    // If wrapped in Intent syntax with "cmp=", extract the component part
    if let Some(idx) = clean_comp.find("cmp=") {
        clean_comp = clean_comp[idx + 4..].trim();
        // cmp is terminated by whitespace, bracket, brace, or end of string
        if let Some(end_idx) = clean_comp.find(|c: char| c.is_whitespace() || c == '}' || c == ']') {
            clean_comp = &clean_comp[..end_idx];
        }
    } else if clean_comp.contains(' ') {
        if let Some(word) = clean_comp.split_whitespace().find(|w| w.contains('/')) {
            clean_comp = word.trim_matches(['{', '}', '"', '\'']);
        }
    }

    clean_comp = clean_comp.trim_matches(['{', '}', ' ', '"', '\'']);
    if clean_comp.is_empty() {
        return None;
    }

    let pkg = clean_comp.split('/').next().unwrap_or(clean_comp).trim();
    if pkg.is_empty() || pkg.starts_with('-') || pkg.starts_with('{') || !pkg.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }

    Some(ResumeActivityEvent {
        pkg,
        component: clean_comp,
    })
}

/// Parse `am_proc_start` payloads.
///
/// Standard AOSP format (API 29+):
/// `[<user_id>, <pid>, <uid>, <process_name>, <type>, <component>]`
/// e.g. `[0,12763,10130,com.google.android.calculator,next-top-activity,{...}]`
///
/// Resilient extraction:
/// - Fast path: standard indices (tokens[1] = PID, tokens[2] = UID, tokens[3] = proc_name, tokens[4] = type).
/// - Fallback: dynamically identify PID, UID, and process name by inspecting token formats.
pub fn parse_proc_start(payload: &str) -> Option<ProcStartEvent<'_>> {
    let inner = payload.trim().trim_matches(['[', ']']).trim();
    let mut tokens = [""; 8];
    let count = tokenize(inner, &mut tokens);
    if count < 4 {
        return None;
    }

    // Fast-path: Standard AOSP indices
    if let (Some(pid), Some(uid)) = (tokens[1].parse::<u32>().ok(), tokens[2].parse::<u32>().ok()) {
        let proc_name = tokens[3];
        if proc_name.starts_with('-') {
            return None;
        }
        if !proc_name.is_empty() && !proc_name.starts_with('{') {
            let spawn_type = if count > 4 { tokens[4] } else { "" };
            let pkg = proc_name.split(':').next().unwrap_or(proc_name).trim();
            if !pkg.is_empty() && !pkg.starts_with('-') {
                return Some(ProcStartEvent {
                    proc_name,
                    pkg,
                    spawn_type,
                    pid,
                    uid,
                });
            } else {
                return None;
            }
        }
    }

    // Resilient fallback: dynamic detection across non-standard or shifted vendor schemas
    let mut pid = 0u32;
    let mut uid = 0u32;
    let mut proc_idx = None;

    for (i, &tok) in tokens[..count].iter().enumerate() {
        if let Ok(val) = tok.parse::<u32>() {
            if pid == 0 && i > 0 {
                pid = val;
            } else if pid > 0 && uid == 0 {
                uid = val;
            }
        } else if !tok.is_empty() && !tok.starts_with('{') && !tok.starts_with('-') && proc_idx.is_none() {
            // First non-numeric, non-component token is candidate for process name
            if tok.contains('.') || tok.contains(':') || tok.chars().any(|c| c.is_alphabetic()) {
                proc_idx = Some(i);
            }
        }
    }

    let p_idx = proc_idx?;
    let proc_name = tokens[p_idx];
    if proc_name.starts_with('-') {
        return None;
    }
    let spawn_type = if p_idx + 1 < count && !tokens[p_idx + 1].starts_with('{') {
        tokens[p_idx + 1]
    } else {
        ""
    };

    if pid == 0 {
        return None;
    }

    let pkg = proc_name.split(':').next().unwrap_or(proc_name).trim();
    if pkg.is_empty() || pkg.starts_with('-') {
        return None;
    }
    Some(ProcStartEvent {
        proc_name,
        pkg,
        spawn_type,
        pid,
        uid,
    })
}

/// Validate whether a token looks like a valid Linux/Android process identifier
/// (e.g. "com.example.app", "system_server", "zygote64", "com.android.chrome:sandboxed_process0").
/// Rejects empty strings, strings with spaces (e.g. reasons like "kill background"),
/// strings starting with hyphens or braces, and purely numeric strings.
#[inline(always)]
fn is_proc_name(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut has_alpha = false;
    for &b in s.as_bytes() {
        if b.is_ascii_alphabetic() {
            has_alpha = true;
        } else if !b.is_ascii_digit() && b != b'.' && b != b'_' && b != b':' {
            return false;
        }
    }
    has_alpha
}

/// Parse `am_proc_died` payloads.
///
/// Standard AOSP format (API 29+):
/// `[<user_id>, <pid>, <process_name>, <oom_adj>, <reason>]`
/// e.g. `[0,12763,com.google.android.calculator,900,kill background]`
///
/// Legacy/OEM format:
/// `[<pid>, <process_name>]`
/// e.g. `[12763,com.example.app]`
///
/// Resilient extraction:
/// - Fast path: standard AOSP index 1 (PID strictly adjacent to validated process name at index 2).
/// - Legacy/OEM path: index 0 (PID strictly adjacent to validated process name at index 1).
/// - Resilient scan: matches a positive numeric token strictly preceding a verified process identifier.
///   Eliminates naive fallbacks where `oom_adj` or `proc_state` values could be misparsed as PIDs.
pub fn parse_proc_died(payload: &str) -> Option<ProcDiedEvent> {
    let inner = payload.trim().trim_matches(['[', ']']).trim();
    let mut tokens = [""; 8];
    let count = tokenize(inner, &mut tokens);
    if count < 2 {
        return None;
    }

    // Scan for candidate PID strictly followed by verified process identifier
    for i in 0..count.saturating_sub(1) {
        if let Ok(pid) = tokens[i].parse::<u32>() {
            if pid > 0 && is_proc_name(tokens[i + 1]) {
                return Some(ProcDiedEvent { pid });
            }
        }
    }

    None
}

/// Parse `screen_toggled` payloads.
///
/// Standard format: `0` (OFF) or `1` (ON).
/// Tolerates brackets `[0]`, `[1]` and leading/trailing whitespace.
#[inline(always)]
pub fn parse_screen_toggled(payload: &str) -> Option<bool> {
    let s = payload.trim().trim_matches(['[', ']']).trim();
    match s {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Parse epoch timestamp from the leading field of a `-v epoch` logcat line.
/// Input: "1695411709.231" → Output: 1695411709231 (milliseconds since Unix epoch).
/// Returns `None` for a non-numeric field or a seconds value too large to scale to ms,
/// so a garbage timestamp can never reach the clock-step classifier or the idle math.
#[inline(always)]
fn parse_epoch_ms(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: u64 = secs.parse().ok()?;
    let mut ms = 0u64;
    let mut mult = 100;
    for &b in frac.as_bytes().iter().take(3) {
        if !b.is_ascii_digit() { return None; }
        ms += (b - b'0') as u64 * mult;
        mult /= 10;
    }
    secs.checked_mul(1000)?.checked_add(ms)
}

/// Zero-copy dispatcher for raw logcat lines from `logcat -v epoch`.
/// Extracts the event's own epoch timestamp and dispatches to the appropriate parser.
///
/// Format: `<epoch_sec.ms>  <pid>  <tid> <level> <tag>: <payload>`
/// Example: `1695411709.231  2415  2415 I am_proc_start: [0,12345,10130,...]`
pub fn parse_logcat_line(line: &str) -> Option<(u64, LogcatEvent<'_>)> {
    let s = line.trim();
    if s.is_empty() || s.starts_with("---") {
        return None;
    }

    let (header, payload) = s.split_once(':')?;
    let mut tokens = header.split_whitespace();
    let epoch_ms = parse_epoch_ms(tokens.next()?)?;
    let tag = tokens.next_back()?;

    // am_proc_start and am_proc_died have the highest arrival frequency
    let event = match tag {
        "am_proc_start" => parse_proc_start(payload).map(LogcatEvent::ProcStart),
        "am_proc_died" => parse_proc_died(payload).map(LogcatEvent::ProcDied),
        "wm_resume_activity" | "am_resume_activity" => {
            parse_resume_activity(payload).map(LogcatEvent::ResumeActivity)
        }
        "screen_toggled" => parse_screen_toggled(payload).map(LogcatEvent::ScreenToggled),
        "device_idle_light_step" => {
            let p = payload.trim().trim_matches(['[', ']']).trim();
            if p.is_empty() {
                Some(LogcatEvent::DeviceIdleLightStep)
            } else {
                None
            }
        }
        _ => None,
    }?;

    Some((epoch_ms, event))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "1695411709.231";
    const TS_MS: u64 = 1695411709231;

    fn epoch_line(tag: &str, payload: &str) -> String {
        format!("{TS}  2415  2415 I {tag}: {payload}")
    }

    #[test]
    fn test_parse_epoch_ms() {
        assert_eq!(parse_epoch_ms("1695411709.231"), Some(1695411709231));
        assert_eq!(parse_epoch_ms("1695411709.23"), Some(1695411709230));
        assert_eq!(parse_epoch_ms("1695411709.2"), Some(1695411709200));
        assert_eq!(parse_epoch_ms("1695411709"), Some(1695411709000));
        assert_eq!(parse_epoch_ms("1695411709.1234"), Some(1695411709123));
        assert_eq!(parse_epoch_ms("abc"), None);
        // Out-of-range seconds must be rejected, not wrapped (1e19 fits u64 but not * 1000).
        assert_eq!(parse_epoch_ms("10000000000000000000.5"), None);
        assert_eq!(parse_epoch_ms("18446744073709551615"), None);
        // Largest representable epoch-ms value still parses.
        assert_eq!(parse_epoch_ms("18446744073709551"), Some(18446744073709551000));
    }

    #[test]
    fn test_parse_device_idle_light_step() {
        let l1 = epoch_line("device_idle_light_step", "\n");
        assert!(matches!(parse_logcat_line(&l1), Some((_, LogcatEvent::DeviceIdleLightStep))));
        let l2 = epoch_line("device_idle_light_step", " ");
        assert!(matches!(parse_logcat_line(&l2), Some((_, LogcatEvent::DeviceIdleLightStep))));
        let l3 = epoch_line("device_idle_light_step", "[]");
        assert!(matches!(parse_logcat_line(&l3), Some((_, LogcatEvent::DeviceIdleLightStep))));
        let l4 = epoch_line("device_idle_light_step", "[ ]");
        assert!(matches!(parse_logcat_line(&l4), Some((_, LogcatEvent::DeviceIdleLightStep))));
        assert_eq!(parse_logcat_line(&epoch_line("device_idle_light_step", "[4,s:alarm]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("device_idle_light_step", "fake")), None);
        assert_eq!(parse_logcat_line(&epoch_line("device_idle_light_step", "1")), None);
    }

    #[test]
    fn test_reject_leading_hyphen_injection() {
        assert_eq!(parse_logcat_line(&epoch_line("wm_resume_activity", "[0,123,1,-bad.pkg/-bad.pkg.Act]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("wm_resume_activity", "[0,123,1,--user/-bad.pkg.Act]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("am_proc_start", "[0,1234,10001,--user,next-top-activity,{}]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("am_proc_start", "[0,1234,10001,-malicious.app,service,{}]")), None);
    }

    #[test]
    fn test_parse_wm_resume_activity() {
        let line = epoch_line("wm_resume_activity", "[0,1234567,12,com.google.android.calculator/com.android.calculator2.Calculator]");
        match parse_logcat_line(&line) {
            Some((ts, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ts, TS_MS);
                assert_eq!(ev.pkg, "com.google.android.calculator");
                assert_eq!(ev.component, "com.google.android.calculator/com.android.calculator2.Calculator");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_am_resume_activity_api29() {
        let line = epoch_line("am_resume_activity", "[0, 987654, 42, com.android.settings/.Settings$StorageUseActivity]");
        match parse_logcat_line(&line) {
            Some((_, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ev.pkg, "com.android.settings");
                assert_eq!(ev.component, "com.android.settings/.Settings$StorageUseActivity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_am_resume_activity_api24() {
        let l4 = epoch_line("am_resume_activity", "[0, 1234567, 12, com.android.dialer/.DialtactsActivity]");
        match parse_logcat_line(&l4) {
            Some((_, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ev.pkg, "com.android.dialer");
                assert_eq!(ev.component, "com.android.dialer/.DialtactsActivity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
        let l3 = epoch_line("am_resume_activity", "[0, 12, com.android.mms/.ui.ConversationList]");
        match parse_logcat_line(&l3) {
            Some((_, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ev.pkg, "com.android.mms");
                assert_eq!(ev.component, "com.android.mms/.ui.ConversationList");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_proc_start() {
        let line = epoch_line("am_proc_start", "[0,12763,10130,com.google.android.calculator,next-top-activity,{com.google.android.calculator/com.android.calculator2.Calculator}]");
        match parse_logcat_line(&line) {
            Some((ts, LogcatEvent::ProcStart(ev))) => {
                assert_eq!(ts, TS_MS);
                assert_eq!(ev.pid, 12763);
                assert_eq!(ev.uid, 10130);
                assert_eq!(ev.pkg, "com.google.android.calculator");
                assert_eq!(ev.proc_name, "com.google.android.calculator");
                assert_eq!(ev.spawn_type, "next-top-activity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_proc_start_subproc() {
        let line = epoch_line("am_proc_start", "[0, 15400, 10150, com.android.chrome:sandboxed_process0, service, {}]");
        match parse_logcat_line(&line) {
            Some((_, LogcatEvent::ProcStart(ev))) => {
                assert_eq!(ev.pid, 15400);
                assert_eq!(ev.uid, 10150);
                assert_eq!(ev.pkg, "com.android.chrome");
                assert_eq!(ev.proc_name, "com.android.chrome:sandboxed_process0");
                assert_eq!(ev.spawn_type, "service");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_proc_died() {
        let line = epoch_line("am_proc_died", "[0,12763,com.google.android.calculator,900,kill background]");
        match parse_logcat_line(&line) {
            Some((ts, LogcatEvent::ProcDied(ev))) => {
                assert_eq!(ts, TS_MS);
                assert_eq!(ev.pid, 12763);
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_proc_died_comprehensive() {
        assert_eq!(parse_proc_died("[0, 12763, com.google.android.calculator, 900, 16]"), Some(ProcDiedEvent { pid: 12763 }));
        assert_eq!(parse_proc_died("[0, 12763, com.android.settings, 100, 2]"), Some(ProcDiedEvent { pid: 12763 }));
        assert_eq!(parse_proc_died("[0, 1500, system_server, -1000, 0]"), Some(ProcDiedEvent { pid: 1500 }));
        assert_eq!(parse_proc_died("[12763, com.example.app]"), Some(ProcDiedEvent { pid: 12763 }));
        assert_eq!(parse_proc_died("[0, not_a_pid, com.example.proc, 900, 16]"), None);
        assert_eq!(parse_proc_died("[0, not_a_pid, com.example.proc, 900, kill background]"), None);
        assert_eq!(parse_proc_died("[0, 900, kill background]"), None);
        assert_eq!(parse_proc_died("[0, 100, low memory]"), None);
    }

    #[test]
    fn test_parse_resume_activity_comprehensive() {
        let r1 = parse_resume_activity("[0, 1234567, 12, com.android.settings/.Settings]").expect("std");
        assert_eq!(r1.pkg, "com.android.settings");
        assert_eq!(r1.component, "com.android.settings/.Settings");
        let r2 = parse_resume_activity("[0, 1234567, 12, {com.google.android.calculator/com.android.calculator2.Calculator}]").expect("wrapped");
        assert_eq!(r2.pkg, "com.google.android.calculator");
        assert_eq!(r2.component, "com.google.android.calculator/com.android.calculator2.Calculator");
        let r3 = parse_resume_activity("[0, 1234567, 12, {act=android.intent.action.MAIN cmp=com.example.app/.MainActivity flg=0x10000000}]").expect("complex");
        assert_eq!(r3.pkg, "com.example.app");
        assert_eq!(r3.component, "com.example.app/.MainActivity");
        let lc = epoch_line("wm_resume_activity", "[0, 1234567, 12, {act=android.intent.action.MAIN cmp=com.example.app/.MainActivity flg=0x10000000}]");
        match parse_logcat_line(&lc) {
            Some((_, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ev.pkg, "com.example.app");
                assert_eq!(ev.component, "com.example.app/.MainActivity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_screen_toggled() {
        assert!(matches!(parse_logcat_line(&epoch_line("screen_toggled", "0")), Some((_, LogcatEvent::ScreenToggled(false)))));
        assert!(matches!(parse_logcat_line(&epoch_line("screen_toggled", "1")), Some((_, LogcatEvent::ScreenToggled(true)))));
        assert!(matches!(parse_logcat_line(&epoch_line("screen_toggled", "[1]")), Some((_, LogcatEvent::ScreenToggled(true)))));
        assert!(matches!(parse_logcat_line(&epoch_line("screen_toggled", "[0]")), Some((_, LogcatEvent::ScreenToggled(false)))));
    }

    #[test]
    fn test_parse_extra_fields_resilience() {
        let l1 = epoch_line("wm_resume_activity", "[0, 1234567, 12, com.example.app/com.example.app.MainActivity, 0, 1]");
        match parse_logcat_line(&l1) {
            Some((_, LogcatEvent::ResumeActivity(ev))) => {
                assert_eq!(ev.pkg, "com.example.app");
                assert_eq!(ev.component, "com.example.app/com.example.app.MainActivity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
        let l2 = epoch_line("am_proc_died", "[0, 9999, com.example.proc, -1000, died]");
        match parse_logcat_line(&l2) {
            Some((_, LogcatEvent::ProcDied(ev))) => assert_eq!(ev.pid, 9999),
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_proc_start_with_internal_commas_in_component() {
        let line = epoch_line("am_proc_start", "[0,12763,10130,com.google.android.calculator,next-top-activity,{act=android.intent.action.MAIN,cmp=com.google.android.calculator/com.android.calculator2.Calculator,flg=0x10000000}]");
        match parse_logcat_line(&line) {
            Some((_, LogcatEvent::ProcStart(ev))) => {
                assert_eq!(ev.pid, 12763);
                assert_eq!(ev.uid, 10130);
                assert_eq!(ev.pkg, "com.google.android.calculator");
                assert_eq!(ev.proc_name, "com.google.android.calculator");
                assert_eq!(ev.spawn_type, "next-top-activity");
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_and_malformed() {
        assert_eq!(parse_logcat_line(&epoch_line("am_anr", "[0,1234,foo]")), None);
        assert_eq!(parse_logcat_line("--- start of log ---"), None);
        assert_eq!(parse_logcat_line(""), None);
        assert_eq!(parse_logcat_line(&epoch_line("wm_resume_activity", "[]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("am_proc_start", "[0, not_a_pid]")), None);
        assert_eq!(parse_logcat_line(&epoch_line("am_proc_died", "[0]")), None);
        // Old -v tag format must be rejected
        assert_eq!(parse_logcat_line("I/am_proc_start: [0,1234,10000,com.test,service,{}]"), None);
    }

    #[test]
    fn test_struct_sizes_and_alignment() {
        use std::mem::{align_of, size_of};
        assert_eq!(size_of::<ResumeActivityEvent>(), 32);
        assert_eq!(align_of::<ResumeActivityEvent>(), 8);
        assert_eq!(size_of::<ProcStartEvent>(), 56);
        assert_eq!(align_of::<ProcStartEvent>(), 8);
        assert_eq!(size_of::<ProcDiedEvent>(), 4);
        assert_eq!(align_of::<ProcDiedEvent>(), 4);
        assert_eq!(size_of::<LogcatEvent>(), 64);
        assert_eq!(align_of::<LogcatEvent>(), 8);
    }

    #[test]
    fn test_logcat_chunked_stream_burst() {
        let mut total_stream = String::new();
        for i in 1..=1000 {
            total_stream.push_str(&format!("{TS}  1000  1000 I am_proc_start: [0,{},10000,com.test.spam{},service,{{}}]\n", i, i));
        }
        let stream_bytes = total_stream.as_bytes();
        let mut parsed_pids = Vec::new();
        for chunk_size in [1, 13, 67, 512, 4096] {
            parsed_pids.clear();
            let mut logcat_buf = Vec::with_capacity(4096);
            let mut offset = 0;
            while offset < stream_bytes.len() {
                let end = (offset + chunk_size).min(stream_bytes.len());
                logcat_buf.extend_from_slice(&stream_bytes[offset..end]);
                offset = end;
                let mut last_newline = 0;
                for (idx, &b) in logcat_buf.iter().enumerate() {
                    if b == b'\n' {
                        let mut line_bytes = &logcat_buf[last_newline..idx];
                        if let Some(&b'\r') = line_bytes.last() { line_bytes = &line_bytes[..line_bytes.len() - 1]; }
                        if !line_bytes.is_empty() && !line_bytes.starts_with(b"---") {
                            if let Ok(line) = std::str::from_utf8(line_bytes) {
                                if let Some((_, LogcatEvent::ProcStart(ps))) = parse_logcat_line(line.trim()) {
                                    parsed_pids.push(ps.pid);
                                }
                            }
                        }
                        last_newline = idx + 1;
                    }
                }
                if last_newline > 0 { logcat_buf.drain(..last_newline); }
                else if logcat_buf.len() > 8192 { logcat_buf.clear(); }
            }
            assert_eq!(parsed_pids.len(), 1000, "Failed for chunk_size {}", chunk_size);
            for (i, &pid) in parsed_pids.iter().enumerate() { assert_eq!(pid, (i + 1) as u32); }
        }
    }
}
