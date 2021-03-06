#![cfg_attr(test, deny(warnings))]
#![deny(missing_docs)]

//! # shared-mutex
//!
//! A RwLock that can be used with a Condvar.

#[cfg(test)]
extern crate scoped_pool;

extern crate poison;

use std::sync::{Condvar, LockResult, TryLockResult, TryLockError};
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::{mem, ptr, fmt};

use poison::{Poison, PoisonGuard, RawPoisonGuard};

pub use raw::RawSharedMutex;

pub mod monitor;
mod raw;

/// A lock providing both shared read locks and exclusive write locks.
///
/// Similar to `std::sync::RwLock`, except that its guards (`SharedMutexReadGuard` and
/// `SharedMutexWriteGuard`) can wait on `std::sync::Condvar`s, which is very
/// useful for implementing efficient concurrent programs.
///
/// Another difference from `std::sync::RwLock` is that the guard types are `Send`.
pub struct SharedMutex<T: ?Sized> {
    raw: RawSharedMutex,
    data: UnsafeCell<Poison<T>>
}

unsafe impl<T: ?Sized + Send> Send for SharedMutex<T> {}
unsafe impl<T: ?Sized + Sync> Sync for SharedMutex<T> {}

impl<T> SharedMutex<T> {
    /// Create a new SharedMutex protecting the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        SharedMutex {
            raw: RawSharedMutex::new(),
            data: UnsafeCell::new(Poison::new(value))
        }
    }

    /// Extract the data from the lock and destroy the lock.
    ///
    /// Safe since it requires ownership of the lock.
    #[inline]
    pub fn into_inner(self) -> LockResult<T> {
        unsafe { self.data.into_inner().into_inner() }
    }
}

impl<T: ?Sized> SharedMutex<T> {
    /// Acquire an exclusive Write lock on the data.
    #[inline]
    pub fn write(&self) -> LockResult<SharedMutexWriteGuard<T>> {
        self.raw.write();
        unsafe { SharedMutexWriteGuard::new(self) }
    }

    /// Acquire a shared Read lock on the data.
    #[inline]
    pub fn read(&self) -> LockResult<SharedMutexReadGuard<T>> {
        self.raw.read();
        unsafe { SharedMutexReadGuard::new(self) }
    }

    /// Attempt to acquire a shared Read lock on the data.
    ///
    /// If acquiring the lock would block, returns `TryLockError::WouldBlock`.
    #[inline]
    pub fn try_read(&self) -> TryLockResult<SharedMutexReadGuard<T>> {
        if self.raw.try_read() {
            Ok(try!(unsafe { SharedMutexReadGuard::new(self) }))
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

    /// Attempt to acquire an exclusive Write lock on the data.
    ///
    /// If acquiring the lock would block, returns `TryLockError::WouldBlock`.
    #[inline]
    pub fn try_write(&self) -> TryLockResult<SharedMutexWriteGuard<T>> {
        if self.raw.try_write() {
            Ok(try!(unsafe { SharedMutexWriteGuard::new(self) }))
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

    /// Get a mutable reference to the data without locking.
    ///
    /// Safe since it requires exclusive access to the lock itself.
    #[inline]
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        poison::map_result(unsafe { &mut *self.data.get() }.lock(),
                           |poison| unsafe { poison.into_mut() })
    }
}

/// A shared read guard on a SharedMutex.
pub struct SharedMutexReadGuard<'mutex, T: ?Sized + 'mutex> {
    data: &'mutex T,
    mutex: &'mutex SharedMutex<T>
}

unsafe impl<'mutex, T: ?Sized + Send> Send for SharedMutexReadGuard<'mutex, T> {}
unsafe impl<'mutex, T: ?Sized + Sync> Sync for SharedMutexReadGuard<'mutex, T> {}

/// An exclusive write guard on a SharedMutex.
pub struct SharedMutexWriteGuard<'mutex, T: ?Sized + 'mutex> {
    data: PoisonGuard<'mutex, T>,
    mutex: &'mutex SharedMutex<T>
}

impl<'mutex, T: ?Sized> Deref for SharedMutexReadGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data }
}

impl<'mutex, T: ?Sized> Deref for SharedMutexWriteGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data.get() }
}

impl<'mutex, T: ?Sized> DerefMut for SharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T { self.data.get_mut() }
}

