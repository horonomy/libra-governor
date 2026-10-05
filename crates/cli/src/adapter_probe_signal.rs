//! Signal policy belongs only to the explicit synchronous CLI probe. Stop
//! intent never exits the process while dispatch owns a child.

use std::sync::atomic::{AtomicBool, Ordering};

use libra_governor_daemon::host_runtime::dispatch::Cancellation;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static RESTORE_FAILED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static FAIL_INSTALL_TERM: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FAIL_RESTORE_TERM: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static PENDING_INSTALL_INT: AtomicBool = AtomicBool::new(false);

fn replace_action(
    signal: libc::c_int,
    action: &libc::sigaction,
    previous: *mut libc::sigaction,
    _restoring: bool,
) -> Result<(), ()> {
    #[cfg(test)]
    if signal == libc::SIGTERM
        && if _restoring {
            FAIL_RESTORE_TERM.swap(false, Ordering::SeqCst)
        } else {
            FAIL_INSTALL_TERM.swap(false, Ordering::SeqCst)
        }
    {
        return Err(());
    }
    // SAFETY: the guard owns initialized action/previous storage and calls
    // only in its signal-blocked installation or restoration window.
    if unsafe { libc::sigaction(signal, action, previous) } == 0 {
        Ok(())
    } else {
        Err(())
    }
}

extern "C" fn stop(_: libc::c_int) {
    // AtomicBool is lock-free on both supported host platforms.
    STOP.store(true, Ordering::SeqCst);
}

pub(super) struct ProbeSignals {
    previous: [libc::sigaction; 2],
    mask: libc::sigset_t,
    installed: usize,
    armed: bool,
    stopped: bool,
    thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

impl ProbeSignals {
    pub(super) fn install() -> Result<Self, ()> {
        if RESTORE_FAILED.load(Ordering::SeqCst)
            || ACTIVE
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(());
        }
        // SAFETY: initialized before any signal structure is passed to libc.
        let mut guard = Self {
            previous: unsafe { std::mem::zeroed() },
            mask: unsafe { std::mem::zeroed() },
            installed: 0,
            armed: true,
            stopped: false,
            thread_bound: std::marker::PhantomData,
        };
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: initialized masks and live saved-mask storage are used only
        // on the launching thread; no pointer is retained by the handler.
        unsafe {
            if libc::sigemptyset(&mut blocked) != 0
                || libc::sigaddset(&mut blocked, libc::SIGINT) != 0
                || libc::sigaddset(&mut blocked, libc::SIGTERM) != 0
                || libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut guard.mask) != 0
            {
                // No disposition changed. Do not restore an uninitialized mask.
                guard.armed = false;
                ACTIVE.store(false, Ordering::SeqCst);
                return Err(());
            }
            if SIGNALS
                .iter()
                .any(|signal| libc::sigismember(&guard.mask, *signal) != 0)
            {
                guard.restore()?;
                return Err(());
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = stop as *const () as usize;
            if libc::sigemptyset(&mut action.sa_mask) != 0
                || libc::sigaddset(&mut action.sa_mask, libc::SIGINT) != 0
                || libc::sigaddset(&mut action.sa_mask, libc::SIGTERM) != 0
            {
                guard.restore()?;
                return Err(());
            }
            // A fresh exclusive scope may clear prior stop intent only while
            // both target signals are blocked in its launching thread.
            STOP.store(false, Ordering::SeqCst);
            for (index, signal) in SIGNALS.iter().enumerate() {
                if replace_action(*signal, &action, &mut guard.previous[index], false).is_err() {
                    guard.restore()?;
                    return Err(());
                }
                guard.installed += 1;
            }
            #[cfg(test)]
            if PENDING_INSTALL_INT.swap(false, Ordering::SeqCst) {
                assert_eq!(libc::raise(libc::SIGINT), 0);
                let mut pending: libc::sigset_t = std::mem::zeroed();
                assert_eq!(libc::sigpending(&mut pending), 0);
                assert_eq!(libc::sigismember(&pending, libc::SIGINT), 1);
            }
            if libc::pthread_sigmask(libc::SIG_SETMASK, &guard.mask, std::ptr::null_mut()) != 0 {
                guard.restore()?;
                return Err(());
            }
        }
        Ok(guard)
    }

    pub(super) fn cancellation(&self) -> Cancellation {
        Cancellation::from_static_flag(&STOP)
    }

    /// Caller invokes this only after dispatcher returned verified cleanup.
    /// Pending signals are preserved and may follow the restored prior policy;
    /// a late signal can prevent output, but cannot abandon a live owned child.
    pub(super) fn restore(&mut self) -> Result<bool, ()> {
        let outcome = self.restore_policy();
        if outcome.is_err() {
            // Best-effort Drop may recover policy later; a normal restoration
            // failure still permanently closes new admission in this CLI.
            RESTORE_FAILED.store(true, Ordering::SeqCst);
        }
        outcome
    }

    fn restore_policy(&mut self) -> Result<bool, ()> {
        if !self.armed {
            return Ok(self.stopped);
        }
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: handler uses no borrowed storage; previous dispositions and
        // original launching-thread mask remain owned by this guard.
        unsafe {
            if libc::sigemptyset(&mut blocked) != 0
                || libc::sigaddset(&mut blocked, libc::SIGINT) != 0
                || libc::sigaddset(&mut blocked, libc::SIGTERM) != 0
                || libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()) != 0
            {
                return Err(());
            }
            self.stopped |= STOP.load(Ordering::SeqCst);
            // Retain the guard on partial failure for another best-effort
            // restoration; no second signal grants an emergency exit policy.
            for index in (0..self.installed).rev() {
                if replace_action(
                    SIGNALS[index],
                    &self.previous[index],
                    std::ptr::null_mut(),
                    true,
                )
                .is_err()
                {
                    return Err(());
                }
                self.installed = index;
            }
            if libc::pthread_sigmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut()) != 0 {
                return Err(());
            }
        }
        self.armed = false;
        ACTIVE.store(false, Ordering::SeqCst);
        Ok(self.stopped)
    }
}

