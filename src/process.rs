use std::process::Child;
use std::time::Duration;

pub(crate) fn stop_process_group(child: &mut Child) {
    // Leave room for the audio guard's 10-second recovery wait and supervisor cleanup.
    stop_with_grace(child, Duration::from_secs(15), Duration::from_secs(2));
}

/// Give the package supervisor time to restore shared resources while its
/// dependencies are still alive, then remove any remaining group members.
/// The caller must have spawned the child with `process_group(0)`.
fn stop_with_grace(child: &mut Child, grace: Duration, fallback: Duration) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        let group = -pid;
        if matches!(child.try_wait(), Ok(None)) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let deadline = std::time::Instant::now() + grace;
            while std::time::Instant::now() < deadline {
                if !matches!(child.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        // The supervisor may already have exited while leaving helpers alive.
        unsafe { libc::kill(group, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + fallback;
        while std::time::Instant::now() < deadline {
            let _ = child.try_wait();
            if unsafe { libc::kill(group, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = (grace, fallback);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        child: Child,
        dir: PathBuf,
    }

    impl Fixture {
        fn start(script: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "luna-stop-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let child = Command::new("sh")
                .args(["-c", script])
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            let fixture = Self { child, dir };
            fixture.wait_for("ready");
            fixture
        }

        fn wait_for(&self, file: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.dir.join(file).exists() {
                assert!(Instant::now() < deadline, "fixture did not create {file}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn stop(&mut self) {
            stop_with_grace(
                &mut self.child,
                Duration::from_secs(1),
                Duration::from_millis(200),
            );
        }

        fn assert_helper_stopped(&self) {
            let pid = fs::read_to_string(self.dir.join("helper.pid")).unwrap();
            let stat = format!("/proc/{}/stat", pid.trim());
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match fs::read_to_string(&stat) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                    Ok(s) if s.rsplit_once(") ").unwrap().1.starts_with('Z') => return,
                    _ => {}
                }
                assert!(Instant::now() < deadline, "helper remains running");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn supervisor_can_recover_before_dependencies_receive_term() {
        let mut f = Fixture::start(
            r#"
            sh -c 'trap "touch helper-stopped; exit 0" TERM; touch helper-ready; while :; do sleep 0.02; done' &
            helper=$!
            echo "$helper" > helper.pid
            while [ ! -f helper-ready ]; do sleep 0.01; done
            trap 'sleep 0.15; if [ ! -f helper-stopped ]; then touch recovered; fi; kill "$helper"; wait "$helper"; exit 0' TERM
            touch ready
            while :; do sleep 0.02; done
        "#,
        );
        stop_process_group(&mut f.child);
        assert!(
            f.dir.join("recovered").exists(),
            "dependency terminated before recovery completed"
        );
        f.assert_helper_stopped();
    }

    #[test]
    fn stubborn_supervisor_and_helper_are_killed_within_bound() {
        let mut f = Fixture::start(
            r#"
            trap '' TERM
            sh -c 'trap "" TERM; touch helper-ready; while :; do sleep 0.02; done' &
            echo "$!" > helper.pid
            while [ ! -f helper-ready ]; do sleep 0.01; done
            touch ready
            while :; do sleep 0.02; done
        "#,
        );
        let started = Instant::now();
        f.stop();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(f.child.try_wait().unwrap().is_some());
        f.assert_helper_stopped();
    }

    #[test]
    fn exited_supervisor_does_not_leave_stubborn_helper() {
        let mut f = Fixture::start(
            r#"
            sh -c 'trap "" TERM; touch helper-ready; while :; do sleep 0.02; done' &
            echo "$!" > helper.pid
            while [ ! -f helper-ready ]; do sleep 0.01; done
            touch ready
            exit 0
        "#,
        );
        f.child.wait().unwrap();
        f.stop();
        f.assert_helper_stopped();
    }
}
