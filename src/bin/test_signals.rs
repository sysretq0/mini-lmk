use std::process::Command;

fn main() {
    println!("=== TESTING SIGNALS AS SHELL ===");

    // 1. Test cn_proc (NETLINK_CONNECTOR = 11)
    unsafe {
        let sock = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM, 11);
        if sock < 0 {
            println!("cn_proc socket(AF_NETLINK, SOCK_DGRAM, 11): FAILED ({})", std::io::Error::last_os_error());
        } else {
            println!("cn_proc socket(AF_NETLINK, SOCK_DGRAM, 11): SUCCESS (fd={})", sock);
            libc::close(sock);
        }
    }

    // 2. Check past am_proc_start lines in logcat
    let out = Command::new("logcat")
        .args(["-d", "-b", "events", "-s", "am_proc_start"])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = stdout.lines().filter(|l| !l.starts_with("---")).collect();
            println!("logcat -b events -s am_proc_start: SUCCESS (found {} lines)", lines.len());
            for l in lines.iter().take(5) {
                println!("  SAMPLE: {}", l);
            }
        }
        Ok(o) => println!("logcat -b events -s am_proc_start: FAILED (exit={})", o.status),
        Err(e) => println!("logcat failed: {}", e),
    }

    // 3. Check past am_proc_died lines in logcat
    let out = Command::new("logcat")
        .args(["-d", "-b", "events", "-s", "am_proc_died"])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = stdout.lines().filter(|l| !l.starts_with("---")).collect();
            println!("logcat -b events -s am_proc_died: SUCCESS (found {} lines)", lines.len());
            for l in lines.iter().take(5) {
                println!("  SAMPLE: {}", l);
            }
        }
        Ok(o) => println!("logcat -b events -s am_proc_died: FAILED (exit={})", o.status),
        Err(e) => println!("logcat failed: {}", e),
    }
}
