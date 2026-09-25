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

#![allow(dead_code, unused_imports)]

//! Standalone, criterion-free microbenchmark harness for mini-lmk.
//! Evaluates per-call statistical latency distributions across N iterations,
//! including multi-PID cold procfs access and CFS scheduler preemption diagnostics.

use std::hint::black_box;
use std::time::Instant;

#[path = "../src/procfs.rs"]
mod procfs;

#[path = "../src/telemetry.rs"]
mod telemetry;

struct BenchmarkResult {
    name: &'static str,
    iterations: usize,
    min_ns: u64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    mean_ns: f64,
    preempted_outliers: usize,
}

impl BenchmarkResult {
    fn from_samples(name: &'static str, mut samples: Vec<u64>, preempted_outliers: usize) -> Self {
        assert!(!samples.is_empty());
        samples.sort_unstable();
        let n = samples.len();
        let sum: u64 = samples.iter().sum();
        let mean_ns = sum as f64 / n as f64;

        let p50_idx = ((n as f64) * 0.50).round() as usize;
        let p95_idx = ((n as f64) * 0.95).round() as usize;
        let p99_idx = ((n as f64) * 0.99).round() as usize;

        Self {
            name,
            iterations: n,
            min_ns: samples[0],
            p50_ns: samples[p50_idx.min(n - 1)],
            p95_ns: samples[p95_idx.min(n - 1)],
            p99_ns: samples[p99_idx.min(n - 1)],
            max_ns: samples[n - 1],
            mean_ns,
            preempted_outliers,
        }
    }
}

fn format_ns(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{:6.1} ns", ns)
    } else if ns < 1_000_000.0 {
        format!("{:6.2} µs", ns / 1_000.0)
    } else {
        format!("{:6.2} ms", ns / 1_000_000.0)
    }
}

const RUSAGE_THREAD: libc::c_int = 1;

fn get_thread_nivcsw() -> i64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(RUSAGE_THREAD, &mut usage);
    }
    usage.ru_nivcsw
}

fn get_thread_cputime_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

fn run_bench<F, R>(name: &'static str, warmup: usize, iters: usize, mut op: F) -> BenchmarkResult
where
    F: FnMut() -> R,
{
    // Warmup cycles to prime caches
    for _ in 0..warmup {
        black_box(op());
    }

    let mut samples = Vec::with_capacity(iters);
    let mut preempted_count = 0usize;

    for _ in 0..iters {
        let cs_before = get_thread_nivcsw();
        let cpu_before = get_thread_cputime_ns();
        let start = Instant::now();

        let val = op();

        let elapsed = start.elapsed();
        let cpu_after = get_thread_cputime_ns();
        let cs_after = get_thread_nivcsw();

        black_box(val);
        let nanos = elapsed.as_nanos() as u64;
        samples.push(nanos);

        // An iteration taking > 500 µs where thread CPU time remained < 50 µs or
        // involuntary context switches incremented indicates OS CFS scheduler preemption.
        if nanos > 500_000 && (cs_after > cs_before || (cpu_after.saturating_sub(cpu_before)) < 50_000) {
            preempted_count += 1;
        }
    }

    BenchmarkResult::from_samples(name, samples, preempted_count)
}

fn collect_external_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let self_pid = std::process::id();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() {
                if pid != self_pid && pid > 1 {
                    let mut path_buf = [0u8; 32];
                    procfs::format_proc_path(&mut path_buf, pid, "oom_score_adj");
                    let fd = unsafe {
                        libc::open(path_buf.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC)
                    };
                    if fd >= 0 {
                        unsafe { libc::close(fd); }
                        pids.push(pid);
                    }
                }
            }
        }
    }
    if pids.is_empty() {
        pids.push(self_pid);
    }
    pids
}

fn print_text_table(results: &[BenchmarkResult]) {
    println!("\n=== mini-lmk Microbenchmark Suite ===");
    println!(
        "{:<42} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>9}",
        "Target / Workload", "Iterations", "Min", "P50 (Med)", "P95", "P99", "Max", "Mean", "Preempted"
    );
    println!("{:-<42}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<9}", "", "", "", "", "", "", "", "", "");

    for r in results {
        println!(
            "{:<42} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>10} | {:>9}",
            r.name,
            r.iterations,
            format_ns(r.min_ns as f64),
            format_ns(r.p50_ns as f64),
            format_ns(r.p95_ns as f64),
            format_ns(r.p99_ns as f64),
            format_ns(r.max_ns as f64),
            format_ns(r.mean_ns),
            r.preempted_outliers,
        );
    }
}

fn print_markdown_table(results: &[BenchmarkResult]) {
    println!("\n### Microbenchmark Empirical Latency Distribution");
    println!("| Target / Operation | Source / Mechanism | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean | Flagged Preemption Samples |");
    println!("|---|---|---|---|---|---|---|---|---|---|");

    for r in results {
        let (name, mechanism) = if r.name.contains('(') {
            let parts: Vec<&str> = r.name.splitn(2, '(').collect();
            (parts[0].trim(), parts[1].trim_end_matches(')').trim())
        } else {
            (r.name, "default")
        };

        println!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            name,
            mechanism,
            r.iterations,
            format_ns(r.min_ns as f64).trim(),
            format_ns(r.p50_ns as f64).trim(),
            format_ns(r.p95_ns as f64).trim(),
            format_ns(r.p99_ns as f64).trim(),
            format_ns(r.max_ns as f64).trim(),
            format_ns(r.mean_ns).trim(),
            r.preempted_outliers,
        );
    }
}