impl<'mutex, T: ?Sized> SharedMutexReadGuard<'mutex, T> {
    #[inline]
    unsafe fn new(mutex: &'mutex SharedMutex<T>) -> LockResult<Self> {
        poison::map_result((&*mutex.data.get()).get(), |data| {
            SharedMutexReadGuard {
                data: data,
                mutex: mutex
            }
        })
    }
}

impl<'mutex, T: ?Sized> SharedMutexWriteGuard<'mutex, T> {
    #[inline]
    unsafe fn new(mutex: &'mutex SharedMutex<T>) -> LockResult<Self> {
        poison::map_result((&mut *mutex.data.get()).lock(), |poison| {
            SharedMutexWriteGuard {
                data: poison,
                mutex: mutex
            }
        })
    }
}

impl<'mutex, T: ?Sized> SharedMutexReadGuard<'mutex, T> {
    /// Turn this guard into a guard which can be mapped to a sub-borrow.
    ///
    /// Note that a mapped guard cannot wait on a `Condvar`.
    pub fn into_mapped(self) -> MappedSharedMutexReadGuard<'mutex, T> {
        let guard = MappedSharedMutexReadGuard {
            mutex: &self.mutex.raw,
            data: self.data
        };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }

    /// Wait on the given condition variable, and resume with a write lock.
    ///
    /// See the documentation for `std::sync::Condvar::wait` for more information.
    pub fn wait_for_write(self, cond: &Condvar) -> LockResult<SharedMutexWriteGuard<'mutex, T>> {
        self.mutex.raw.wait_from_read_to_write(cond);

        let guard = unsafe { SharedMutexWriteGuard::new(self.mutex) };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }

    /// Wait on the given condition variable, and resume with another read lock.
    ///
    /// See the documentation for `std::sync::Condvar::wait` for more information.
    pub fn wait_for_read(self, cond: &Condvar) -> LockResult<Self> {
        self.mutex.raw.wait_from_read_to_read(cond);

        let guard = unsafe { SharedMutexReadGuard::new(self.mutex) };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }
}

impl<'mutex, T: ?Sized> SharedMutexWriteGuard<'mutex, T> {
    /// Turn this guard into a guard which can be mapped to a sub-borrow.
    ///
    /// Note that a mapped guard cannot wait on a `Condvar`.
    pub fn into_mapped(self) -> MappedSharedMutexWriteGuard<'mutex, T> {
        let guard = MappedSharedMutexWriteGuard {
            mutex: &self.mutex.raw,
            poison: unsafe { ptr::read(&self.data).into_raw() },
            data: unsafe { (&mut *self.mutex.data.get()).get_mut() }
        };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }

    /// Wait on the given condition variable, and resume with another write lock.
    pub fn wait_for_write(self, cond: &Condvar) -> LockResult<Self> {
        self.mutex.raw.wait_from_write_to_write(cond);

        let guard = unsafe { SharedMutexWriteGuard::new(self.mutex) };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }

    /// Wait on the given condition variable, and resume with a read lock.
    pub fn wait_for_read(self, cond: &Condvar) -> LockResult<SharedMutexReadGuard<'mutex, T>> {
        self.mutex.raw.wait_from_write_to_read(cond);

        let guard = unsafe { SharedMutexReadGuard::new(self.mutex) };

        // Don't double-unlock.
        mem::forget(self);

        guard
    }
}

impl<'mutex, T: ?Sized> Drop for SharedMutexReadGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) { self.mutex.raw.unlock_read() }
}

impl<'mutex, T: ?Sized> Drop for SharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) { self.mutex.raw.unlock_write() }
}

