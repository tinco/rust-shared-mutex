#![cfg_attr(test, deny(warnings))]
#![deny(missing_docs)]

//! # shared-mutex
//!
//! A RwLock that can be used with a Condvar.

use std::sync::{Mutex, Condvar, MutexGuard};
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::mem;

/// A lock providing both shared read locks and exclusive write locks.
///
/// Similar to `std::sync::RwLock`, except that its guards (`SharedMutexReadGuard` and
/// `SharedMutexWriteGuard`) can wait on `std::sync::Condvar`s, which is very
/// useful for implementing efficient concurrent programs.
///
/// Another difference from `std::sync::RwLock` is that the guard types are `Send`.
pub struct SharedMutex<T> {
    state: Mutex<State>,
    readers: Condvar,
    both: Condvar,
    data: UnsafeCell<T>
}

unsafe impl<T: Send> Send for SharedMutex<T> {}
unsafe impl<T: Sync> Sync for SharedMutex<T> {}

/// A shared read guard on a SharedMutex.
pub struct SharedMutexReadGuard<'mutex, T: 'mutex> {
    data: &'mutex T,
    mutex: &'mutex SharedMutex<T>
}

/// An exclusive write guard on a SharedMutex.
pub struct SharedMutexWriteGuard<'mutex, T: 'mutex> {
    data: &'mutex mut T,
    mutex: &'mutex SharedMutex<T>
}

impl<T> SharedMutex<T> {
    /// Create a new SharedMutex protecting the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        SharedMutex {
            state: Mutex::new(State::new()),
            readers: Condvar::new(),
            both: Condvar::new(),
            data: UnsafeCell::new(value)
        }
    }

    /// Acquire an exclusive Write lock on the data.
    #[inline]
    pub fn write(&self) -> SharedMutexWriteGuard<T> {
        let state_lock = self.state.lock().unwrap();
        unsafe { self.write_from(state_lock) }
    }

    /// Acquire a shared Read lock on the data.
    #[inline]
    pub fn read(&self) -> SharedMutexReadGuard<T> {
        let state_lock = self.state.lock().unwrap();
        unsafe { self.read_from(state_lock) }
    }

    /// Get a mutable reference to the data without locking.
    ///
    /// Safe since it requires exclusive access to the lock itself.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T { unsafe { &mut *self.data.get() } }

    /// Extract the data from the lock and destroy the lock.
    ///
    /// Safe since it requires ownership of the lock.
    #[inline]
    pub fn into_inner(self) -> T {
        unsafe { self.data.into_inner() }
    }

    /// Get a write lock using the given state lock.
    ///
    /// WARNING: The lock MUST be from self.state!!
    unsafe fn write_from(&self, mut state_lock: MutexGuard<State>) -> SharedMutexWriteGuard<T> {
        // First wait for any other writers to unlock.
        while state_lock.is_writer_active() {
            state_lock = self.both.wait(state_lock).unwrap();
        }

        // At this point there must be no writers, but there may be readers.
        //
        // We set the writer-active flag so that readers which try to
        // acquire the lock from here on out will be queued after us, to
        // prevent starvation.
        state_lock.set_writer_active();

        // Now wait for all readers to exit.
        //
        // This will happen eventually since new readers are waiting on
        // us because we set the writer-active flag.
        while state_lock.readers() != 0 {
            state_lock = self.readers.wait(state_lock).unwrap();
        }

        // At this point there should be one writer (us) and no readers.
        debug_assert!(state_lock.is_writer_active() && state_lock.readers() == 0,
                      "State not empty on write lock! State = {:?}", *state_lock);

        // Create the guard, then release the state lock.
        SharedMutexWriteGuard {
            data: &mut *self.data.get(),
            mutex: self
        }
    }

    /// Get a read lock using the given state lock.
    ///
    /// WARNING: The lock MUST be from self.state!!
    unsafe fn read_from(&self, mut state_lock: MutexGuard<State>) -> SharedMutexReadGuard<T> {
        // Wait for any writers to finish and for there to be space
        // for another reader. (There are a max of 2^63 readers at any time)
        while state_lock.is_writer_active() || state_lock.has_max_readers() {
            state_lock = self.both.wait(state_lock).unwrap();
        }

        // At this point there should be no writers and space
        // for at least one more reader.
        //
        // Add ourselves as a reader.
        state_lock.add_reader();

        // Create the guard, then release the state lock.
        SharedMutexReadGuard {
            data: &*self.data.get(),
            mutex: self
        }
    }

    #[inline]
    unsafe fn unlock_reader(&self) -> MutexGuard<State> {
        let mut state_lock = self.state.lock().unwrap();

        // First decrement the reader count.
        state_lock.remove_reader();

        // Now check if there is a writer waiting and
        // we are the last reader.
        if state_lock.is_writer_active() {
            if state_lock.readers() == 0 {
                // Wake up the waiting writer.
                self.readers.notify_one();
            }
        // Check if we where at the max number of readers.
        } else if state_lock.near_max_readers() {
            // Wake up a reader to replace us.
            self.both.notify_one()
        }

        // Return the lock for potential further use.
        state_lock
    }

    #[inline]
    unsafe fn unlock_writer(&self) -> MutexGuard<State> {
        let mut state_lock = self.state.lock().unwrap();

        // Writer locks are exclusive so we know we can just
        // set the state to empty.
        *state_lock = State::new();

        state_lock
    }
}