impl Drop for ProbeSignals {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    static PRIOR_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn prior(_: libc::c_int) {
        PRIOR_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn scoped_signal_guard_controls_run_in_an_isolated_process() {
        let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "adapter_probe_signal::tests::isolated_probe_signal_guard_helper",
                "--ignored",
            ])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success(), "isolated probe signal controls failed");
                return;
            }
            if Instant::now() >= deadline {
                helper.kill().unwrap();
                helper.wait().unwrap();
                panic!("probe signal fixture exceeded deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "invoked explicitly by the isolated-process control"]
    fn isolated_probe_signal_guard_helper() {
        struct Original {
            actions: [libc::sigaction; 2],
            mask: libc::sigset_t,
        }
        impl Drop for Original {
            fn drop(&mut self) {
                unsafe {
                    for (signal, action) in SIGNALS.iter().zip(&self.actions) {
                        libc::sigaction(*signal, action, std::ptr::null_mut());
                    }
                    libc::pthread_sigmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut());
                }
            }
        }
        // Process-global disposition changes are confined to this helper.
        let mut original: Original = unsafe { std::mem::zeroed() };
        unsafe {
            for (index, signal) in SIGNALS.iter().enumerate() {
                assert_eq!(
                    libc::sigaction(*signal, std::ptr::null(), &mut original.actions[index]),
                    0
                );
            }
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut original.mask),
                0
            );
        }
        let mut initial_mask = original.mask;
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = prior as *const () as usize;
        unsafe {
            assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
            assert_eq!(libc::sigaddset(&mut initial_mask, libc::SIGUSR2), 0);
            for signal in SIGNALS {
                assert_eq!(libc::sigdelset(&mut initial_mask, signal), 0);
                assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
            }
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &initial_mask, std::ptr::null_mut()),
                0
            );
        }
        PENDING_INSTALL_INT.store(true, Ordering::SeqCst);
        let mut pending_guard = ProbeSignals::install().unwrap();
        assert!(STOP.load(Ordering::SeqCst));
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(pending_guard.restore(), Ok(true));
        let mut guard = ProbeSignals::install().unwrap();
        assert!(!STOP.load(Ordering::SeqCst));
        assert!(ProbeSignals::install().is_err());
        unsafe {
            assert_eq!(libc::raise(libc::SIGINT), 0);
            assert_eq!(libc::raise(libc::SIGTERM), 0);
        }
        assert!(STOP.load(Ordering::SeqCst));
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(guard.restore(), Ok(true));
        assert_eq!(guard.restore(), Ok(true));
        let mut observed_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut observed_mask),
                0
            );
            assert_eq!(libc::sigismember(&observed_mask, libc::SIGUSR2), 1);
            for signal in SIGNALS {
                assert_eq!(libc::sigismember(&observed_mask, signal), 0);
                let mut observed: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut observed), 0);
                assert_eq!(observed.sa_sigaction, prior as *const () as usize);
            }
            assert_eq!(libc::raise(libc::SIGINT), 0);
        }
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 1);
        // A stop handled immediately before restoration is captured even
        // after the dispatcher could have completed its own final check.
        let mut guard = ProbeSignals::install().unwrap();
        assert!(!STOP.load(Ordering::SeqCst));
        unsafe {
            assert_eq!(libc::raise(libc::SIGTERM), 0);
        }
        assert_eq!(guard.restore(), Ok(true));
        let mut guard = ProbeSignals::install().unwrap();
        assert_eq!(guard.restore(), Ok(false));
        // Failed second installation rolls back only the changed prefix.
        FAIL_INSTALL_TERM.store(true, Ordering::SeqCst);
        assert!(ProbeSignals::install().is_err());
        assert!(!ACTIVE.load(Ordering::SeqCst));
        unsafe {
            for signal in SIGNALS {
                let mut observed: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut observed), 0);
                assert_eq!(observed.sa_sigaction, prior as *const () as usize);
            }
        }
        // Preserve an initially blocked mask, refusing responsive-probe
        // claims rather than silently overriding the operator's policy.
        unsafe {
            assert_eq!(libc::sigaddset(&mut initial_mask, libc::SIGINT), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &initial_mask, std::ptr::null_mut()),
                0
            );
        }
        assert!(ProbeSignals::install().is_err());
        assert!(!ACTIVE.load(Ordering::SeqCst));
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut observed_mask),
                0
            );
            assert_eq!(libc::sigismember(&observed_mask, libc::SIGINT), 1);
            assert_eq!(libc::sigdelset(&mut initial_mask, libc::SIGINT), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &initial_mask, std::ptr::null_mut()),
                0
            );
        }
        let mut guard = ProbeSignals::install().unwrap();
        FAIL_RESTORE_TERM.store(true, Ordering::SeqCst);
        assert_eq!(guard.restore(), Err(()));
        assert!(ACTIVE.load(Ordering::SeqCst));
        assert!(ProbeSignals::install().is_err());
        drop(guard);
        assert!(!ACTIVE.load(Ordering::SeqCst));
        assert!(RESTORE_FAILED.load(Ordering::SeqCst));
        assert!(ProbeSignals::install().is_err());
        drop(original);
    }
}