fn print_csv(results: &[BenchmarkResult]) {
    println!("target,iterations,min_ns,p50_ns,p95_ns,p99_ns,max_ns,mean_ns,preempted_outliers");
    for r in results {
        println!(
            "{},{},{},{},{},{},{},{:.2},{}",
            r.name, r.iterations, r.min_ns, r.p50_ns, r.p95_ns, r.p99_ns, r.max_ns, r.mean_ns, r.preempted_outliers
        );
    }
}

fn print_json(results: &[BenchmarkResult]) {
    println!("[");
    for (i, r) in results.iter().enumerate() {
        let comma = if i + 1 < results.len() { "," } else { "" };
        println!(
            "  {{\"target\":\"{}\",\"iterations\":{},\"min_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"mean_ns\":{:.2},\"preempted_outliers\":{}}}{}",
            r.name, r.iterations, r.min_ns, r.p50_ns, r.p95_ns, r.p99_ns, r.max_ns, r.mean_ns, r.preempted_outliers, comma
        );
    }
    println!("]");
}

fn print_help() {
    println!(
        r#"mini-lmk microbenchmark harness
Usage: microbench [OPTIONS]

Options:
  -n, --iterations <N>  Number of timed iterations for microbenches (default: 10000)
  -w, --warmup <N>      Number of warm-up iterations (default: 1000)
  --json                Emit results as JSON array
  --csv                 Emit results as CSV
  --markdown            Emit results as Markdown table only
  -h, --help            Print this help message
"#
    );
}

fn main() {
    let mut iterations = 10_000usize;
    let mut warmup = 1_000usize;
    let mut json_out = false;
    let mut csv_out = false;
    let mut md_only = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--iterations" => {
                if i + 1 < args.len() {
                    iterations = args[i + 1].parse().unwrap_or(10_000);
                    i += 1;
                }
            }
            "-w" | "--warmup" => {
                if i + 1 < args.len() {
                    warmup = args[i + 1].parse().unwrap_or(1_000);
                    i += 1;
                }
            }
            "--json" => json_out = true,
            "--csv" => csv_out = true,
            "--markdown" => md_only = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            other => {
                if let Ok(num) = other.parse::<usize>() {
                    iterations = num;
                } else {
                    eprintln!("[WARN] Unknown benchmark option: {}", other);
                }
            }
        }
        i += 1;
    }

    let self_pid = std::process::id();
    let external_pids = collect_external_pids();
    let pid_count = external_pids.len();

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size_kb = if page_size > 0 { (page_size as u64) / 1024 } else { 4 };

    let mut results = Vec::new();

    // 1. Timer overhead baseline (Instant::now resolution)
    results.push(run_bench("Instant::now (measurement overhead)", warmup, iterations, || {
        let t0 = Instant::now();
        black_box(());
        t0.elapsed()
    }));

    // 2. procfs::read_oom_score_adj (Self, Cache Hot)
    results.push(run_bench("procfs::read_oom_score_adj (self, hot cache)", warmup, iterations, || {
        procfs::read_oom_score_adj(self_pid)
    }));

    // 3. procfs::read_oom_score_adj (Multi-PID Pool, Real World)
    let mut pid_idx = 0usize;
    results.push(run_bench("procfs::read_oom_score_adj (multi-PID, pool)", warmup, iterations, || {
        let pid = external_pids[pid_idx % pid_count];
        pid_idx += 1;
        procfs::read_oom_score_adj(pid)
    }));

    // 4. procfs::read_statm_rss_kb (Self, Cache Hot)
    results.push(run_bench("procfs::read_statm_rss_kb (self, hot cache)", warmup, iterations, || {
        procfs::read_statm_rss_kb(self_pid, page_size_kb)
    }));

    // 5. procfs::read_statm_rss_kb (Multi-PID Pool, Real World)
    let mut pid_idx2 = 0usize;
    results.push(run_bench("procfs::read_statm_rss_kb (multi-PID, pool)", warmup, iterations, || {
        let pid = external_pids[pid_idx2 % pid_count];
        pid_idx2 += 1;
        procfs::read_statm_rss_kb(pid, page_size_kb)
    }));

    // 6. procfs::read_meminfo_kb
    results.push(run_bench("procfs::read_meminfo_kb (/proc/meminfo)", warmup, iterations, || {
        procfs::read_meminfo_kb()
    }));

    // 7. telemetry::FormattedTime (FormattedTime)
    let mut epoch_counter = 1_774_436_400_000u64;
    results.push(run_bench("telemetry::FormattedTime (stack time format)", warmup, iterations, || {
        epoch_counter += 1;
        let ft = telemetry::format_time_hms_ms(epoch_counter);
        black_box(&*ft);
    }));

    if json_out {
        print_json(&results);
    } else if csv_out {
        print_csv(&results);
    } else if md_only {
        print_markdown_table(&results);
    } else {
        print_text_table(&results);
        print_markdown_table(&results);
    }
}
