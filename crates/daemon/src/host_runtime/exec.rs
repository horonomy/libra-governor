//! Private one-request process ownership. This is not a sandbox or host authority.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{self, Pid, Signal, WaitId, WaitIdOptions, WaitOptions};

const MAX_OUTPUT: usize = 1_048_576;
const MAX_ERROR: usize = 16_384;
const CHUNK: usize = 65_536;
const FORK_PLAN_WAIT_BUDGET: Duration = Duration::from_secs(60);
const REQUEST_BUDGET: Duration = Duration::from_secs(4);
const CLEANUP_BUDGET: Duration = Duration::from_secs(1);
const TERM_GRACE: Duration = Duration::from_millis(150);
const SLICE: Duration = Duration::from_millis(5);

// Serialize this product's portable pipe+fcntl preparation through fork. No
// claim is made about foreign callers retaining non-CLOEXEC descriptors.
static FORK_PLAN: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(super) fn spawn_test_child(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    let _plan = FORK_PLAN.lock().unwrap();
    command.spawn()
}

#[derive(Clone, Default)]
pub struct Cancellation {
    local: Arc<AtomicBool>,
    external: Option<&'static AtomicBool>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.local.store(true, Ordering::SeqCst);
    }

    /// Observe caller-owned static stop intent without installing any library
    /// signal policy or granting execution authority.
    pub fn from_static_flag(flag: &'static AtomicBool) -> Self {
        Self {
            external: Some(flag),
            ..Self::default()
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        if self
            .external
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            self.local.store(true, Ordering::SeqCst);
        }
        self.local.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExecFailure {
    Cancelled,
    Timeout,
    Setup,
    SignalContext,
    Exec,
    Io,
    InputLimit,
    OutputLimit,
    ErrorLimit,
    NonzeroExit,
    OwnershipInterference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TimeoutPhase {
    NotOwned,
    OwnedAwaitingReady,
    GroupReadyAwaitingExecStatus,
    ExecStatusClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ExecDiagnosticFailure {
    pub kind: ExecFailure,
    pub timeout_phase: Option<TimeoutPhase>,
}

/// Only dispatch may construct a launch after matching current recorded trust.
pub(super) struct VerifiedLaunch {
    pub executable: CString,
    pub arguments: Vec<CString>,
    pub directory: File,
    pub max_request_bytes: usize,
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

fn pipe() -> Result<Pipe, ExecFailure> {
    let mut raw = [-1; 2];
    // SAFETY: writable storage holds the two descriptors returned by pipe.
    if unsafe { libc::pipe(raw.as_mut_ptr()) } != 0 {
        return Err(ExecFailure::Setup);
    }
    // SAFETY: successful pipe returned distinct, newly owned descriptors.
    let original = unsafe { [OwnedFd::from_raw_fd(raw[0]), OwnedFd::from_raw_fd(raw[1])] };
    let duplicate = |fd: &OwnedFd| {
        // Sources stay above stdio, including when the caller closed stdio.
        // SAFETY: fcntl duplicates a live borrowed descriptor.
        let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            Err(ExecFailure::Setup)
        } else {
            // SAFETY: fcntl returned a new owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    };
    Ok(Pipe {
        read: duplicate(&original[0])?,
        write: duplicate(&original[1])?,
    })
}

fn directory_source(directory: &File) -> Result<OwnedFd, ExecFailure> {
    // SAFETY: duplicate the borrowed directory above stdio before fork.
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        Err(ExecFailure::Setup)
    } else {
        // SAFETY: fcntl returned a newly owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn nonblocking(fd: &OwnedFd) -> Result<(), ExecFailure> {
    // SAFETY: fcntl operates on a live descriptor; no borrowed memory escapes.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(ExecFailure::Setup);
    }
    Ok(())
}

struct Plan {
    stdin: Pipe,
    stdout: Pipe,
    stderr: Pipe,
    status: Pipe,
}

impl Plan {
    fn new() -> Result<Self, ExecFailure> {
        let plan = Self {
            stdin: pipe()?,
            stdout: pipe()?,
            stderr: pipe()?,
            status: pipe()?,
        };
        for fd in [
            &plan.stdin.write,
            &plan.stdout.read,
            &plan.stderr.read,
            &plan.status.read,
        ] {
            nonblocking(fd)?;
        }
        Ok(plan)
    }
}

struct RawPlan {
    descriptors: [RawFd; 8],
    directory: RawFd,
    executable: *const libc::c_char,
    arguments: *const *const libc::c_char,
    environment: *const *const libc::c_char,
    default_signal: libc::sigaction,
    empty_mask: libc::sigset_t,
    signals: *const libc::c_int,
    signal_count: usize,
    #[cfg(test)]
    gate: Option<[RawFd; 4]>,
    #[cfg(test)]
    late_gate: bool,
}

struct ParentMask {
    saved: libc::sigset_t,
    armed: bool,
}

impl ParentMask {
    fn block() -> Result<Self, ExecFailure> {
        // SAFETY: masks are initialized by libc before use, in the parent.
        let mut all: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut saved: libc::sigset_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigfillset(&mut all) } != 0
            || unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut saved) } != 0
        {
            return Err(ExecFailure::SignalContext);
        }
        Ok(Self { saved, armed: true })
    }

    fn restore(&mut self) -> Result<(), ExecFailure> {
        if self.armed {
            // SAFETY: saved is this launching thread's initialized original mask.
            if unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.saved, std::ptr::null_mut())
            } != 0
            {
                return Err(ExecFailure::SignalContext);
            }
            self.armed = false;
        }
        Ok(())
    }
}

impl Drop for ParentMask {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn signal_plan() -> Result<Vec<libc::c_int>, ExecFailure> {
    #[cfg(target_os = "linux")]
    let maximum = libc::SIGRTMAX();
    // The Darwin SDK defines __DARWIN_NSIG=32, including signal zero.
    #[cfg(target_os = "macos")]
    let maximum = 31;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let maximum = 0;
    if !(1..=128).contains(&maximum) {
        return Err(ExecFailure::SignalContext);
    }
    let mut signals = Vec::with_capacity(maximum as usize);
    for signal in 1..=maximum {
        if matches!(signal, libc::SIGKILL | libc::SIGSTOP) {
            continue;
        }
        // SAFETY: query an initialized parent disposition without changing it.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                && !matches!(signal, libc::SIGPIPE | libc::SIGCHLD)
            {
                continue;
            }
            return Err(ExecFailure::SignalContext);
        }
        // Refuse known foreign reapers instead of altering caller policy.
        if signal == libc::SIGCHLD
            && (action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0)
        {
            return Err(ExecFailure::SignalContext);
        }
        // A closed adapter stdin must return EPIPE, never terminate the
        // caller or invoke foreign handler code before lexical cleanup.
        // Masked DFL is also refused: it can leave a fatal pending SIGPIPE.
        if signal == libc::SIGPIPE && action.sa_sigaction != libc::SIG_IGN {
            return Err(ExecFailure::SignalContext);
        }
        signals.push(signal);
    }
    Ok(signals)
}