impl<'mutex, T> Deref for SharedMutexReadGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data }
}

impl<'mutex, T> Deref for SharedMutexWriteGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data }
}

impl<'mutex, T> DerefMut for SharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T { self.data }
}

impl<'mutex, T> SharedMutexReadGuard<'mutex, T> {
    /// Wait on the given condition variable, and resume with a write lock.
    ///
    /// See the documentation for `std::sync::Condvar::wait` for more information.
    pub fn wait_for_write(self, cond: &Condvar) -> SharedMutexWriteGuard<'mutex, T> {
        // Grab a reference for later.
        let shared = self.mutex;

        // Unlock and wait.
        let state_lock = cond.wait(self.unlock()).unwrap();

        // Re-acquire as a write lock.
        unsafe { shared.write_from(state_lock) }
    }

    /// Wait on the given condition variable, and resume with another read lock.
    ///
    /// See the documentation for `std::sync::Condvar::wait` for more information.
    pub fn wait_for_read(self, cond: &Condvar) -> Self {
        // Grab a reference for later.
        let shared = self.mutex;

        // Unlock and wait.
        let state_lock = cond.wait(self.unlock()).unwrap();

        // Re-acquire as a read lock.
        unsafe { shared.read_from(state_lock) }
    }

    fn unlock(self) -> MutexGuard<'mutex, State> {
        // Unlock the read lock.
        let state_lock = unsafe { self.mutex.unlock_reader() };

        // Don't double-unlock.
        mem::forget(self);
        state_lock
    }
}

impl<'mutex, T> SharedMutexWriteGuard<'mutex, T> {
    /// Wait on the given condition variable, and resume with another write lock.
    pub fn wait_for_write(self, cond: &Condvar) -> Self {
        // Grab a reference for later.
        let shared = self.mutex;

        // Unlock and wait.
        let state_lock = cond.wait(self.unlock()).unwrap();

        // Re-acquire as a write lock.
        unsafe { shared.write_from(state_lock) }
    }

    /// Wait on the given condition variable, and resume with a read lock.
    pub fn wait_for_read(self, cond: &Condvar) -> SharedMutexReadGuard<'mutex, T> {
        // Grab a reference for later.
        let shared = self.mutex;

        // Unlock and wait.
        let state_lock = cond.wait(self.unlock()).unwrap();

        // Re-acquire as a read lock.
        unsafe { shared.read_from(state_lock) }
    }

    fn unlock(self) -> MutexGuard<'mutex, State> {
        // Unlock the write lock.
        let state_lock = unsafe { self.mutex.unlock_writer() };

        // Don't double-unlock.
        mem::forget(self);
        state_lock
    }
}

impl<'mutex, T> Drop for SharedMutexReadGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { let _ = self.mutex.unlock_reader(); }
    }
}

impl<'mutex, T> Drop for SharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { let _ = self.mutex.unlock_writer(); }
    }
}

/// Internal State of the SharedMutex.
///
/// The high bit indicates if a writer is active.
///
/// The lower bits are used to count the number of readers.
#[derive(Debug)]
struct State(usize);

#[cfg(target_pointer_width = "64")]
const USIZE_BITS: u8 = 64;

#[cfg(target_pointer_width = "32")]
const USIZE_BITS: u8 = 32;

const WRITER_ACTIVE: usize = 1 << USIZE_BITS - 1;
const READERS_MASK: usize = !WRITER_ACTIVE;

impl State {
    #[inline]
    fn new() -> Self { State(0) }

    #[inline]
    fn is_writer_active(&self) -> bool { self.0 & WRITER_ACTIVE != 0 }

    #[inline]
    fn set_writer_active(&mut self) { self.0 |= WRITER_ACTIVE }

    #[inline]
    fn readers(&self) -> usize { self.0 & READERS_MASK }

    #[inline]
    fn has_max_readers(&self) -> bool { self.readers() == READERS_MASK }

    #[inline]
    fn near_max_readers(&self) -> bool { self.readers() == READERS_MASK - 1 }

    #[inline]
    fn add_reader(&mut self) { self.0 += 1 }

    #[inline]
    fn remove_reader(&mut self) { self.0 -= 1 }
}

#[cfg(test)]
mod test {
    use super::*;

    fn _check_bounds() {
        fn _is_send_sync<T: Send + Sync>() {}

        _is_send_sync::<SharedMutex<()>>();
        _is_send_sync::<SharedMutexReadGuard<()>>();
        _is_send_sync::<SharedMutexWriteGuard<()>>();
    }
}

