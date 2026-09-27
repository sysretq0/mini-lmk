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

use std::io::{stderr, stdout};
use std::io::Write;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub base_dir: String,
    pub config_dir: String,
    pub logs_dir: String,
    pub config_file: String,
    pub exclude_file: String,
    pub games_file: String,
    pub operations_log: String,
}

impl ConfigPaths {
    pub fn get() -> &'static ConfigPaths {
        static INSTANCE: OnceLock<ConfigPaths> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let base_dir = std::env::var("MODPATH")
                .ok()
                .filter(|p| !p.trim().is_empty())
                .map(|p| format!("{}/mlmk", p.trim().trim_end_matches('/')))
                .unwrap_or_else(|| "/data/local/tmp/mlmk".to_string());

            Self::from_base(&base_dir)
        })
    }

    pub fn from_base(base: &str) -> Self {
        let base_dir = base.trim_end_matches('/').to_string();
        let config_dir = format!("{}/config", base_dir);
        let logs_dir = format!("{}/logs", base_dir);
        let config_file = format!("{}/daemon.conf", config_dir);
        let exclude_file = format!("{}/exclude.list", config_dir);
        let games_file = format!("{}/games.list", config_dir);
        let operations_log = format!("{}/operations.log", logs_dir);

        ConfigPaths {
            base_dir,
            config_dir,
            logs_dir,
            config_file,
            exclude_file,
            games_file,
            operations_log,
        }
    }

    /// Ensure the base directory structure, list files, and default daemon.conf exist on disk.
    /// Returns true if the default configuration template was written.
    pub fn ensure_default_files(&self, quiet: bool) -> bool {
        let _ = std::fs::create_dir_all(&self.config_dir);
        let _ = std::fs::create_dir_all(&self.logs_dir);
        let p_ex = std::path::Path::new(&self.exclude_file);
        if !p_ex.exists() {
            let _ = std::fs::write(
                &self.exclude_file,
                "# Package names excluded from eviction (one per line)\n",
            );
        }
        let p_gm = std::path::Path::new(&self.games_file);
        if !p_gm.exists() {
            let _ = std::fs::write(
                &self.games_file,
                "# Game package names monitored for game session mode (one per line)\n",
            );
        }
        RuntimeConfig::ensure_default_file(&self.config_file, quiet)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub t_idle_sec: u64,
    pub lru_protect_depth: usize,
    pub mem_critical_percent: u64,
    pub fg_lru_max_depth: usize,
    pub screen_off_harvest: bool,
    pub max_kills_per_pass: usize,
    pub min_oom_score_adj: i32,
    pub log_enabled: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            t_idle_sec: 180,
            lru_protect_depth: 3,
            mem_critical_percent: 10,
            fg_lru_max_depth: 10,
            screen_off_harvest: true,
            max_kills_per_pass: 2,
            min_oom_score_adj: 900,
            log_enabled: true,
        }
    }
}

pub const DEFAULT_DAEMON_CONF: &str = r#"# mini-lmk runtime configuration (daemon.conf)
# Live-tunable parameters, reloaded automatically via inotify.

# Base background idle timeout before eviction eligibility (seconds)
t_idle_sec = 180

# Number of recently visited foreground packages immune from eviction
lru_protect_depth = 3

# Low-memory watermark triggering emergency T_idle=10s grace window.
# With kernel PSI (4.20+): percent of memory-stall time (some avg10). Otherwise:
# percent of MemTotal (MemAvailable watermark). One knob, whichever backend answers.
# 0 disables the low-memory escalation entirely.
mem_critical_percent = 10

# Maximum depth of the foreground history ring buffer
fg_lru_max_depth = 10

# Deep screen-off harvesting (F15/F16: ceilings t_idle at 30/60s and lru depth at 1 —
# can only tighten the configured baseline, never loosen it)
screen_off_harvest = true

# Eviction burst cap: maximum background apps evicted per reap pass.
# Uncomment to override auto-scaling (<=4.5GB RAM -> 4, 4.5-8.5GB -> 2, >8.5GB -> 1).
# max_kills_per_pass = 2

# Minimum oom_score_adj threshold required for eviction (range: 500-900, default: 900)
# 900: Conservative (cached & idle processes only; protects all background services)
# 500: Aggressive (matches AOSP SERVICE_ADJ; reclaims background services for games/heavy loads)
min_oom_score_adj = 900

# Append NDJSON records to logs/operations.log. Telemetry only: kill decisions,
# the terminal table, and the --json stream are unaffected.
log_enabled = true
"#;