// Every post-fork operation below is async-signal-safe. All strings, pointer
// arrays, descriptor plans and signal structures were built in the parent.
// No Rust allocation, formatting, locks, callbacks or destructors run here.
unsafe fn child(plan: &RawPlan) -> ! {
    let status = plan.descriptors[7];
    unsafe {
        #[cfg(test)]
        if let Some(gate) = plan.gate.filter(|_| !plan.late_gate) {
            // Fixture-only fixed syscalls, while every inherited signal is
            // blocked. No application callback executes in the child.
            libc::close(gate[0]);
            libc::close(gate[3]);
            let mut byte = 1_u8;
            if libc::write(gate[1], (&byte as *const u8).cast(), 1) != 1
                || libc::read(gate[2], (&mut byte as *mut u8).cast(), 1) != 1
            {
                libc::_exit(127);
            }
            libc::close(gate[1]);
            libc::close(gate[2]);
        }
        if libc::setpgid(0, 0) != 0 {
            let frame = 2_u8;
            libc::write(status, (&frame as *const u8).cast(), 1);
            libc::_exit(127);
        }
        let ready = 1_u8;
        if libc::write(status, (&ready as *const u8).cast(), 1) != 1 {
            libc::_exit(127);
        }
        if libc::dup2(plan.descriptors[0], 0) < 0
            || libc::dup2(plan.descriptors[3], 1) < 0
            || libc::dup2(plan.descriptors[5], 2) < 0
            || libc::fchdir(plan.directory) != 0
        {
            let frame = 2_u8;
            libc::write(status, (&frame as *const u8).cast(), 1);
            libc::_exit(127);
        }
        for index in 0..plan.signal_count {
            let signal = *plan.signals.add(index);
            if libc::sigaction(signal, &plan.default_signal, std::ptr::null_mut()) != 0 {
                let frame = 2_u8;
                libc::write(status, (&frame as *const u8).cast(), 1);
                libc::_exit(127);
            }
        }
        for fd in plan.descriptors {
            if fd != status {
                libc::close(fd);
            }
        }
        libc::close(plan.directory);
        #[cfg(test)]
        if let Some(gate) = plan.gate.filter(|_| plan.late_gate) {
            // Same fixed gate after setup/reset/FD closure, still blocked.
            libc::close(gate[0]);
            libc::close(gate[3]);
            let mut byte = 1_u8;
            if libc::write(gate[1], (&byte as *const u8).cast(), 1) != 1
                || libc::read(gate[2], (&mut byte as *mut u8).cast(), 1) != 1
            {
                libc::_exit(127);
            }
            libc::close(gate[1]);
            libc::close(gate[2]);
        }
        // Handlers have been reset while blocked; unblock at the exec boundary.
        if libc::sigprocmask(libc::SIG_SETMASK, &plan.empty_mask, std::ptr::null_mut()) != 0 {
            let frame = 2_u8;
            libc::write(status, (&frame as *const u8).cast(), 1);
            libc::_exit(127);
        }
        libc::execve(plan.executable, plan.arguments, plan.environment);
        let frame = 3_u8;
        libc::write(status, (&frame as *const u8).cast(), 1);
        libc::_exit(127);
    }
}

struct Attempt {
    pid: Option<Pid>,
    group: bool,
    group_absent: bool,
    reaped: bool,
    exited: Option<i32>,
    interference: bool,
    input: Option<OwnedFd>,
    output: Option<OwnedFd>,
    error: Option<OwnedFd>,
    status: Option<OwnedFd>,
    output_bytes: Vec<u8>,
    error_bytes: usize,
    status_bytes: [u8; 3],
    status_length: usize,
    ready: bool,
    failure: Option<ExecFailure>,
    timeout_phase: Option<TimeoutPhase>,
    term_at: Option<Instant>,
    group_final_signal: bool,
    leader_final_signal: bool,
    #[cfg(test)]
    leader_kill_outcomes: std::collections::VecDeque<SignalOutcome>,
    #[cfg(test)]
    leader_kill_attempts: usize,
    #[cfg(test)]
    kill_outcomes: std::collections::VecDeque<SignalOutcome>,
    #[cfg(test)]
    kill_attempts: usize,
}

#[derive(Clone, Copy)]
enum SignalOutcome {
    Delivered,
    Absent,
    Interrupted,
    Uncertain,
}

impl Attempt {
    fn prepared() -> Self {
        Self {
            pid: None,
            group: false,
            group_absent: false,
            reaped: false,
            exited: None,
            interference: false,
            input: None,
            output: None,
            error: None,
            status: None,
            output_bytes: Vec::with_capacity(MAX_OUTPUT),
            error_bytes: 0,
            status_bytes: [0; 3],
            status_length: 0,
            ready: false,
            failure: None,
            timeout_phase: None,
            term_at: None,
            group_final_signal: false,
            leader_final_signal: false,
            #[cfg(test)]
            leader_kill_outcomes: std::collections::VecDeque::new(),
            #[cfg(test)]
            leader_kill_attempts: 0,
            #[cfg(test)]
            kill_outcomes: std::collections::VecDeque::new(),
            #[cfg(test)]
            kill_attempts: 0,
        }
    }

    fn timeout(&mut self) -> ExecFailure {
        if self.failure.is_none() && self.timeout_phase.is_none() {
            self.timeout_phase = Some(if self.pid.is_none() {
                TimeoutPhase::NotOwned
            } else if !self.ready {
                TimeoutPhase::OwnedAwaitingReady
            } else if self.status.is_some() {
                TimeoutPhase::GroupReadyAwaitingExecStatus
            } else {
                TimeoutPhase::ExecStatusClosed
            });
        }
        ExecFailure::Timeout
    }

    fn fail(&mut self, failure: ExecFailure) {
        if failure == ExecFailure::Timeout {
            self.timeout();
        }
        self.failure.get_or_insert(failure);
    }

