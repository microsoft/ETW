//! A mutex with a condition variable and deadlines, built only on `core` and Win32 APIs, so it
//! works without `std`. Shared by the raw consumer and the ProcessTrace thread.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::time::Duration;

use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    AcquireSRWLockExclusive, CONDITION_VARIABLE, CONDITION_VARIABLE_INIT, INFINITE,
    ReleaseSRWLockExclusive, SRWLOCK, SRWLOCK_INIT, SleepConditionVariableSRW,
    WakeAllConditionVariable,
};

// A point on the QueryPerformanceCounter clock. `None` never arrives.
#[derive(Clone, Copy)]
pub(crate) struct Deadline(Option<i64>);

impl Deadline {
    pub(crate) const NEVER: Deadline = Deadline(None);

    // A timeout too large to represent never expires.
    pub(crate) fn after(timeout: Option<Duration>) -> Self {
        Deadline(timeout.and_then(|timeout| {
            let ticks = timeout
                .as_nanos()
                .checked_mul(qpc_frequency() as u128)?
                .div_ceil(1_000_000_000);
            qpc_now().checked_add(i64::try_from(ticks).ok()?)
        }))
    }

    // Milliseconds left, rounded up so waits never end early; `None` once the deadline has passed.
    fn remaining_ms(self) -> Option<u32> {
        let Some(deadline) = self.0 else {
            return Some(INFINITE);
        };
        let left = deadline - qpc_now();
        if left <= 0 {
            return None;
        }
        let ms = (left as u128 * 1000).div_ceil(qpc_frequency() as u128);
        Some(ms.min(INFINITE as u128 - 1) as u32)
    }
}

// QueryPerformanceCounter and QueryPerformanceFrequency cannot fail on supported versions of Windows.
fn qpc_now() -> i64 {
    let mut count = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut count) };
    count
}

fn qpc_frequency() -> i64 {
    let mut frequency = 1;
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    frequency
}

// How many times `spin` doubles its wait: at most 2^7 - 1 = 127 spin hints in total, mirroring the
// spin phase of crossbeam's `Backoff`.
const SPIN_LIMIT: u32 = 6;

// Calls `attempt` until it returns `Some`, spinning with exponential backoff in between, and
// gives up with `None` after `SPIN_LIMIT` doublings. Handing an event to another thread usually
// takes well under a microsecond, while blocking costs a thread sleep and wake on each side.
#[cfg(any(feature = "crossbeam", feature = "flume", feature = "futures"))]
pub(crate) fn spin<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    for step in 0..=SPIN_LIMIT {
        if let Some(output) = attempt() {
            return Some(output);
        }
        for _ in 0..(1u32 << step) {
            core::hint::spin_loop();
        }
    }
    None
}

// A mutex with a condition variable, built on SRWLOCK and CONDITION_VARIABLE, which need no
// allocation or initialization. They must not move while in use, so `Lock` is only used
// behind an `Arc`.
pub(crate) struct Lock<T> {
    srw: UnsafeCell<SRWLOCK>,
    cv: UnsafeCell<CONDITION_VARIABLE>,
    data: UnsafeCell<T>,
}

// SAFETY: `data` is only accessed through a `Guard`, which holds the SRW lock exclusively.
unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub(crate) fn new(data: T) -> Self {
        Lock {
            srw: UnsafeCell::new(SRWLOCK_INIT),
            cv: UnsafeCell::new(CONDITION_VARIABLE_INIT),
            data: UnsafeCell::new(data),
        }
    }

    pub(crate) fn lock(&self) -> Guard<'_, T> {
        unsafe { AcquireSRWLockExclusive(self.srw.get()) };
        Guard {
            lock: self,
            _not_send: PhantomData,
        }
    }

    // Wakes every thread waiting in `Guard::wait_while`.
    pub(crate) fn notify_all(&self) {
        unsafe { WakeAllConditionVariable(self.cv.get()) };
    }
}

pub(crate) struct Guard<'a, T> {
    lock: &'a Lock<T>,
    // An SRW lock must be released by the thread that acquired it.
    _not_send: PhantomData<*const ()>,
}

impl<T> Guard<'_, T> {
    // Releases the lock and waits (for a notification, or until `deadline`) while `condition`
    // holds. Callers check the state afterwards to tell whether the deadline passed.
    pub(crate) fn wait_while(
        &mut self,
        deadline: Deadline,
        mut condition: impl FnMut(&mut T) -> bool,
    ) {
        // First spin briefly with the lock released, like `spin`, since sleeping on the
        // condition variable costs a wake on each side.
        let srw = self.lock.srw.get();
        for step in 0..=SPIN_LIMIT {
            if !condition(&mut **self) {
                return;
            }
            unsafe { ReleaseSRWLockExclusive(srw) };
            for _ in 0..(1u32 << step) {
                core::hint::spin_loop();
            }
            unsafe { AcquireSRWLockExclusive(srw) };
        }

        while condition(&mut **self) {
            let Some(ms) = deadline.remaining_ms() else {
                return;
            };
            // Fails with ERROR_TIMEOUT when `ms` elapses; the loop re-checks the deadline either way.
            let _ = unsafe {
                SleepConditionVariableSRW(self.lock.cv.get(), self.lock.srw.get(), ms, 0)
            };
        }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        unsafe { ReleaseSRWLockExclusive(self.lock.srw.get()) };
    }
}