/// A read guard to a sub-borrow of an original SharedMutexReadGuard.
///
/// Unlike SharedMutexReadGuard, it cannot be used to wait on a
/// `Condvar`.
pub struct MappedSharedMutexReadGuard<'mutex, T: ?Sized + 'mutex> {
    mutex: &'mutex RawSharedMutex,
    data: &'mutex T
}

/// A write guard to a sub-borrow of an original `SharedMutexWriteGuard`.
///
/// Unlike `SharedMutexWriteGuard`, it cannot be used to wait on a
/// `Condvar`.
pub struct MappedSharedMutexWriteGuard<'mutex, T: ?Sized + 'mutex> {
    mutex: &'mutex RawSharedMutex,
    poison: RawPoisonGuard<'mutex>,
    data: &'mutex mut T,
}

impl<'mutex, T: ?Sized> MappedSharedMutexReadGuard<'mutex, T> {
    /// Transform this guard into a sub-borrow of the original data.
    #[inline]
    pub fn map<U: ?Sized, F>(self, action: F) -> MappedSharedMutexReadGuard<'mutex, U>
    where F: FnOnce(&T) -> &U {
        self.option_map(move |t| Some(action(t))).unwrap()
    }

    /// Conditionally transform this guard into a sub-borrow of the original data.
    #[inline]
    pub fn option_map<U: ?Sized, F>(self, action: F) -> Option<MappedSharedMutexReadGuard<'mutex, U>>
    where F: FnOnce(&T) -> Option<&U> {
        self.result_map(move |t| action(t).ok_or(())).ok()
    }

    /// Conditionally transform this guard into a sub-borrow of the original data.
    ///
    /// If the transformation operation is aborted, returns the original guard.
    #[inline]
    pub fn result_map<U: ?Sized, E, F>(self, action: F)
        -> Result<MappedSharedMutexReadGuard<'mutex, U>, (Self, E)>
    where F: FnOnce(&T) -> Result<&U, E> {
        let data = self.data;
        let mutex = self.mutex;

        match action(data) {
            Ok(new_data) => {
                // Don't double-unlock.
                mem::forget(self);

                Ok(MappedSharedMutexReadGuard {
                    data: new_data,
                    mutex: mutex
                })
            },
            Err(e) => { Err((self, e)) }
        }
    }

    /// Recover the original guard for waiting.
    ///
    /// Takes the original mutex to recover the original type and data. If the
    /// passed mutex is not the same object as the original mutex, returns `Err`.
    #[inline]
    pub fn recover<U: ?Sized>(self, mutex: &'mutex SharedMutex<U>) -> Result<SharedMutexReadGuard<'mutex, U>, Self> {
        if self.mutex.is(&mutex.raw) {
            // The mutex can't have become poisoned since we are continuously holding a guard.
            let guard = unsafe { SharedMutexReadGuard::new(mutex) }.unwrap();

            // Don't double-unlock.
            mem::forget(self);

            Ok(guard)
        } else {
            Err(self)
        }
    }
}

impl<'mutex, T: ?Sized> MappedSharedMutexWriteGuard<'mutex, T> {
    /// Transform this guard into a sub-borrow of the original data.
    #[inline]
    pub fn map<U: ?Sized, F>(self, action: F) -> MappedSharedMutexWriteGuard<'mutex, U>
    where F: FnOnce(&mut T) -> &mut U {
        self.option_map(move |t| Some(action(t))).unwrap()
    }

    /// Conditionally transform this guard into a sub-borrow of the original data.
    #[inline]
    pub fn option_map<U: ?Sized, F>(self, action: F) -> Option<MappedSharedMutexWriteGuard<'mutex, U>>
    where F: FnOnce(&mut T) -> Option<&mut U> {
        self.result_map(move |t| action(t).ok_or(())).ok()
    }

