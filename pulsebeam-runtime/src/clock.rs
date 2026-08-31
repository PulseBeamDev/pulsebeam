const TIME_ERROR: i32 = 5;
const STA_UNSYNC: u64 = 0x0040;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisciplineStatus {
    pub state: i32,
    pub status: u64,
}

impl DisciplineStatus {
    pub const fn new(state: i32, status: u64) -> Self {
        Self { state, status }
    }

    pub const fn synchronized(self) -> bool {
        self.state != TIME_ERROR && self.status & STA_UNSYNC == 0
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;

    use super::DisciplineStatus;

    #[allow(
        unsafe_code,
        reason = "adjtimex is the Linux kernel API used to read clock discipline status"
    )]
    pub fn read() -> io::Result<DisciplineStatus> {
        let mut timex = std::mem::MaybeUninit::<libc::timex>::zeroed();
        // SAFETY: zeroed timex is the adjtimex(2) input and libc fills it before returning.
        let state = unsafe { libc::adjtimex(timex.as_mut_ptr()) };
        if state < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a non-negative adjtimex return means the kernel filled timex.
        let timex = unsafe { timex.assume_init() };
        Ok(DisciplineStatus {
            state,
            status: u64::try_from(timex.status).unwrap_or_default(),
        })
    }
}

#[cfg(target_os = "linux")]
pub use linux::read;

fn validate(status: DisciplineStatus) -> std::io::Result<()> {
    if status.synchronized() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "kernel clock discipline is unsynchronized",
        ))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> std::io::Result<DisciplineStatus> {
    Ok(DisciplineStatus::new(0, 0))
}

pub fn ensure_synchronized() -> std::io::Result<()> {
    #[cfg(feature = "sim")]
    {
        Ok(())
    }
    #[cfg(not(feature = "sim"))]
    {
        ensure_synchronized_with(read)
    }
}

pub fn ensure_synchronized_with(
    reader: impl FnOnce() -> std::io::Result<DisciplineStatus>,
) -> std::io::Result<()> {
    validate(reader()?)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{DisciplineStatus, ensure_synchronized_with};

    #[test]
    fn injected_synchronized_status_is_accepted() {
        assert!(ensure_synchronized_with(|| Ok(DisciplineStatus::new(0, 0))).is_ok());
    }

    #[test]
    fn injected_time_error_status_is_rejected() {
        assert!(ensure_synchronized_with(|| Ok(DisciplineStatus::new(5, 0))).is_err());
    }

    #[test]
    fn injected_unsynchronized_status_is_rejected() {
        assert!(ensure_synchronized_with(|| Ok(DisciplineStatus::new(0, 0x0040))).is_err());
    }

    #[test]
    fn injected_reader_error_is_propagated() {
        let error =
            ensure_synchronized_with(|| Err(io::Error::from(io::ErrorKind::PermissionDenied)))
                .expect_err("reader failure must prevent readiness");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
