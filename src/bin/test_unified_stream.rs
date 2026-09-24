use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    println!("=== TESTING UNIFIED EVENT LOGCAT STREAM ===");

    let mut child = Command::new("logcat")
        .args([
            "-b", "events",
            "-v", "tag",
            "-s", "wm_resume_activity", "am_proc_start", "am_proc_died",
            "-T", "1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn logcat");

    let stdout = child.stdout.take().unwrap();
    let handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            println!(">> EVENT: {}", line);
        }
    });

    println!("Logcat stream started. Triggering app launches...");
    std::thread::sleep(Duration::from_millis(500));

    // Force stop calculator, launch it, then switch to settings
    let _ = Command::new("cmd").args(["activity", "force-stop", "com.google.android.calculator"]).status();
    std::thread::sleep(Duration::from_millis(300));

    let _ = Command::new("monkey").args(["-p", "com.google.android.calculator", "1"]).status();
    std::thread::sleep(Duration::from_secs(1));

    let _ = Command::new("monkey").args(["-p", "com.android.settings", "1"]).status();
    std::thread::sleep(Duration::from_secs(1));

    let _ = Command::new("cmd").args(["activity", "kill", "--user", "0", "com.google.android.calculator"]).status();
    std::thread::sleep(Duration::from_secs(1));

    let _ = child.kill();
    let _ = child.wait();
    let _ = handle.join();
    println!("=== UNIFIED STREAM TEST COMPLETE ===");
}