    /// Conditionally transform this guard into a sub-borrow of the original data.
    ///
    /// If the transformation operation is aborted, returns the original guard.
    #[inline]
    pub fn result_map<U: ?Sized, E, F>(self, action: F)
        -> Result<MappedSharedMutexWriteGuard<'mutex, U>, (Self, E)>
    where F: FnOnce(&mut T) -> Result<&mut U, E> {
        let data = unsafe { ptr::read(&self.data) };
        let mutex = self.mutex;

        match action(data) {
            Ok(new_data) => {
                let poison = unsafe { ptr::read(&self.poison) };

                // Don't double-unlock.
                mem::forget(self);

                Ok(MappedSharedMutexWriteGuard {
                    data: new_data,
                    poison: poison,
                    mutex: mutex
                })
            },
            Err(e) => { Err((self, e)) }
        }
    }

    /// Recover the original guard for waiting.
    ///
    /// Takes the original mutex to recover the original type and data. If the
    /// passed mutex is not the same object as the original mutex, returns `Err`.
    #[inline]
    pub fn recover<U: ?Sized>(self, mutex: &'mutex SharedMutex<U>) -> Result<SharedMutexWriteGuard<'mutex, U>, Self> {
        if self.mutex.is(&mutex.raw) {
            // The mutex can't have become poisoned since we are continuously holding a guard.
            let guard = unsafe { SharedMutexWriteGuard::new(mutex) }.unwrap();

            // Don't double-unlock.
            mem::forget(self);

            Ok(guard)
        } else {
            Err(self)
        }
    }
}

impl<'mutex, T: ?Sized> Deref for MappedSharedMutexReadGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data }
}

impl<'mutex, T: ?Sized> Deref for MappedSharedMutexWriteGuard<'mutex, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T { self.data }
}

impl<'mutex, T: ?Sized> DerefMut for MappedSharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T { self.data }
}

impl<'mutex, T: ?Sized> Drop for MappedSharedMutexReadGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) { self.mutex.unlock_read() }
}

impl<'mutex, T: ?Sized> Drop for MappedSharedMutexWriteGuard<'mutex, T> {
    #[inline]
    fn drop(&mut self) { self.mutex.unlock_write() }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SharedMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut writer = f.debug_struct("SharedMutex");

        match self.try_read() {
            Ok(l) => writer.field("data", &&*l),
            Err(TryLockError::WouldBlock) => writer.field("data", &"{{ locked }}"),
            Err(TryLockError::Poisoned(_)) => writer.field("data", &"{{ poisoned }}")
        }.finish()
    }
}

impl<'mutex, T: ?Sized + fmt::Debug> fmt::Debug for SharedMutexReadGuard<'mutex, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SharedMutexReadGuard")
            .field("data", &*self)
            .finish()
    }
}

impl<'mutex, T: ?Sized + fmt::Debug> fmt::Debug for SharedMutexWriteGuard<'mutex, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SharedMutexWriteGuard")
            .field("data", &*self)
            .finish()
    }
}

impl<'mutex, T: ?Sized + fmt::Debug> fmt::Debug for MappedSharedMutexReadGuard<'mutex, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MappedSharedMutexReadGuard")
            .field("data", &*self)
            .finish()
    }
}

