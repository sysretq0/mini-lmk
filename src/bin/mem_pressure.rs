use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== INDUCING CONTROLLED MEMORY PRESSURE FOR LMKD KILL LINE ===");

    // Spawn logcat listener
    let mut logcat = Command::new("logcat")
        .args(["-b", "all", "-s", "lmkd", "ActivityManager"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn logcat");

    let stdout = logcat.stdout.take().unwrap();
    let handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            if line.contains("Kill ") || line.contains("Killing ") || line.contains("lmkd") {
                println!(">> LOG: {}", line);
            }
        }
    });

    // Allocate memory progressively
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let chunk_size = 128 * 1024 * 1024; // 128 MB

    for i in 1..=35 {
        println!("Allocating chunk {} ({} MB total)...", i, i * 128);
        let mut chunk = vec![0u8; chunk_size];
        // Touch every 4KB page
        for p in (0..chunk.len()).step_by(4096) {
            chunk[p] = (i & 0xFF) as u8;
        }
        chunks.push(chunk);

        // Check meminfo
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            for line in meminfo.lines() {
                if line.starts_with("MemAvailable:") || line.starts_with("SwapFree:") {
                    println!("  {}", line);
                }
            }
        }

        // Check vmstat deltas
        if let Ok(vmstat) = std::fs::read_to_string("/proc/vmstat") {
            for line in vmstat.lines() {
                if line.starts_with("allocstall_normal") || line.starts_with("pgscan_direct") {
                    println!("  {}", line);
                }
            }
        }

        thread::sleep(Duration::from_millis(300));
    }

    println!("Pressure phase ended. Sleeping 2 seconds...");
    thread::sleep(Duration::from_secs(2));

    let _ = logcat.kill();
    let _ = logcat.wait();
    drop(chunks);
    println!("Memory released.");
}
