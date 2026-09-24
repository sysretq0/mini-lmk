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

pub const MLMK_CONFIG_DIR: &str = "/data/local/tmp/mlmk/config";

pub const MLMK_LOGS_DIR: &str = "/data/local/tmp/mlmk/logs";
pub const CONFIG_FILE: &str = "/data/local/tmp/mlmk/config/daemon.conf";
pub const EXCLUDE_FILE: &str = "/data/local/tmp/mlmk/config/exclude.list";
pub const GAMES_FILE: &str = "/data/local/tmp/mlmk/config/games.list";
pub const OPERATIONS_LOG: &str = "/data/local/tmp/mlmk/logs/operations.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub t_idle_sec: u64,
    pub lru_protect_depth: usize,
    pub mem_critical_percent: u64,
    pub fg_lru_max_depth: usize,
    pub screen_off_harvest: bool,
    pub max_kills_per_pass: usize,
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
                    _ => {}
                }
            }
        }
    }

    pub fn load_from_file(&mut self, path: &str) {
        if let Ok(text) = std::fs::read_to_string(path) {
            self.parse_str(&text);
            println!(
                "[CONFIG] Active: t_idle={}s, lru_depth={}, mem_crit={}%, fg_lru_max={}, screen_off_harvest={}, max_kills_per_pass={}",
                self.t_idle_sec, self.lru_protect_depth, self.mem_critical_percent, self.fg_lru_max_depth, self.screen_off_harvest, self.max_kills_per_pass
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_config_parse() {
        let mut cfg = RuntimeConfig::default();
        assert_eq!(cfg.t_idle_sec, 180);
        assert_eq!(cfg.lru_protect_depth, 3);
        assert_eq!(cfg.mem_critical_percent, 10);
        assert_eq!(cfg.fg_lru_max_depth, 10);
        assert!(cfg.screen_off_harvest);
        assert_eq!(cfg.max_kills_per_pass, 2);

        let content = "
            # /data/local/tmp/mlmk/config/daemon.conf
            # Live-tunable volatile parameters
            t_idle_sec = 60
            lru_protect_depth = 5
            mem_critical_percent = 15
            fg_lru_max_depth = 20
            screen_off_harvest = false
            max_kills_per_pass = 4
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

        // Enforce lower bound of 1
        cfg.parse_str("max_kills_per_pass = 0");
        assert_eq!(cfg.max_kills_per_pass, 1);
    }

    #[test]
    fn test_inline_comments() {
        let mut cfg = RuntimeConfig::default();
        cfg.parse_str("t_idle_sec = 60 # aggressive\nlru_protect_depth=2# tight");
        assert_eq!(cfg.t_idle_sec, 60);
        assert_eq!(cfg.lru_protect_depth, 2);
    }
}