    fn observe_exit(&mut self) {
        let Some(pid) = self.pid else { return };
        if self.reaped || self.exited.is_some() || self.interference {
            return;
        }
        match process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(status)) => {
                self.exited = Some(status.exit_status().unwrap_or(-1));
            }
            Ok(None) | Err(Errno::INTR) => {}
            Err(Errno::CHILD) => {
                self.interference = true;
                self.fail(ExecFailure::OwnershipInterference);
            }
            Err(_) => self.fail(ExecFailure::Io),
        }
    }

    fn read_status(&mut self) {
        let Some(fd) = self.status.as_ref() else {
            return;
        };
        let mut bytes = [0_u8; 3];
        match rustix::io::read(fd, &mut bytes) {
            Ok(0) => {
                self.status = None;
                if !self.ready {
                    self.fail(ExecFailure::Setup);
                }
            }
            Ok(count) => {
                for byte in &bytes[..count] {
                    if self.status_length == self.status_bytes.len() {
                        self.fail(ExecFailure::Setup);
                        break;
                    }
                    self.status_bytes[self.status_length] = *byte;
                    self.status_length += 1;
                    match (*byte, self.status_length) {
                        (1, 1) => {
                            self.ready = true;
                            self.group = true;
                        }
                        (2, 1 | 2) => self.fail(ExecFailure::Setup),
                        (3, 2) => self.fail(ExecFailure::Exec),
                        _ => self.fail(ExecFailure::Setup),
                    }
                }
            }
            Err(Errno::AGAIN | Errno::INTR) => {}
            Err(_) => self.fail(ExecFailure::Io),
        }
    }

    fn drain(&mut self, stderr: bool, discard: bool) {
        let fd = if stderr { &self.error } else { &self.output };
        let Some(fd) = fd.as_ref() else { return };
        let mut bytes = [0_u8; CHUNK];
        match rustix::io::read(fd, &mut bytes) {
            Ok(0) => {
                if stderr {
                    self.error = None
                } else {
                    self.output = None
                }
            }
            Ok(count) if !discard => {
                if stderr {
                    self.error_bytes = self.error_bytes.saturating_add(count);
                    if self.error_bytes > MAX_ERROR {
                        self.fail(ExecFailure::ErrorLimit);
                    }
                } else if count > MAX_OUTPUT.saturating_sub(self.output_bytes.len()) {
                    self.fail(ExecFailure::OutputLimit);
                } else {
                    self.output_bytes.extend_from_slice(&bytes[..count]);
                }
            }
            Ok(_) | Err(Errno::AGAIN | Errno::INTR) => {}
            Err(_) => self.fail(ExecFailure::Io),
        }
    }

    fn signal_original_group(&mut self, signal: Signal) -> SignalOutcome {
        let Some(pid) = self.pid else {
            return SignalOutcome::Absent;
        };
        if self.reaped || self.interference {
            return SignalOutcome::Uncertain;
        }
        if !self.group && process::getpgid(Some(pid)) == Ok(pid) {
            self.group = true;
        }
        if !self.group || self.group_absent {
            return SignalOutcome::Absent;
        }
        #[cfg(test)]
        if signal == Signal::KILL {
            self.kill_attempts += 1;
            if let Some(outcome) = self.kill_outcomes.pop_front() {
                return outcome;
            }
        }
        let result = process::kill_process_group(pid, signal);
        self.classify_signal_result(result, true)
    }

    fn signal_owned_leader(&mut self, signal: Signal) -> SignalOutcome {
        let Some(pid) = self.pid else {
            return SignalOutcome::Absent;
        };
        if self.reaped || self.interference {
            return SignalOutcome::Uncertain;
        }
        #[cfg(test)]
        if signal == Signal::KILL {
            self.leader_kill_attempts += 1;
            if let Some(outcome) = self.leader_kill_outcomes.pop_front() {
                return outcome;
            }
        }
        // The original fork PID remains pinned by NOWAIT. Never adopt or
        // signal a foreign group merely because this leader moved into it.
        let result = process::kill_process(pid, signal);
        self.classify_signal_result(result, false)
    }

    fn classify_signal_result(
        &mut self,
        result: Result<(), Errno>,
        group_target: bool,
    ) -> SignalOutcome {
        match result {
            Ok(()) => SignalOutcome::Delivered,
            Err(Errno::SRCH) => {
                if group_target {
                    self.group_absent = true;
                }
                SignalOutcome::Absent
            }
            Err(Errno::INTR) => SignalOutcome::Interrupted,
            Err(Errno::PERM) => SignalOutcome::Uncertain,
            Err(_) => {
                self.fail(ExecFailure::Io);
                SignalOutcome::Uncertain
            }
        }
    }

    fn final_attempt(&self, outcome: SignalOutcome) -> bool {
        match outcome {
            SignalOutcome::Delivered | SignalOutcome::Absent => true,
            SignalOutcome::Interrupted => false,
            // An exited leader permits final reap plus a fresh absence probe;
            // a denied live target remains owned and retryable.
            SignalOutcome::Uncertain => self.exited.is_some() && !self.interference,
        }
    }

    fn cleanup_step(&mut self) -> bool {
        let Some(pid) = self.pid else { return true };
        self.input = None;
        self.read_status();
        self.observe_exit();
        let now = Instant::now();
        if self.term_at.is_none() {
            self.signal_original_group(Signal::TERM);
            self.signal_owned_leader(Signal::TERM);
            self.term_at = Some(now);
        }
        if self.term_at.is_some_and(|at| now - at >= TERM_GRACE) {
            if !self.group_final_signal {
                let outcome = self.signal_original_group(Signal::KILL);
                // No group proof is not proof of completed group signaling.
                // Re-read the setup frame before reaping the held leader.
                if self.group {
                    self.group_final_signal = self.final_attempt(outcome);
                }
            }
            if !self.leader_final_signal {
                let outcome = self.signal_owned_leader(Signal::KILL);
                self.leader_final_signal = self.final_attempt(outcome);
            }
        }
        self.drain(false, true);
        self.drain(true, true);
        self.observe_exit();
        self.read_status();
        if !self.group && process::getpgid(Some(pid)) == Ok(pid) {
            self.group = true;
        }
        if (!self.group || self.group_final_signal)
            && self.leader_final_signal
            && self.exited.is_some()
            && self.status.is_none()
            && !self.reaped
            && !self.interference
        {
            match process::waitpid(Some(pid), WaitOptions::NOHANG) {
                Ok(Some((observed, _))) if observed == pid => self.reaped = true,
                Ok(None) | Err(Errno::INTR) => {}
                _ => {
                    self.interference = true;
                    self.fail(ExecFailure::OwnershipInterference);
                }
            }
        }
        // No destructive signal is ever sent after reaping. PERM stays
        // uncertain; Darwin's zombie-only PERM requires a fresh post-reap SRCH.
        if self.reaped && process::test_kill_process_group(pid) == Err(Errno::SRCH) {
            self.pid = None;
            return true;
        }
        false
    }

    fn finish(&mut self) {
        while self.pid.is_some() {
            let deadline = Instant::now() + CLEANUP_BUDGET;
            while Instant::now() < deadline {
                if self.cleanup_step() {
                    return;
                }
                std::thread::sleep(SLICE);
            }
            // Uncertain ownership is nonterminal. No child or request can be
            // admitted during these bounded observations, even after cancel.
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        // Emergency unwinding only. Ordinary execution closes explicitly;
        // this nonblocking best effort never reports verified cleanup.
        self.signal_original_group(Signal::KILL);
        self.signal_owned_leader(Signal::KILL);
        if let Some(pid) = self.pid {
            if !self.reaped && !self.interference {
                let _ = process::waitpid(Some(pid), WaitOptions::NOHANG);
            }
        }
    }
}