impl RuntimeConfig {
    /// Ensure the configuration file exists on disk. If missing, creates the parent directory,
    /// writes the documented default configuration template, and prints a notification
    /// to stdout unless quiet is requested.
    ///
    /// Returns true if the template was written, or false if the file was already present.
    pub fn ensure_default_file(path: &str, quiet: bool) -> bool {
        let p = std::path::Path::new(path);
        if p.exists() {
            return false;
        }
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(path, DEFAULT_DAEMON_CONF).is_ok() {
            if !quiet {
                let _ = writeln!(
                    stdout(),
                    "[CONFIG] Created default configuration at {}",
                    path
                );
            }
            true
        } else {
            false
        }
    }

    /// Parse one config body. `quiet` gates the clamp/floor notices: `DaemonState::new()`
    /// preloads the file to read `log_enabled` before the sink exists, and the full reload
    /// loads it again — unguarded, every clamped value printed its warning twice (F23).
    pub fn parse_str(&mut self, text: &str, quiet: bool) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.split('#').next().unwrap_or(v).trim();

                match key {
                    "t_idle_sec" => {
                        if let Ok(num) = val.parse::<u64>() {
                            self.t_idle_sec = num;
                        }
                    }
                    "lru_protect_depth" => {
                        if let Ok(num) = val.parse::<usize>() {
                            self.lru_protect_depth = num;
                        }
                    }
                    "mem_critical_percent" => {
                        if let Ok(num) = val.parse::<u64>() {
                            self.mem_critical_percent = num;
                        }
                    }
                    "fg_lru_max_depth" => {
                        if let Ok(num) = val.parse::<usize>() {
                            self.fg_lru_max_depth = num;
                        }
                    }
                    "screen_off_harvest" => {
                        if let Ok(b) = val.parse::<bool>() {
                            self.screen_off_harvest = b;
                        }
                    }
                    "max_kills_per_pass" => {
                        if let Ok(num) = val.parse::<usize>() {
                            if num == 0 {
                                // F17 (REFACTORING.md): the floor is deliberate, the silence is not.
                                // 0 stays 1 — a 0 here must not look applied, and it must not become
                                // a stealth off-switch: it would silence all kill telemetry, which
                                // reads like a dead pipeline. Pausing eviction is --observe or a
                                // future dedicated `eviction_enabled` key, not this knob.
                                if !quiet {
                                    let _ = writeln!(stderr(),
                                        "[CONFIG] max_kills_per_pass = 0 below minimum 1; using 1 (use --observe to run without kills)");
                                }
                            }
                            self.max_kills_per_pass = num.max(1);
                        }
                    }
                    "min_oom_score_adj" => {
                        if let Ok(num) = val.parse::<i32>() {
                            let clamped = num.clamp(500, 900);
                            if clamped != num {
                                // F1 (REFACTORING.md): the clamp is deliberate, the silence is not.
                                // A value someone set to 300 must not look applied.
                                if !quiet {
                                    let _ = writeln!(
                                        stderr(),
                                        "[CONFIG] min_oom_score_adj = {} outside 500..=900; clamped to {}",
                                        num, clamped
                                    );
                                }
                            }
                            self.min_oom_score_adj = clamped;
                        }
                    }
                    "log_enabled" => {
                        if let Ok(b) = val.parse::<bool>() {
                            self.log_enabled = b;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn load_from_file(&mut self, path: &str, quiet: bool) {
        if let Ok(text) = std::fs::read_to_string(path) {
            self.parse_str(&text, quiet);
            if !quiet {
                let _ = writeln!(stdout(),
                    "[CONFIG] Active: t_idle={}s, lru_depth={}, mem_crit={}%, fg_lru_max={}, screen_off_harvest={}, max_kills_per_pass={}, min_oom_adj={}, log_enabled={}",
                    self.t_idle_sec, self.lru_protect_depth, self.mem_critical_percent, self.fg_lru_max_depth, self.screen_off_harvest, self.max_kills_per_pass, self.min_oom_score_adj, self.log_enabled
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_paths_resolution() {
        let p_default = ConfigPaths::from_base("/data/local/tmp/mlmk");
        assert_eq!(p_default.base_dir, "/data/local/tmp/mlmk");
        assert_eq!(p_default.config_dir, "/data/local/tmp/mlmk/config");
        assert_eq!(p_default.logs_dir, "/data/local/tmp/mlmk/logs");
        assert_eq!(
            p_default.config_file,
            "/data/local/tmp/mlmk/config/daemon.conf"
        );
        assert_eq!(
            p_default.exclude_file,
            "/data/local/tmp/mlmk/config/exclude.list"
        );
        assert_eq!(
            p_default.games_file,
            "/data/local/tmp/mlmk/config/games.list"
        );
        assert_eq!(
            p_default.operations_log,
            "/data/local/tmp/mlmk/logs/operations.log"
        );

        let p_mod = ConfigPaths::from_base(
            "/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk/",
        );
        assert_eq!(
            p_mod.base_dir,
            "/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk"
        );
        assert_eq!(
            p_mod.config_file,
            "/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk/config/daemon.conf"
        );
    }

    #[test]
    fn test_runtime_config_parse() {
        let mut cfg = RuntimeConfig::default();
        assert_eq!(cfg.t_idle_sec, 180);
        assert_eq!(cfg.lru_protect_depth, 3);
        assert_eq!(cfg.mem_critical_percent, 10);
        assert_eq!(cfg.fg_lru_max_depth, 10);
        assert!(cfg.screen_off_harvest);
        assert_eq!(cfg.max_kills_per_pass, 2);
        assert_eq!(cfg.min_oom_score_adj, 900);
        assert!(cfg.log_enabled, "on unless daemon.conf says otherwise");

        let content = "
            # /data/local/tmp/mlmk/config/daemon.conf
            # Live-tunable volatile parameters
            t_idle_sec = 60
            lru_protect_depth = 5
            mem_critical_percent = 15
            fg_lru_max_depth = 20
            screen_off_harvest = false
            max_kills_per_pass = 4
            min_oom_score_adj = 700
            log_enabled = false
            # Unknown keys should be safely ignored
            invalid_key = 999
        ";
        cfg.parse_str(content, true);
        assert_eq!(cfg.t_idle_sec, 60);
        assert_eq!(cfg.lru_protect_depth, 5);
        assert_eq!(cfg.mem_critical_percent, 15);
        assert_eq!(cfg.fg_lru_max_depth, 20);
        assert!(!cfg.screen_off_harvest);
        assert_eq!(cfg.max_kills_per_pass, 4);
        assert_eq!(cfg.min_oom_score_adj, 700);
        assert!(!cfg.log_enabled);

        // F17 regression: 0 is floored to 1 (kill telemetry must stay observable), never 0.
        cfg.parse_str("max_kills_per_pass = 0\n", true);
        assert_eq!(cfg.max_kills_per_pass, 1, "0 must not become a silent off-switch");

        // An unparseable value leaves the previous setting alone rather than guessing
        cfg.parse_str("log_enabled = maybe", true);
        assert!(!cfg.log_enabled);
        cfg.parse_str("log_enabled = true", true);
        assert!(cfg.log_enabled);

        // Enforce lower bound of 1 for max_kills_per_pass
        cfg.parse_str("max_kills_per_pass = 0", true);
        assert_eq!(cfg.max_kills_per_pass, 1);

        // Clamp min_oom_score_adj to 500..=900
        cfg.parse_str("min_oom_score_adj = 300", true);
        assert_eq!(cfg.min_oom_score_adj, 500);
        cfg.parse_str("min_oom_score_adj = 1000", true);
        assert_eq!(cfg.min_oom_score_adj, 900);
    }

    #[test]
    fn test_inline_comments() {
        let mut cfg = RuntimeConfig::default();
        cfg.parse_str("t_idle_sec = 60 # aggressive\nlru_protect_depth=2# tight", true);
        assert_eq!(cfg.t_idle_sec, 60);
        assert_eq!(cfg.lru_protect_depth, 2);
    }

    #[test]
    fn test_ensure_default_file_and_paths() {
        let base_dir = std::env::temp_dir().join(format!("mlmk_cfg_test_{}", std::process::id()));
        let base_str = base_dir.to_string_lossy().to_string();
        let paths = ConfigPaths::from_base(&base_str);

        assert!(!std::path::Path::new(&paths.config_file).exists());
        assert!(!std::path::Path::new(&paths.exclude_file).exists());
        assert!(!std::path::Path::new(&paths.games_file).exists());

        // First call creates directories, list placeholders, and daemon.conf
        assert!(paths.ensure_default_files(true));
        assert!(std::path::Path::new(&paths.config_file).exists());
        assert!(std::path::Path::new(&paths.exclude_file).exists());
        assert!(std::path::Path::new(&paths.games_file).exists());

        // Second call sees existing file and returns false without overwriting
        assert!(!paths.ensure_default_files(true));

        // Verify the template parses to defaults
        let mut cfg = RuntimeConfig::default();
        cfg.load_from_file(&paths.config_file, true);
        assert_eq!(cfg.t_idle_sec, 180);
        assert_eq!(cfg.lru_protect_depth, 3);
        assert_eq!(cfg.mem_critical_percent, 10);
        assert_eq!(cfg.fg_lru_max_depth, 10);
        assert!(cfg.screen_off_harvest);
        assert_eq!(cfg.max_kills_per_pass, 2);
        assert_eq!(cfg.min_oom_score_adj, 900);
        assert!(cfg.log_enabled);

        let _ = std::fs::remove_dir_all(&base_dir);
    }
}
