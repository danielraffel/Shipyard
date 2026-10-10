//! Which boot of this host the current process belongs to.
//!
//! A process recorded under one boot cannot be alive under another: a reboot
//! ends every process. So a worker receipt that carries a different boot
//! identity than the running daemon is proof that its worker is gone, which no
//! process-table probe can give after a reboot (pids are reused).

/// The current boot's identity, or `None` when it cannot be read.
///
/// macOS: `kern.boottime` (`{ sec = …, usec = … } …`), stable for one boot.
/// Linux: `/proc/sys/kernel/random/boot_id`, a UUID per boot.
#[must_use]
pub fn current() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.boottime"])
            .env_clear()
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return (!text.is_empty()).then_some(text);
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let text = text.trim().to_owned();
        return (!text.is_empty()).then_some(text);
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod tests {
    /// Elsewhere there is no boot identity, so no receipt ever proves a
    /// reboot and the boot requeue never runs: lost workers stay UNCERTAIN.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn other_platforms_have_no_boot_identity() {
        assert_eq!(super::current(), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_boot_identity_is_readable_and_stable_within_one_boot() {
        let first = super::current().expect("boot identity");
        assert!(!first.is_empty());
        assert_eq!(super::current().as_deref(), Some(first.as_str()));
    }
}
