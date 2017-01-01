use super::super::Spawn;
use futures::sync::{mpsc, oneshot};
use futures::{Future, Poll, Stream, Async};
use std::thread;
use std::sync::{LockResult, PoisonError};
use std::mem;
use std::ops::{Deref, DerefMut};


struct LockRequest<T: Send>(oneshot::Sender<LockResult<MutexGuard<T>>>);

enum MutexState<T> {
    Unlocked(T, oneshot::Sender<LockResult<T>>),
    Locked(oneshot::Receiver<(T, bool)>, oneshot::Sender<LockResult<T>>),
    Invalid
}

/// Represents a living mutex. Never constructed directly.
pub struct MutexTask<T: Send> {
    requests: mpsc::Receiver<LockRequest<T>>,
    state: MutexState<T>,
    is_poisoned: bool
}

impl<T: Send> MutexTask<T> {
    fn poison<U>(&self, value: U) -> LockResult<U> {
        if self.is_poisoned {
            Err(PoisonError::new(value))
        } else {
            Ok(value)
        }
    }

    fn step(&mut self, state: MutexState<T>) -> (MutexState<T>, Async<bool>) {
        match state {
            // Mutex is currently unlocked
            MutexState::Unlocked(value, cs) => {
                // Check for lock requests
                match self.requests.poll() {
                    // Lock request stream is closed, so mutex task should end
                    Ok(Async::Ready(None)) => (MutexState::Unlocked(value, cs), Async::Ready(true)),
                    // Received a lock request
                    Ok(Async::Ready(Some(LockRequest(req)))) => {
                        // Create a channel to receive the "unlock" message
                        let (sender, receiver) = oneshot::channel();
                        // Send a mutex guard to the lucky locker
                        let guard = MutexGuard::new(value, sender);
                        req.complete(self.poison(guard));
                        // Transition to "locked" state
                        (MutexState::Locked(receiver, cs), Async::Ready(false))
                    },
                    // No requests outstanding
                    Ok(Async::NotReady) => (MutexState::Unlocked(value, cs), Async::NotReady),
                    // The mpsc::channel should never return errors...
                    Err(()) => unreachable!()
                }
            },
            // Mutex is currently locked
            MutexState::Locked(mut receiver, cs) => {
                // Check for an unlock notification
                match receiver.poll() {
                    // Received an unlock message
                    Ok(Async::Ready((value, poisoned))) => {
                        // Unlock and maybe poison the mutex
                        self.is_poisoned |= poisoned;
                        (MutexState::Unlocked(value, cs), Async::Ready(false))
                    },
                    // No unlock message yet
                    Ok(Async::NotReady) => (MutexState::Locked(receiver, cs), Async::NotReady),
                    // MutexGuard should never close the channel before sending an unlock message
                    Err(_) => unreachable!()
                }
            },
            // We should never be polled while in this transient state
            MutexState::Invalid => unreachable!()
        }
    }
}

impl<T: Send> Future for MutexTask<T> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let old_state = mem::replace(&mut self.state, MutexState::Invalid);
            let (new_state, result) = self.step(old_state);
            self.state = new_state;
            match result {
                Async::NotReady => return Ok(Async::NotReady),
                Async::Ready(true) => return Ok(Async::Ready(())),
                Async::Ready(false) => {}
            }
        }
    }
}

impl<T: Send> Drop for MutexTask<T> {
    fn drop(&mut self) {
        if let MutexState::Unlocked(value, cs) = mem::replace(&mut self.state, MutexState::Invalid) {
            cs.complete(self.poison(value));
        }
    }
}

/// A handle to a future-based mutex
pub struct Mutex<T: Send>(mpsc::Sender<LockRequest<T>>);

struct MutexGuardInner<T: Send> {
    value: T,
    unlock: oneshot::Sender<(T, bool)>
}

/// A guard mediating access to the mutex
pub struct MutexGuard<T: Send>(Option<MutexGuardInner<T>>);

impl<T: Send> MutexGuard<T> {
    fn new(value: T, unlock: oneshot::Sender<(T, bool)>) -> Self {
        MutexGuard(Some(MutexGuardInner {
            value: value,
            unlock: unlock
        }))
    }
}

impl<T: Send> Drop for MutexGuard<T> {
    fn drop(&mut self) {
        let MutexGuardInner { value, unlock} = self.0.take().unwrap();
        unlock.complete((value, thread::panicking()));
    }
}

impl<T: Send> Deref for MutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.as_ref().unwrap().value
    }
}

impl<T: Send> DerefMut for MutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0.as_mut().unwrap().value
    }
}

/// Future which completes with the value contained within a mutex
pub struct MutexCompletion<T: Send>(oneshot::Receiver<LockResult<T>>);

impl<T: Send> Future for MutexCompletion<T> {
    type Item = LockResult<T>;
    type Error = oneshot::Canceled;

    fn poll(&mut self) -> Poll<LockResult<T>, oneshot::Canceled> {
        self.0.poll()
    }
}

impl<T: Send> Mutex<T> {
    /// Create a new mutex and run it on the specified `Spawn` implementation
    pub fn new<S: Spawn<MutexTask<T>>>(value: T, spawn: &S) -> (Self, MutexCompletion<T>) {
        let (sender, receiver) = mpsc::channel(0);
        let (completion_sender, completion_receiver) = oneshot::channel();
        spawn.spawn_detached(MutexTask {
            requests: receiver,
            state: MutexState::Unlocked(value, completion_sender),
            is_poisoned: false
        });
        (Mutex(sender), MutexCompletion(completion_receiver))
    }
}
