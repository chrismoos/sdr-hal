//! Process-wide scheduling must be configured before daemon threads are created.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Sets process-wide daemon scheduling before starting any daemon or runtime workers.
/// Library callers retain ordinary scheduling unless this function enables it.
pub fn configure(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    log::info!("radiod: realtime scheduling requested={enabled}");
}

/// Applies the control priority to the calling runtime or main thread.
pub fn apply_control() {
    apply(Role::Control);
}

pub(super) fn apply_stream() {
    apply(Role::Stream);
}

pub(super) fn apply_data() {
    apply(Role::Data);
}

#[derive(Clone, Copy)]
enum Role {
    Control,
    Data,
    Stream,
}

fn apply(role: Role) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("radiod-control");
    #[cfg(target_os = "linux")]
    linux(name, role);
    #[cfg(target_os = "macos")]
    macos::apply(name, role);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = role;
        log::warn!("radiod: thread={name} realtime scheduling is unsupported on this platform");
    }
}

#[cfg(target_os = "linux")]
fn linux(name: &str, role: Role) {
    const CONTROL_PRIORITY: i32 = 10;
    const DATA_PRIORITY: i32 = 30;
    const STREAM_PRIORITY: i32 = 40;
    let priority = match role {
        Role::Control => CONTROL_PRIORITY,
        Role::Data => DATA_PRIORITY,
        Role::Stream => STREAM_PRIORITY,
    };
    let requested = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: pthread_self identifies the calling thread and all output pointers refer to live stack values.
    unsafe {
        let thread = libc::pthread_self();
        let result = libc::pthread_setschedparam(thread, libc::SCHED_FIFO, &requested);
        if result != 0 {
            log::warn!(
                "radiod: thread={name} SCHED_FIFO priority={priority} refused: {}. Continuing with existing scheduling. Linux requires CAP_SYS_NICE or a sufficient RLIMIT_RTPRIO",
                std::io::Error::from_raw_os_error(result)
            );
            return;
        }
        let mut policy = 0;
        let mut actual = libc::sched_param { sched_priority: 0 };
        let result = libc::pthread_getschedparam(thread, &mut policy, &mut actual);
        if result == 0 && policy == libc::SCHED_FIFO && actual.sched_priority == priority {
            log::info!("radiod: thread={name} SCHED_FIFO priority={priority} verified");
        } else {
            log::warn!(
                "radiod: thread={name} scheduling verification failed result={result} policy={policy} priority={}",
                actual.sched_priority
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Role;

    const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
    const POLICY_COUNT: u32 = 4;
    const PERIOD_NS: u64 = 2_000_000;
    // Mach raises computation to at least half the constraint.
    const COMPUTATION_NS: u64 = CONSTRAINT_NS / 2;
    const CONSTRAINT_NS: u64 = 1_000_000;

    #[repr(C)]
    #[derive(Debug, Default, PartialEq)]
    struct Policy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_thread_self() -> u32;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn thread_policy_set(thread: u32, flavor: u32, policy: *const i32, count: u32) -> i32;
        fn thread_policy_get(
            thread: u32,
            flavor: u32,
            policy: *mut i32,
            count: *mut u32,
            default: *mut i32,
        ) -> i32;
        fn mach_port_deallocate(task: u32, port: u32) -> i32;
        static mach_task_self_: u32;
    }

    pub(super) fn apply(name: &str, role: Role) {
        // SAFETY: These pthread functions operate on the calling thread and receive valid QoS values.
        unsafe {
            let class = match role {
                Role::Control => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
                Role::Data | Role::Stream => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
            };
            let result = libc::pthread_set_qos_class_self_np(class, 0);
            let mut actual = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
            let read = libc::pthread_get_qos_class_np(
                libc::pthread_self(),
                &mut actual,
                std::ptr::null_mut(),
            );
            if result == 0 && read == 0 && actual as u32 == class as u32 {
                log::info!("radiod: thread={name} QoS={class:?} verified");
            } else {
                log::warn!(
                    "radiod: thread={name} QoS request failed result={result} actual={actual:?}"
                );
            }
        }
        if matches!(role, Role::Stream) {
            time_constraint(name);
        }
    }

    fn time_constraint(name: &str) {
        // SAFETY: Policy and timebase match the Mach C layouts. The acquired thread send right is released below.
        unsafe {
            let mut timebase = Timebase { numer: 0, denom: 0 };
            if mach_timebase_info(&mut timebase) != 0 || timebase.numer == 0 || timebase.denom == 0
            {
                log::warn!("radiod: thread={name} Mach timebase unavailable, retaining QoS");
                return;
            }
            let ticks =
                |ns: u64| (ns * u64::from(timebase.denom) / u64::from(timebase.numer)) as u32;
            let requested = Policy {
                period: ticks(PERIOD_NS),
                computation: ticks(COMPUTATION_NS),
                constraint: ticks(CONSTRAINT_NS),
                preemptible: 1,
            };
            let thread = mach_thread_self();
            let result = thread_policy_set(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                (&requested as *const Policy).cast(),
                POLICY_COUNT,
            );
            let mut actual = Policy::default();
            let mut count = POLICY_COUNT;
            let mut default = 0;
            let read = thread_policy_get(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                (&mut actual as *mut Policy).cast(),
                &mut count,
                &mut default,
            );
            let released = mach_port_deallocate(mach_task_self_, thread);
            if released != 0 {
                log::warn!(
                    "radiod: thread={name} Mach thread right release failed result={released}"
                );
            }
            if result == 0
                && read == 0
                && default == 0
                && count == POLICY_COUNT
                && actual == requested
            {
                log::info!(
                    "radiod: thread={name} Mach realtime verified period_us={} computation_us={} constraint_us={}",
                    PERIOD_NS / 1000,
                    COMPUTATION_NS / 1000,
                    CONSTRAINT_NS / 1000
                );
            } else {
                log::warn!(
                    "radiod: thread={name} Mach realtime request or verification failed set={result} get={read} default={default} count={count} requested_ticks={requested:?} actual_ticks={actual:?}"
                );
            }
        }
    }
}