impl<'mutex, T: ?Sized + fmt::Debug> fmt::Debug for MappedSharedMutexWriteGuard<'mutex, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MappedSharedMutexWriteGuard")
            .field("data", &*self)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Condvar, Barrier};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use scoped_pool::Pool;

    use super::*;

    fn _check_bounds() {
        fn _is_send_sync<T: Send + Sync>() {}

        _is_send_sync::<RawSharedMutex>();
        _is_send_sync::<SharedMutex<()>>();
        _is_send_sync::<SharedMutexReadGuard<()>>();
        _is_send_sync::<SharedMutexWriteGuard<()>>();
    }

    #[test]
    fn test_simple_multithreaded() {
        let pool = Pool::new(8);
        let mut mutex = SharedMutex::new(0);
        let n = 100;

        pool.scoped(|scope| {
            for _ in 0..n {
                scope.execute(|| {
                    let before = *mutex.read().unwrap();
                    *mutex.write().unwrap() += 1;
                    let after = *mutex.read().unwrap();

                    assert!(before < after, "Time travel! {:?} >= {:?}", before, after);
                })
            }
        });

        assert_eq!(*mutex.get_mut().unwrap(), 100);
        pool.shutdown();
    }

    #[test]
    fn test_simple_single_thread() {
        let mut mutex = SharedMutex::new(0);
        let n = 100;

        for _ in 0..n {
            let before = *mutex.read().unwrap();
            *mutex.write().unwrap() += 1;
            let after = *mutex.read().unwrap();

            assert!(before < after, "Time travel! {:?} >= {:?}", before, after);
        }

        assert_eq!(*mutex.get_mut().unwrap(), 100);
    }

    #[test]
    fn test_locking_multithreaded() {
        // This test makes a best effort to test the actual locking
        // behavior of the mutex.
        //
        // Read locks attempt to read from an atomic many times,
        // while write locks write to them many times.
        //
        // If any of these operations interleave (readers read different
        // values under the same lock, writers observe other writers) then
        // we know there is a bug.
        //
        // We make use of a barrier to attempt to cluster threads together.

        let mut mutex = SharedMutex::new(());
        let value = AtomicUsize::new(0);

        let threads = 50;
        let actors = threads * 20; // Must be a multiple threads.
        let actions_per_actor = 100;
        let start_barrier = Barrier::new(threads);
        let pool = Pool::new(threads);

        pool.scoped(|scope| {
            for _ in 0..actors {
                // Reader
                scope.execute(|| {
                    start_barrier.wait();

                    let _read = mutex.read().unwrap();
                    let original = value.load(Ordering::SeqCst);

                    for _ in 0..actions_per_actor {
                        assert_eq!(original, value.load(Ordering::SeqCst));
                    }
                });

                // Writer
                scope.execute(|| {
                    start_barrier.wait();

                    let _write = mutex.write().unwrap();
                    let mut previous = value.load(Ordering::SeqCst);

                    for _ in 0..actions_per_actor {
                        let next = value.fetch_add(1, Ordering::SeqCst);

                        // fetch_add returns the old value
                        assert_eq!(previous, next);

                        // next time we will expect the old value + 1
                        previous = next + 1;
                    }
                });
            }
        });

        mutex.get_mut().unwrap();
        pool.shutdown();
    }

    #[test]
    fn test_simple_waiting() {
        let pool = Pool::new(20);
        let mutex = SharedMutex::new(());
        let cond = Condvar::new();

        pool.scoped(|scope| {
            let lock = mutex.write().unwrap();

            scope.execute(|| {
                let _ = mutex.write().unwrap();
                cond.notify_one();
            });

            // Write -> Read
            let lock = lock.wait_for_read(&cond).unwrap();

            scope.execute(|| {
                drop(mutex.write().unwrap());
                cond.notify_one();
            });

            // Read -> Read
            let lock = lock.wait_for_read(&cond).unwrap();

            scope.execute(|| {
                drop(mutex.write().unwrap());
                cond.notify_one();
            });


            // Read -> Write
            let lock = lock.wait_for_write(&cond).unwrap();

            scope.execute(|| {
                drop(mutex.write().unwrap());
                cond.notify_one();
            });

            // Write -> Write
            lock.wait_for_write(&cond).unwrap();
        });

        pool.shutdown();
    }

    #[test]
    fn test_mapping() {
        let mutex = SharedMutex::new(vec![1, 2, 3]);

        *mutex.write().unwrap().into_mapped()
            .map(|v| &mut v[0]) = 100;

        assert_eq!(*mutex.read().unwrap().into_mapped().map(|v| &v[0]), 100);
    }

    #[test]
    fn test_map_recover() {
        let mutex = SharedMutex::new(vec![1, 2]);

        let mut write_map = mutex.write().unwrap().into_mapped()
            .map(|v| &mut v[0]);
        *write_map = 123;

        let whole_guard = write_map.recover(&mutex).unwrap();
        assert_eq!(&*whole_guard, &[123, 2]);
    }

    #[test]
    fn test_try_locking() {
        let mutex = SharedMutex::new(10);
        mutex.try_read().unwrap();
        mutex.try_write().unwrap();
    }
}

