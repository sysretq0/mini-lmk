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

use std::io::Write;
use std::io::stdout;
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
        }
    }
}

impl RuntimeConfig {
    pub fn parse_str(&mut self, text: &str) {
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
                            self.max_kills_per_pass = num.max(1);
                        }
                    }
                    "min_oom_score_adj" => {
                        if let Ok(num) = val.parse::<i32>() {
                            self.min_oom_score_adj = num.clamp(500, 900);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn load_from_file(&mut self, path: &str, quiet: bool) {
        if let Ok(text) = std::fs::read_to_string(path) {
            self.parse_str(&text);
            if !quiet {
                let _ = writeln!(stdout(),
                    "[CONFIG] Active: t_idle={}s, lru_depth={}, mem_crit={}%, fg_lru_max={}, screen_off_harvest={}, max_kills_per_pass={}, min_oom_adj={}",
                    self.t_idle_sec, self.lru_protect_depth, self.mem_critical_percent, self.fg_lru_max_depth, self.screen_off_harvest, self.max_kills_per_pass, self.min_oom_score_adj
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
        assert_eq!(p_default.config_file, "/data/local/tmp/mlmk/config/daemon.conf");
        assert_eq!(p_default.exclude_file, "/data/local/tmp/mlmk/config/exclude.list");
        assert_eq!(p_default.games_file, "/data/local/tmp/mlmk/config/games.list");
        assert_eq!(p_default.operations_log, "/data/local/tmp/mlmk/logs/operations.log");

        let p_mod = ConfigPaths::from_base("/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk/");
        assert_eq!(p_mod.base_dir, "/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk");
        assert_eq!(p_mod.config_file, "/data/user_de/0/com.android.shell/axeron/plugins/mini_lmk/mlmk/config/daemon.conf");
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
            # Unknown keys should be safely ignored
            invalid_key = 999
        ";
        cfg.parse_str(content);
        assert_eq!(cfg.t_idle_sec, 60);
        assert_eq!(cfg.lru_protect_depth, 5);
        assert_eq!(cfg.mem_critical_percent, 15);
        assert_eq!(cfg.fg_lru_max_depth, 20);
        assert!(!cfg.screen_off_harvest);
        assert_eq!(cfg.max_kills_per_pass, 4);
        assert_eq!(cfg.min_oom_score_adj, 700);

        // Enforce lower bound of 1 for max_kills_per_pass
        cfg.parse_str("max_kills_per_pass = 0");
        assert_eq!(cfg.max_kills_per_pass, 1);

        // Clamp min_oom_score_adj to 500..=900
        cfg.parse_str("min_oom_score_adj = 300");
        assert_eq!(cfg.min_oom_score_adj, 500);
        cfg.parse_str("min_oom_score_adj = 1000");
        assert_eq!(cfg.min_oom_score_adj, 900);
    }

    #[test]
    fn test_inline_comments() {
        let mut cfg = RuntimeConfig::default();
        cfg.parse_str("t_idle_sec = 60 # aggressive\nlru_protect_depth=2# tight");
        assert_eq!(cfg.t_idle_sec, 60);
        assert_eq!(cfg.lru_protect_depth, 2);
    }
}
