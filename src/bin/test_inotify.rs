use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;

fn main() {
    println!("=== INOTIFY & PACKAGES.XML TEST ===");

    unsafe {
        let ifd = libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC);
        if ifd < 0 {
            println!("inotify_init1: FAILED ({})", std::io::Error::last_os_error());
            return;
        }
        println!("inotify_init1: SUCCESS (fd={})", ifd);

        // 1. Test inotify on /data/local/tmp/mlmk/
        let dir = CString::new("/data/local/tmp/mlmk").unwrap();
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
        let wd = libc::inotify_add_watch(ifd, dir.as_ptr(), mask);
        if wd < 0 {
            println!("inotify_add_watch(/data/local/tmp/mlmk): FAILED ({})", std::io::Error::last_os_error());
        } else {
            println!("inotify_add_watch(/data/local/tmp/mlmk): SUCCESS (wd={})", wd);

            // Test file operations inside /data/local/tmp/mlmk
            let test_file = "/data/local/tmp/mlmk/exclude.list";
            {
                let mut f = File::create(test_file).expect("create exclude.list");
                writeln!(f, "com.example.app").expect("write exclude.list");
                writeln!(f, "# comment line").expect("write comment");
                writeln!(f, "com.whatsapp").expect("write whatsapp");
            }

            // Read events from inotify
            let mut buf = [0u8; 1024];
            let n = libc::read(ifd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n > 0 {
                println!("  VERIFIED: inotify event triggered on /data/local/tmp/mlmk (read {} bytes)", n);
                let event = &*(buf.as_ptr() as *const libc::inotify_event);
                println!("  Event wd: {}, mask: 0x{:x}, name_len: {}", event.wd, event.mask, event.len);
                if event.len > 0 {
                    let name_bytes = &buf[std::mem::size_of::<libc::inotify_event>()..std::mem::size_of::<libc::inotify_event>() + event.len as usize];
                    let name = String::from_utf8_lossy(name_bytes).trim_matches('\0').to_string();
                    println!("  Event file: {}", name);
                }
            } else {
                println!("  No inotify event read (n={})", n);
            }
        }

        // 2. Test inotify on /data/system/packages.xml
        let pkg_xml = CString::new("/data/system/packages.xml").unwrap();
        let wd_xml = libc::inotify_add_watch(ifd, pkg_xml.as_ptr(), libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB);
        if wd_xml < 0 {
            println!("inotify_add_watch(/data/system/packages.xml): FAILED ({})", std::io::Error::last_os_error());
        } else {
            println!("inotify_add_watch(/data/system/packages.xml): SUCCESS (wd={})", wd_xml);
        }

        // 3. Test inotify on /data/system/ directory
        let data_sys = CString::new("/data/system").unwrap();
        let wd_sys = libc::inotify_add_watch(ifd, data_sys.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE);
        if wd_sys < 0 {
            println!("inotify_add_watch(/data/system): FAILED ({})", std::io::Error::last_os_error());
        } else {
            println!("inotify_add_watch(/data/system): SUCCESS (wd={})", wd_sys);
        }

        // 4. Test stat on /data/system/packages.xml
        let mut stat_buf: libc::stat = std::mem::zeroed();
        let ret = libc::stat(pkg_xml.as_ptr(), &mut stat_buf);
        if ret == 0 {
            println!("stat(/data/system/packages.xml): SUCCESS (mtime={}, size={}, mode=0{:o})",
                stat_buf.st_mtime, stat_buf.st_size, stat_buf.st_mode);
        } else {
            println!("stat(/data/system/packages.xml): FAILED ({})", std::io::Error::last_os_error());
        }

        libc::close(ifd);
    }
}
