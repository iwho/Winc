//! Signal a running App Server on window teardown without waiting for pipe threads.

use std::io;
use std::process::Child;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
type Job = libc::pid_t;
#[cfg(windows)]
type Job = std::os::windows::io::OwnedHandle;
#[cfg(not(any(unix, windows)))]
type Job = ();

#[derive(Default)]
pub(super) struct Shutdown {
    stopping: AtomicBool,
    job: Mutex<Option<Job>>,
}

impl Shutdown {
    pub(super) fn register(&self, child: &Child) -> io::Result<()> {
        #[cfg(unix)]
        let job = -(child.id() as libc::pid_t);
        #[cfg(windows)]
        let job = windows::attach(child)?;
        #[cfg(not(any(unix, windows)))]
        let job = ();
        let mut active = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.stopping.load(Ordering::Acquire) {
            terminate(&job);
            return Err(io::ErrorKind::Interrupted.into());
        }
        *active = Some(job);
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let active = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = &*active {
            terminate(job);
        }
    }

    // Clearing the process identity before wait prevents another thread from
    // signalling a Unix PID after the system reuses it.
    pub(super) fn release(&self) {
        let mut active = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = active.take() {
            terminate(&job);
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn terminate(group: &Job) {
    // SAFETY: the child was placed in its own group before exec. The supervisor
    // cannot reap it until it removes the registered process identity.
    unsafe { libc::kill(*group, libc::SIGKILL) };
}

#[cfg(windows)]
use windows::terminate;

#[cfg(not(any(unix, windows)))]
fn terminate(_job: &Job) {}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub(super) fn attach(child: &Child) -> io::Result<OwnedHandle> {
        // SAFETY: null attributes and name create a private unnamed job.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle is newly created and owned by this function.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: all fields accept zero as an initial value.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle and structure remain valid for the duration of both calls.
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the child and job handles are open and owned by their Rust objects.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn terminate(job: &OwnedHandle) {
        // SAFETY: the owned handle stays valid for the duration of the call.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    }
}