fn poll(attempt: &Attempt, write_input: bool, deadline: Instant) -> Result<(), ExecFailure> {
    let descriptor = |fd: &Option<OwnedFd>, events| libc::pollfd {
        fd: fd.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        events,
        revents: 0,
    };
    let mut fds = [
        descriptor(&attempt.input, if write_input { libc::POLLOUT } else { 0 }),
        descriptor(&attempt.output, libc::POLLIN),
        descriptor(&attempt.error, libc::POLLIN),
        descriptor(&attempt.status, libc::POLLIN),
    ];
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .min(SLICE)
        .as_millis();
    // SAFETY: the array remains live and writable for poll's bounded call.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis as i32) } < 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
    {
        return Err(ExecFailure::Io);
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn run(
    launch: VerifiedLaunch,
    request: &[u8],
    cancellation: &Cancellation,
) -> Result<Vec<u8>, ExecFailure> {
    run_diagnostic(launch, request, cancellation).map_err(|error| error.kind)
}

pub(super) fn run_diagnostic(
    launch: VerifiedLaunch,
    request: &[u8],
    cancellation: &Cancellation,
) -> Result<Vec<u8>, ExecDiagnosticFailure> {
    run_owned_diagnostic(launch, request, cancellation, RunControls::default())
}

#[cfg(test)]
type BeforeCleanup = Box<dyn FnOnce(&mut Attempt)>;
#[cfg(test)]
type AfterCleanup = Box<dyn FnOnce(&Attempt)>;

#[derive(Default)]
struct RunControls {
    #[cfg(test)]
    expire_before_launch: bool,
    #[cfg(test)]
    before_cleanup: Option<BeforeCleanup>,
    #[cfg(test)]
    after_cleanup: Option<AfterCleanup>,
    #[cfg(test)]
    gate: Option<ChildGate>,
    #[cfg(test)]
    on_owned: Option<Box<dyn FnOnce(Pid)>>,
    #[cfg(test)]
    hold_gate: bool,
}

#[cfg(test)]
struct ChildGate {
    ready: Pipe,
    resume: Pipe,
    late: bool,
}

#[cfg(test)]
impl ChildGate {
    fn new() -> Self {
        let _lock = FORK_PLAN.lock().unwrap();
        Self {
            ready: pipe().unwrap(),
            resume: pipe().unwrap(),
            late: false,
        }
    }
}

fn completion_failure(
    cancellation: &Cancellation,
    deadline: Instant,
    observed: Instant,
) -> Option<ExecFailure> {
    if cancellation.cancelled() {
        Some(ExecFailure::Cancelled)
    } else if observed >= deadline {
        Some(ExecFailure::Timeout)
    } else {
        None
    }
}

#[cfg(test)]
fn run_owned(
    launch: VerifiedLaunch,
    request: &[u8],
    cancellation: &Cancellation,
    controls: RunControls,
) -> Result<Vec<u8>, ExecFailure> {
    run_owned_diagnostic(launch, request, cancellation, controls).map_err(|error| error.kind)
}

fn run_owned_diagnostic(
    launch: VerifiedLaunch,
    request: &[u8],
    cancellation: &Cancellation,
    _controls: RunControls,
) -> Result<Vec<u8>, ExecDiagnosticFailure> {
    if request.len() > launch.max_request_bytes.min(MAX_OUTPUT) {
        return Err(ExecDiagnosticFailure {
            kind: ExecFailure::InputLimit,
            timeout_phase: None,
        });
    }
    let mut owner = Attempt::prepared();
    let preparation_deadline = Instant::now() + FORK_PLAN_WAIT_BUDGET;
    #[cfg(test)]
    let mut held_gate = None;
    // Owner lives outside the unwind boundary; cleanup precedes any resumed
    // panic, so dropping an idle public dispatcher cannot abandon a child.
    let execution = catch_unwind(AssertUnwindSafe(|| {
        let environment = [
            CString::new("LANG=C.UTF-8").unwrap(),
            CString::new("LC_ALL=C.UTF-8").unwrap(),
        ];
        let mut argv: Vec<_> = std::iter::once(launch.executable.as_ptr())
            .chain(launch.arguments.iter().map(|argument| argument.as_ptr()))
            .collect();
        argv.push(std::ptr::null());
        let env = [
            environment[0].as_ptr(),
            environment[1].as_ptr(),
            std::ptr::null(),
        ];
        let lock = loop {
            if cancellation.cancelled() {
                return Err(ExecFailure::Cancelled);
            }
            if Instant::now() >= preparation_deadline {
                return Err(owner.timeout());
            }
            match FORK_PLAN.try_lock() {
                Ok(lock) => break lock,
                Err(TryLockError::Poisoned(_)) => return Err(ExecFailure::Setup),
                Err(TryLockError::WouldBlock) => std::thread::sleep(SLICE),
            }
        };
        let directory = directory_source(&launch.directory)?;
        let plan = Plan::new()?;
        let signals = signal_plan()?;
        // Waiting for our own launch mutex must not consume the adapter's
        // request budget. Under parallel callers that made a healthy, ready
        // adapter time out before it was ever admitted to run.
        let deadline = Instant::now() + REQUEST_BUDGET;
        #[cfg(test)]
        if _controls.expire_before_launch {
            std::thread::sleep(REQUEST_BUDGET);
        }
        // SAFETY: all-zero signal structures are initialized by sigemptyset
        // before the child can read them; disposition is explicitly SIG_DFL.
        let mut default_signal: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut empty_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        default_signal.sa_sigaction = libc::SIG_DFL;
        if unsafe { libc::sigemptyset(&mut default_signal.sa_mask) } != 0
            || unsafe { libc::sigemptyset(&mut empty_mask) } != 0
        {
            return Err(ExecFailure::Setup);
        }
        let raw = RawPlan {
            descriptors: [
                plan.stdin.read.as_raw_fd(),
                plan.stdin.write.as_raw_fd(),
                plan.stdout.read.as_raw_fd(),
                plan.stdout.write.as_raw_fd(),
                plan.stderr.read.as_raw_fd(),
                plan.stderr.write.as_raw_fd(),
                plan.status.read.as_raw_fd(),
                plan.status.write.as_raw_fd(),
            ],
            directory: directory.as_raw_fd(),
            executable: launch.executable.as_ptr(),
            arguments: argv.as_ptr(),
            environment: env.as_ptr(),
            default_signal,
            empty_mask,
            signals: signals.as_ptr(),
            signal_count: signals.len(),
            #[cfg(test)]
            gate: _controls.gate.as_ref().map(|gate| {
                [
                    gate.ready.read.as_raw_fd(),
                    gate.ready.write.as_raw_fd(),
                    gate.resume.read.as_raw_fd(),
                    gate.resume.write.as_raw_fd(),
                ]
            }),
            #[cfg(test)]
            late_gate: _controls.gate.as_ref().is_some_and(|gate| gate.late),
        };
        if cancellation.cancelled() {
            return Err(ExecFailure::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(owner.timeout());
        }
        let mut mask = ParentMask::block()?;
        // SAFETY: child takes only the audited async-signal-safe path above.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(ExecFailure::Setup);
        }
        if pid == 0 {
            unsafe { child(&raw) };
        }
        // Publish ownership before any fallible allocation or callback.
        owner.pid = Pid::from_raw(pid);
        owner.group = process::setpgid(owner.pid, owner.pid).is_ok();
        let Plan {
            stdin,
            stdout,
            stderr,
            status,
        } = plan;
        owner.input = Some(stdin.write);
        owner.output = Some(stdout.read);
        owner.error = Some(stderr.read);
        owner.status = Some(status.read);
        drop((stdin.read, stdout.write, stderr.write, status.write));
        mask.restore()?;
        drop(lock);
        #[cfg(test)]
        if let Some(gate) = _controls.gate {
            drop((gate.ready.write, gate.resume.read));
            nonblocking(&gate.ready.read)?;
            let mut byte = [0_u8; 1];
            loop {
                if cancellation.cancelled() || Instant::now() >= deadline {
                    return Err(if cancellation.cancelled() {
                        ExecFailure::Cancelled
                    } else {
                        owner.timeout()
                    });
                }
                match rustix::io::read(&gate.ready.read, &mut byte) {
                    Ok(1) => break,
                    Err(Errno::AGAIN | Errno::INTR) => std::thread::sleep(SLICE),
                    _ => return Err(ExecFailure::Setup),
                }
            }
            if let Some(observe) = _controls.on_owned {
                observe(owner.pid.unwrap());
            }
            if _controls.hold_gate {
                // Keep the child gated through the production I/O loop and
                // cleanup. The real deadline/cancellation paths decide.
                held_gate = Some(gate.resume.write);
            } else {
                if cancellation.cancelled() {
                    return Err(ExecFailure::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(owner.timeout());
                }
                rustix::io::write(&gate.resume.write, &[1_u8]).map_err(|_| ExecFailure::Setup)?;
            }
        } else if let Some(observe) = _controls.on_owned {
            observe(owner.pid.unwrap());
        }
        let mut delivered = 0;
        loop {
            if cancellation.cancelled() {
                owner.fail(ExecFailure::Cancelled);
            }
            if Instant::now() >= deadline {
                owner.fail(ExecFailure::Timeout);
            }
            owner.read_status();
            owner.observe_exit();
            if owner.exited.is_some_and(|code| code != 0) && owner.status.is_none() {
                owner.fail(ExecFailure::NonzeroExit);
            }
            if owner.failure.is_some() {
                break;
            }
            // CLOEXEC EOF plus the ready frame means child setup has passed.
            // Recheck cancellation/deadline immediately before every write.
            if owner.ready
                && owner.status.is_none()
                && !cancellation.cancelled()
                && Instant::now() < deadline
            {
                if let Some(fd) = owner.input.as_ref() {
                    if delivered == request.len() {
                        owner.input = None;
                    } else {
                        let end = request.len().min(delivered + CHUNK);
                        match rustix::io::write(fd, &request[delivered..end]) {
                            Ok(0) => owner.fail(ExecFailure::Io),
                            Ok(count) => delivered += count,
                            Err(Errno::AGAIN | Errno::INTR) => {}
                            Err(_) => owner.fail(ExecFailure::Io),
                        }
                    }
                }
            }
            owner.drain(false, false);
            owner.drain(true, false);
            let complete = owner.exited == Some(0)
                && owner.input.is_none()
                && owner.output.is_none()
                && owner.error.is_none()
                && owner.status.is_none();
            if complete {
                if let Some(failure) = completion_failure(cancellation, deadline, Instant::now()) {
                    owner.fail(failure);
                }
            }
            if owner.failure.is_some() || complete {
                break;
            }
            poll(&owner, owner.ready && owner.status.is_none(), deadline)?;
        }
        #[cfg(test)]
        if let Some(observe) = _controls.before_cleanup {
            observe(&mut owner);
        }
        Ok(())
    }));
    owner.finish();
    #[cfg(test)]
    drop(held_gate);
    #[cfg(test)]
    if let Some(observe) = _controls.after_cleanup {
        observe(&owner);
    }
    if cancellation.cancelled() {
        owner.fail(ExecFailure::Cancelled);
    }
    match execution {
        Err(panic) => resume_unwind(panic),
        Ok(Err(failure)) => Err(ExecDiagnosticFailure {
            kind: failure,
            timeout_phase: if failure == ExecFailure::Timeout {
                owner.timeout_phase
            } else {
                None
            },
        }),
        Ok(Ok(())) => match owner.failure {
            Some(failure) => Err(ExecDiagnosticFailure {
                kind: failure,
                timeout_phase: if failure == ExecFailure::Timeout {
                    owner.timeout_phase
                } else {
                    None
                },
            }),
            None => Ok(std::mem::take(&mut owner.output_bytes)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These private primitive controls do not bypass the future public
    // registration/trust acceptance tests or grant any host capability.
    fn launch(executable: &str, arguments: &[&str], directory: &std::path::Path) -> VerifiedLaunch {
        VerifiedLaunch {
            executable: CString::new(executable).unwrap(),
            arguments: arguments
                .iter()
                .map(|argument| CString::new(*argument).unwrap())
                .collect(),
            directory: File::open(directory).unwrap(),
            max_request_bytes: MAX_OUTPUT,
        }
    }

    #[test]
    fn exact_input_pressure_is_drained_without_a_write_then_read_deadlock() {
        let directory = tempfile::tempdir().unwrap();
        let request = vec![b'x'; MAX_OUTPUT / 2];
        let result = run_diagnostic(
            launch("/bin/cat", &[], directory.path()),
            &request,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result, request);
    }

    #[test]
    fn executed_environment_contains_only_the_fixed_locale() {
        let directory = tempfile::tempdir().unwrap();
        let result = run(
            launch("/usr/bin/env", &[], directory.path()),
            b"",
            &Cancellation::default(),
        )
        .unwrap();
        let mut lines: Vec<_> = std::str::from_utf8(&result).unwrap().lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["LANG=C.UTF-8", "LC_ALL=C.UTF-8"]);
    }

    #[test]
    fn retained_directory_and_registry_descriptors_do_not_survive_exec() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let registry_path = directory.path().join("registry.json");
        std::fs::write(&registry_path, b"private-registry-marker").unwrap();
        let registry = File::open(&registry_path).unwrap();
        let registry_metadata = registry.metadata().unwrap();
        let directory_metadata = directory.path().metadata().unwrap();
        let identities = format!(
            "{}:{}:{}:{}",
            directory_metadata.dev(),
            directory_metadata.ino(),
            registry_metadata.dev(),
            registry_metadata.ino()
        );
        let output = run(
            launch(
                "/usr/bin/python3",
                &[
                    "-c",
                    r#"
import os, sys
values = list(map(int, sys.argv[1].split(':')))
private = {(values[0], values[1]), (values[2], values[3])}
for fd in range(3, 1024):
    try:
        status = os.fstat(fd)
    except OSError:
        continue
    if (status.st_dev, status.st_ino) in private:
        raise SystemExit(7)
print('no-private-descriptors')
"#,
                    &identities,
                ],
                directory.path(),
            ),
            b"",
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(output, b"no-private-descriptors\n");
        assert_eq!(registry.metadata().unwrap().ino(), registry_metadata.ino());
    }

    #[test]
    fn missing_exec_is_refused_and_cleaned_up() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            run(
                launch("/no-such-libra-fixture", &[], directory.path()),
                b"",
                &Cancellation::default()
            ),
            Err(ExecFailure::Exec)
        );
    }

    #[test]
    fn nonzero_exit_never_becomes_a_successful_response() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            run(
                launch("/bin/sh", &["-c", "exit 7"], directory.path()),
                b"",
                &Cancellation::default()
            ),
            Err(ExecFailure::NonzeroExit)
        );
    }

    #[test]
    fn stdout_and_stderr_floods_have_separate_bounded_failures() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            run(
                launch("/usr/bin/yes", &["payload"], directory.path()),
                b"",
                &Cancellation::default()
            ),
            Err(ExecFailure::OutputLimit)
        );
        assert_eq!(
            run(
                launch(
                    "/bin/sh",
                    &["-c", "exec /usr/bin/yes payload >&2"],
                    directory.path()
                ),
                b"",
                &Cancellation::default()
            ),
            Err(ExecFailure::ErrorLimit)
        );
    }

    #[test]
    fn pre_cancel_and_oversized_request_refuse_before_exec() {
        let directory = tempfile::tempdir().unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert_eq!(
            run(
                launch("/no-such-libra-fixture", &[], directory.path()),
                b"",
                &cancellation
            ),
            Err(ExecFailure::Cancelled)
        );
        assert_eq!(
            run(
                launch("/no-such-libra-fixture", &[], directory.path()),
                &vec![b'x'; MAX_OUTPUT + 1],
                &Cancellation::default()
            ),
            Err(ExecFailure::InputLimit)
        );
    }

    const ISOLATED_WATCHDOG: Duration = Duration::from_secs(30);

    #[test]
    fn diagnostic_timeout_freezes_parent_observation_before_owned_cleanup() {
        // Held pre-exec gates retain inherited pipe ends until exit. Keep
        // these deliberate stalls outside other test-owned runner processes.
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "host_runtime::exec::tests::isolated_timeout_phase_helper",
            "--ignored",
        ]);
        let mut helper = spawn_test_child(&mut command).unwrap();
        let deadline = Instant::now() + ISOLATED_WATCHDOG;
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success(), "isolated timeout phase controls failed");
                return;
            }
            if Instant::now() >= deadline {
                helper.kill().unwrap();
                helper.wait().unwrap();
                panic!("timeout phase fixture exceeded its bounded deadline");
            }
            std::thread::sleep(SLICE);
        }
    }

    #[test]
    #[ignore = "invoked by diagnostic timeout controls in an isolated process"]
    fn isolated_timeout_phase_helper() {
        for late in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut gate = ChildGate::new();
            gate.late = late;
            let phase = if late {
                TimeoutPhase::GroupReadyAwaitingExecStatus
            } else {
                TimeoutPhase::OwnedAwaitingReady
            };
            let observed = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let published = observed.clone();
            let result = run_owned_diagnostic(
                launch(
                    "/bin/sh",
                    &["-c", "printf unexpected > exec-marker"],
                    directory.path(),
                ),
                b"withheld",
                &Cancellation::default(),
                RunControls {
                    gate: Some(gate),
                    hold_gate: true,
                    on_owned: Some(Box::new(move |pid| {
                        published.store(pid.as_raw_pid(), Ordering::SeqCst);
                    })),
                    before_cleanup: Some(Box::new(move |owner| {
                        assert_eq!(owner.timeout_phase, Some(phase));
                    })),
                    after_cleanup: Some(Box::new(move |owner| {
                        assert!(owner.reaped);
                        assert!(owner.pid.is_none());
                        assert_eq!(owner.timeout_phase, Some(phase));
                    })),
                    ..RunControls::default()
                },
            );
            assert_eq!(
                result,
                Err(ExecDiagnosticFailure {
                    kind: ExecFailure::Timeout,
                    timeout_phase: Some(phase)
                })
            );
            assert!(!directory.path().join("exec-marker").exists());
            verify_absent(Pid::from_raw(observed.load(Ordering::SeqCst)).unwrap());
        }
        let directory = tempfile::tempdir().unwrap();
        for (expire, phase) in [
            (true, TimeoutPhase::NotOwned),
            (false, TimeoutPhase::ExecStatusClosed),
        ] {
            let observed = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let published = observed.clone();
            let result = run_owned_diagnostic(
                launch(
                    "/bin/sh",
                    &["-c", "cat >/dev/null; sleep 6"],
                    directory.path(),
                ),
                b"{}",
                &Cancellation::default(),
                RunControls {
                    expire_before_launch: expire,
                    on_owned: Some(Box::new(move |pid| {
                        published.store(pid.as_raw_pid(), Ordering::SeqCst);
                    })),
                    after_cleanup: Some(Box::new(move |owner| {
                        assert!(owner.pid.is_none());
                        assert_eq!(owner.timeout_phase, Some(phase));
                        if !expire {
                            assert!(owner.reaped);
                        }
                    })),
                    ..RunControls::default()
                },
            );
            assert_eq!(
                result,
                Err(ExecDiagnosticFailure {
                    kind: ExecFailure::Timeout,
                    timeout_phase: Some(phase)
                })
            );
            if expire {
                assert_eq!(observed.load(Ordering::SeqCst), 0);
            } else {
                verify_absent(Pid::from_raw(observed.load(Ordering::SeqCst)).unwrap());
            }
        }
    }

    #[test]
    fn timeout_observation_never_replaces_first_failure_or_later_progress() {
        let mut owner = Attempt::prepared();
        owner.fail(ExecFailure::NonzeroExit);
        owner.fail(ExecFailure::Timeout);
        assert_eq!(owner.failure, Some(ExecFailure::NonzeroExit));
        assert_eq!(owner.timeout_phase, None);
        let mut owner = Attempt::prepared();
        owner.fail(ExecFailure::Timeout);
        owner.ready = true;
        owner.fail(ExecFailure::Cancelled);
        owner.finish();
        assert_eq!(owner.failure, Some(ExecFailure::Timeout));
        assert_eq!(owner.timeout_phase, Some(TimeoutPhase::NotOwned));
    }

    #[test]
    fn completion_checks_the_exact_deadline_and_cancellation_priority() {
        let deadline = Instant::now();
        let cancellation = Cancellation::default();
        assert_eq!(
            completion_failure(&cancellation, deadline, deadline - SLICE),
            None
        );
        assert_eq!(
            completion_failure(&cancellation, deadline, deadline),
            Some(ExecFailure::Timeout)
        );
        assert_eq!(
            completion_failure(&cancellation, deadline, deadline + SLICE),
            Some(ExecFailure::Timeout)
        );
        cancellation.cancel();
        assert_eq!(
            completion_failure(&cancellation, deadline, deadline),
            Some(ExecFailure::Cancelled)
        );
    }

    #[test]
    fn observed_external_stop_intent_remains_sticky_across_clones_and_flag_reset() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let cancellation = Cancellation::from_static_flag(&FLAG);
        let clone = cancellation.clone();
        assert!(!cancellation.cancelled());
        FLAG.store(true, Ordering::SeqCst);
        assert!(clone.cancelled());
        FLAG.store(false, Ordering::SeqCst);
        assert!(cancellation.cancelled());
        assert!(clone.cancelled());
    }

    fn verify_absent(pid: Pid) {
        assert!(matches!(
            process::waitpid(Some(pid), WaitOptions::NOHANG),
            Err(Errno::CHILD)
        ));
        assert_eq!(process::test_kill_process_group(pid), Err(Errno::SRCH));
    }

    #[test]
    fn cancellation_at_cleanup_entry_discards_completed_output_after_reap() {
        let directory = tempfile::tempdir().unwrap();
        let cancellation = Cancellation::default();
        let cancel = cancellation.clone();
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        let result = run_owned(
            launch("/bin/cat", &[], directory.path()),
            b"candidate",
            &cancellation,
            RunControls {
                before_cleanup: Some(Box::new(move |owner| {
                    observed.store(owner.pid.unwrap().as_raw_pid(), Ordering::SeqCst);
                    assert_eq!(
                        owner.failure, None,
                        "completed-output fixture must succeed before cancellation"
                    );
                    assert_eq!(owner.exited, Some(0));
                    assert_eq!(owner.output_bytes, b"candidate");
                    cancel.cancel();
                })),
                after_cleanup: Some(Box::new(|owner| {
                    assert!(owner.reaped);
                    assert!(owner.pid.is_none());
                })),
                ..RunControls::default()
            },
        );
        assert_eq!(result, Err(ExecFailure::Cancelled));
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
    }

    #[test]
    fn panic_before_cleanup_resumes_only_after_owned_child_is_reaped() {
        let directory = tempfile::tempdir().unwrap();
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_owned(
                launch("/bin/cat", &[], directory.path()),
                b"candidate",
                &Cancellation::default(),
                RunControls {
                    before_cleanup: Some(Box::new(move |owner| {
                        observed.store(owner.pid.unwrap().as_raw_pid(), Ordering::SeqCst);
                        panic!("fixture-owned unwind");
                    })),
                    after_cleanup: Some(Box::new(|owner| {
                        assert!(owner.reaped);
                        assert!(owner.pid.is_none());
                    })),
                    ..RunControls::default()
                },
            )
        }));
        assert!(result.is_err());
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
    }

    #[test]
    fn interrupted_kill_retries_against_the_same_term_resistant_owned_child() {
        let directory = tempfile::tempdir().unwrap();
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        let result = run_owned(
            launch(
                "/bin/sh",
                &["-c", "trap '' TERM; exec /bin/sleep 30"],
                directory.path(),
            ),
            b"",
            &Cancellation::default(),
            RunControls {
                before_cleanup: Some(Box::new(move |owner| {
                    observed.store(owner.pid.unwrap().as_raw_pid(), Ordering::SeqCst);
                    assert!(owner.exited.is_none());
                    assert_eq!(owner.failure, Some(ExecFailure::Timeout));
                    // Finite uncertainty retains the live owner; the fixture
                    // then releases the seam to the real termination path.
                    owner.term_at = Some(Instant::now() - TERM_GRACE);
                    for _ in 0..10 {
                        owner.kill_outcomes.push_back(SignalOutcome::Uncertain);
                        owner
                            .leader_kill_outcomes
                            .push_back(SignalOutcome::Uncertain);
                        assert!(!owner.cleanup_step());
                        assert!(owner.pid.is_some());
                        assert!(!owner.reaped);
                        assert!(!owner.group_final_signal);
                    }
                    owner.kill_outcomes.push_back(SignalOutcome::Interrupted);
                    owner
                        .leader_kill_outcomes
                        .push_back(SignalOutcome::Interrupted);
                })),
                after_cleanup: Some(Box::new(|owner| {
                    assert!(owner.kill_attempts >= 12);
                    assert!(owner.leader_kill_attempts >= 12);
                    assert!(owner.reaped);
                    assert!(owner.pid.is_none());
                })),
                ..RunControls::default()
            },
        );
        assert_eq!(result, Err(ExecFailure::Timeout));
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
    }

    #[test]
    fn confirmed_group_absence_suppresses_all_later_destructive_signals() {
        let mut owner = Attempt::prepared();
        owner.pid = Pid::from_raw(i32::MAX);
        owner.group = true;
        assert!(matches!(
            owner.classify_signal_result(Err(Errno::SRCH), owner.group),
            SignalOutcome::Absent
        ));
        assert!(owner.group_absent);
        assert!(matches!(
            owner.signal_original_group(Signal::TERM),
            SignalOutcome::Absent
        ));
        assert!(matches!(
            owner.signal_original_group(Signal::KILL),
            SignalOutcome::Absent
        ));
        assert_eq!(owner.kill_attempts, 0);
        // No real process was created; do not synthesize an owned Drop target.
        owner.pid = None;
    }

    #[test]
    fn direct_pid_absence_and_permission_denial_do_not_establish_group_absence() {
        let mut owner = Attempt::prepared();
        assert!(matches!(
            owner.classify_signal_result(Err(Errno::SRCH), owner.group),
            SignalOutcome::Absent
        ));
        assert!(!owner.group_absent);
        owner.group = true;
        assert!(matches!(
            owner.classify_signal_result(Err(Errno::SRCH), false),
            SignalOutcome::Absent
        ));
        assert!(!owner.group_absent);
        assert!(matches!(
            owner.classify_signal_result(Err(Errno::PERM), true),
            SignalOutcome::Uncertain
        ));
        assert!(!owner.group_absent);
    }

    #[test]
    fn reaped_or_foreign_reaper_state_never_sends_another_signal() {
        for interference in [false, true] {
            let mut owner = Attempt::prepared();
            owner.pid = Pid::from_raw(i32::MAX);
            owner.group = true;
            owner.reaped = !interference;
            owner.interference = interference;
            assert!(matches!(
                owner.signal_original_group(Signal::KILL),
                SignalOutcome::Uncertain
            ));
            assert!(matches!(
                owner.signal_owned_leader(Signal::KILL),
                SignalOutcome::Uncertain
            ));
            assert_eq!(owner.kill_attempts, 0);
            assert_eq!(owner.leader_kill_attempts, 0);
            owner.pid = None;
        }
    }

    #[test]
    fn cancellation_while_pre_exec_is_gated_delivers_no_request_and_reaps() {
        let directory = tempfile::tempdir().unwrap();
        let cancellation = Cancellation::default();
        let cancel = cancellation.clone();
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        let result = run_owned(
            launch("/bin/sh", &["-c", "cat > request-marker"], directory.path()),
            b"withheld",
            &cancellation,
            RunControls {
                gate: Some(ChildGate::new()),
                on_owned: Some(Box::new(move |child| {
                    observed.store(child.as_raw_pid(), Ordering::SeqCst);
                    cancel.cancel();
                })),
                after_cleanup: Some(Box::new(|owner| {
                    assert!(owner.pid.is_none());
                    assert!(owner.reaped);
                    assert!(owner.output_bytes.is_empty());
                })),
                ..RunControls::default()
            },
        );
        assert_eq!(result, Err(ExecFailure::Cancelled));
        assert!(!directory.path().join("request-marker").exists());
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
    }

    #[test]
    fn pre_exec_deadline_and_late_setup_cancellation_withhold_execution() {
        // A deliberately stalled pre-exec child retains inherited CLOEXEC
        // pipe ends until exec/exit. Isolate it from other test-owned runners.
        let launched = {
            // Cooperate with portable pipe preparation; retain no raw pipe
            // originals in this longer-lived helper across exec.
            let _plan = FORK_PLAN.lock().unwrap();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host_runtime::exec::tests::isolated_pre_exec_deadline_helper",
                    "--ignored",
                ])
                .spawn()
        };
        let mut helper = launched.unwrap();
        let deadline = Instant::now() + ISOLATED_WATCHDOG;
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "isolated pre-exec deadline control failed"
                );
                return;
            }
            if Instant::now() >= deadline {
                helper.kill().unwrap();
                helper.wait().unwrap();
                panic!("pre-exec fixture exceeded its bounded deadline");
            }
            std::thread::sleep(SLICE);
        }
    }

    #[test]
    #[ignore = "invoked explicitly by the isolated-process control"]
    fn isolated_pre_exec_deadline_helper() {
        for (late, timed_out) in [(false, true), (true, true), (true, false)] {
            let directory = tempfile::tempdir().unwrap();
            let cancellation = Cancellation::default();
            let cancel = cancellation.clone();
            let mut gate = ChildGate::new();
            gate.late = late;
            let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let observed = pid.clone();
            let result = run_owned(
                launch(
                    "/bin/sh",
                    &["-c", "printf started > exec-marker; cat > request-marker"],
                    directory.path(),
                ),
                b"withheld",
                &cancellation,
                RunControls {
                    expire_before_launch: false,
                    gate: Some(gate),
                    hold_gate: true,
                    on_owned: Some(Box::new(move |child| {
                        observed.store(child.as_raw_pid(), Ordering::SeqCst);
                        if timed_out {
                            std::thread::sleep(REQUEST_BUDGET);
                        } else {
                            cancel.cancel();
                        }
                    })),
                    after_cleanup: Some(Box::new(|owner| {
                        assert!(owner.reaped);
                        assert!(owner.pid.is_none());
                    })),
                    before_cleanup: Some(Box::new(move |owner| {
                        assert_eq!(
                            owner.failure,
                            Some(if timed_out {
                                ExecFailure::Timeout
                            } else {
                                ExecFailure::Cancelled
                            })
                        );
                    })),
                },
            );
            assert_eq!(
                result,
                Err(if timed_out {
                    ExecFailure::Timeout
                } else {
                    ExecFailure::Cancelled
                })
            );
            assert!(!directory.path().join("exec-marker").exists());
            assert!(!directory.path().join("request-marker").exists());
            verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
        }
    }

    static HANDLER_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    #[test]
    fn leader_group_drift_controls_run_in_an_isolated_process() {
        let launched = {
            // Cooperate with portable pipe preparation; retain no raw pipe
            // originals in this longer-lived helper across exec.
            let _plan = FORK_PLAN.lock().unwrap();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host_runtime::exec::tests::isolated_leader_group_drift_helper",
                    "--ignored",
                ])
                .spawn()
        };
        let mut helper = launched.unwrap();
        let deadline = Instant::now() + ISOLATED_WATCHDOG;
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success(), "isolated group drift control failed");
                return;
            }
            if Instant::now() >= deadline {
                helper.kill().unwrap();
                helper.wait().unwrap();
                panic!("group drift fixture exceeded its bounded deadline");
            }
            std::thread::sleep(SLICE);
        }
    }

    #[test]
    #[ignore = "invoked explicitly in its test-owned destination group"]
    fn isolated_leader_group_drift_helper() {
        let destination = Pid::from_raw(std::process::id() as i32).unwrap();
        process::setpgid(None, Some(destination)).unwrap();
        for descendant in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let script = directory.path().join("group_drift.py");
            let marker = directory.path().join("moved");
            std::fs::write(
                &script,
                r#"
import os, signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
if sys.argv[3] == "descendant":
    child = os.fork()
    if child == 0:
        time.sleep(5)
        os._exit(0)
os.setpgid(0, int(sys.argv[1]))
with open(sys.argv[2], "w") as stream:
    stream.write(str(os.getpgrp()))
time.sleep(5)
"#,
            )
            .unwrap();
            let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let observed = pid.clone();
            let result = run_owned(
                launch(
                    "/usr/bin/python3",
                    &[
                        script.to_str().unwrap(),
                        &destination.as_raw_pid().to_string(),
                        marker.to_str().unwrap(),
                        if descendant {
                            "descendant"
                        } else {
                            "leader_only"
                        },
                    ],
                    directory.path(),
                ),
                b"",
                &Cancellation::default(),
                RunControls {
                    on_owned: Some(Box::new(move |child| {
                        observed.store(child.as_raw_pid(), Ordering::SeqCst);
                    })),
                    after_cleanup: Some(Box::new(move |owner| {
                        assert!(owner.pid.is_none());
                        assert!(owner.reaped);
                        assert!(owner.leader_kill_attempts >= 1);
                        if descendant {
                            assert!(owner.kill_attempts >= 1);
                        } else {
                            assert!(owner.group_absent);
                            assert_eq!(owner.kill_attempts, 0);
                        }
                    })),
                    ..RunControls::default()
                },
            );
            assert_eq!(result, Err(ExecFailure::Timeout));
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap(),
                destination.as_raw_pid().to_string()
            );
            verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
            // The foreign destination is this isolated helper's own group;
            // surviving this assertion proves cleanup did not signal it.
            assert_eq!(process::getpgid(None), Ok(destination));
            assert_eq!(process::test_kill_process_group(destination), Ok(()));
        }
    }

    extern "C" fn caught_signal(_: libc::c_int) {
        let fd = HANDLER_FD.load(Ordering::SeqCst);
        let byte = 1_u8;
        // Fixture handler uses only a lock-free atomic load and write.
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }

    #[test]
    fn signal_policy_controls_run_in_an_isolated_test_process() {
        let launched = {
            // Cooperate with portable pipe preparation; retain no raw pipe
            // originals in this longer-lived helper across exec.
            let _plan = FORK_PLAN.lock().unwrap();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host_runtime::exec::tests::isolated_signal_policy_helper",
                    "--ignored",
                ])
                .spawn()
        };
        let mut helper = launched.unwrap();
        let deadline = Instant::now() + ISOLATED_WATCHDOG;
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success(), "isolated signal controls failed");
                break;
            }
            if Instant::now() >= deadline {
                helper.kill().unwrap();
                helper.wait().unwrap();
                panic!("isolated signal controls exceeded their fixture deadline");
            }
            std::thread::sleep(SLICE);
        }
    }

    #[test]
    #[ignore = "invoked explicitly by the isolated-process control"]
    fn isolated_signal_policy_helper() {
        struct RestoreSignals {
            usr1: libc::sigaction,
            child: libc::sigaction,
            pipe: libc::sigaction,
            mask: libc::sigset_t,
        }
        impl Drop for RestoreSignals {
            fn drop(&mut self) {
                unsafe {
                    libc::sigaction(libc::SIGUSR1, &self.usr1, std::ptr::null_mut());
                    libc::sigaction(libc::SIGCHLD, &self.child, std::ptr::null_mut());
                    libc::sigaction(libc::SIGPIPE, &self.pipe, std::ptr::null_mut());
                    libc::pthread_sigmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut());
                }
            }
        }
        // Only this isolated process changes global signal dispositions.
        let mut restore: RestoreSignals = unsafe { std::mem::zeroed() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, std::ptr::null(), &mut restore.usr1),
                0
            );
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut restore.child),
                0
            );
            assert_eq!(
                libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut restore.pipe),
                0
            );
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut restore.mask),
                0
            );
        }
        let marker = pipe().unwrap();
        nonblocking(&marker.read).unwrap();
        HANDLER_FD.store(marker.write.as_raw_fd(), Ordering::SeqCst);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = caught_signal as *const () as usize;
        unsafe {
            assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                0
            );
        }
        let mut mask = restore.mask;
        unsafe {
            assert_eq!(libc::sigaddset(&mut mask, libc::SIGUSR2), 0);
            assert_eq!(libc::sigdelset(&mut mask, libc::SIGUSR1), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()),
                0
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        let result = run_owned(
            launch(
                "/bin/sh",
                &["-c", "cat > signal-request-marker"],
                directory.path(),
            ),
            b"",
            &Cancellation::default(),
            RunControls {
                gate: Some(ChildGate::new()),
                on_owned: Some(Box::new(move |child| {
                    observed.store(child.as_raw_pid(), Ordering::SeqCst);
                    // Child is gated with signals blocked, before any resets.
                    assert_eq!(process::kill_process(child, Signal::USR1), Ok(()));
                })),
                ..RunControls::default()
            },
        );
        assert_eq!(result, Err(ExecFailure::NonzeroExit));
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
        assert!(!directory.path().join("signal-request-marker").exists());
        let mut byte = [0_u8; 1];
        assert_eq!(rustix::io::read(&marker.read, &mut byte), Err(Errno::AGAIN));
        // The parent handler and original launching-thread mask survived.
        let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut current),
                0
            );
            assert_eq!(libc::sigismember(&current, libc::SIGUSR2), 1);
            assert_eq!(libc::sigismember(&current, libc::SIGUSR1), 0);
            assert_eq!(libc::raise(libc::SIGUSR1), 0);
        }
        assert_eq!(rustix::io::read(&marker.read, &mut byte), Ok(1));
        assert_eq!(byte, [1]);
        // Known auto-reapers and caught reapers must refuse before any fork.
        for (disposition, flags) in [
            (libc::SIG_IGN, 0),
            (libc::SIG_DFL, libc::SA_NOCLDWAIT),
            (caught_signal as *const () as usize, 0),
        ] {
            action.sa_sigaction = disposition;
            action.sa_flags = flags;
            unsafe {
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                    0
                );
            }
            let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = starts.clone();
            let result = run_owned(
                launch("/bin/cat", &[], directory.path()),
                b"",
                &Cancellation::default(),
                RunControls {
                    on_owned: Some(Box::new(move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })),
                    ..RunControls::default()
                },
            );
            assert_eq!(result, Err(ExecFailure::SignalContext));
            assert_eq!(starts.load(Ordering::SeqCst), 0);
        }
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &restore.child, std::ptr::null_mut()),
                0
            );
        }
        for (disposition, blocked) in [
            (libc::SIG_DFL, false),
            (libc::SIG_DFL, true),
            (caught_signal as *const () as usize, false),
        ] {
            action.sa_sigaction = disposition;
            action.sa_flags = 0;
            let mut pipe_mask = restore.mask;
            unsafe {
                assert_eq!(
                    libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()),
                    0
                );
                assert_eq!(
                    if blocked {
                        libc::sigaddset(&mut pipe_mask, libc::SIGPIPE)
                    } else {
                        libc::sigdelset(&mut pipe_mask, libc::SIGPIPE)
                    },
                    0
                );
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_SETMASK, &pipe_mask, std::ptr::null_mut()),
                    0
                );
            }
            let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = starts.clone();
            assert_eq!(
                run_owned(
                    launch("/bin/sh", &["-c", "exec 0<&-; sleep 1"], directory.path()),
                    &[b'x'; 65536],
                    &Cancellation::default(),
                    RunControls {
                        on_owned: Some(Box::new(move |_| {
                            observed.fetch_add(1, Ordering::SeqCst);
                        })),
                        ..RunControls::default()
                    }
                ),
                Err(ExecFailure::SignalContext)
            );
            assert_eq!(starts.load(Ordering::SeqCst), 0);
            unsafe {
                let mut observed_action: libc::sigaction = std::mem::zeroed();
                let mut observed_mask: libc::sigset_t = std::mem::zeroed();
                assert_eq!(
                    libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut observed_action),
                    0
                );
                assert_eq!(observed_action.sa_sigaction, disposition);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut observed_mask),
                    0
                );
                assert_eq!(
                    libc::sigismember(&observed_mask, libc::SIGPIPE),
                    i32::from(blocked)
                );
            }
        }
        action.sa_sigaction = libc::SIG_IGN;
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()),
                0
            );
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &restore.mask, std::ptr::null_mut()),
                0
            );
        }
        let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let observed = pid.clone();
        assert_eq!(
            run_owned(
                launch("/bin/sh", &["-c", "exec 0<&-; sleep 1"], directory.path()),
                &vec![b'x'; MAX_OUTPUT / 2],
                &Cancellation::default(),
                RunControls {
                    on_owned: Some(Box::new(move |child| {
                        observed.store(child.as_raw_pid(), Ordering::SeqCst);
                    })),
                    ..RunControls::default()
                }
            ),
            Err(ExecFailure::Io)
        );
        verify_absent(Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap());
        drop(restore);
        HANDLER_FD.store(-1, Ordering::SeqCst);
    }
}
