use super::super::Spawn;
use futures::sync::{mpsc, oneshot};
use futures::{Future, Poll, Stream, Async, Sink};
use futures::task::{self, Task};
use std::sync::{LockResult, PoisonError, Arc};
use std::{mem, fmt, thread};
use std::ops::{Deref, DerefMut};
use std::error::Error;
use self::RwLockState::*;


enum LockRequest<T: Send> {
    Exclusive(oneshot::Sender<LockResult<RwLockWriteGuard<T>>>),
    Shared(oneshot::Sender<LockResult<RwLockReadGuard<T>>>)
}

enum RwLockState<T: Send> {
    Unlocked {
        value: T,
        completion: oneshot::Sender<LockResult<T>>
    },
    LockedRead {
        arc: Arc<T>,
        completion: oneshot::Sender<LockResult<T>>
    },
    LockedReadPendingWrite {
        arc: Arc<T>,
        req: oneshot::Sender<LockResult<RwLockWriteGuard<T>>>,
        completion: oneshot::Sender<LockResult<T>>
    },
    LockedWrite {
        unlock: oneshot::Receiver<(T, bool)>,
        completion: oneshot::Sender<LockResult<T>>
    },
    Invalid
}

/// Represents a living rwlock. Never constructed directly.
pub struct RwLockTask<T: Send> {
    requests: mpsc::Receiver<LockRequest<T>>,
    state: RwLockState<T>,
    is_poisoned: bool
}

impl<T: Send> RwLockTask<T> {
    fn poison<U>(&self, value: U) -> LockResult<U> {
        if self.is_poisoned {
            Err(PoisonError::new(value))
        } else {
            Ok(value)
        }
    }

    fn step(&mut self, state: RwLockState<T>) -> (RwLockState<T>, Async<bool>) {
        match state {
            // RwLock is currently unlocked
            Unlocked { value, completion } => {
                // Check for lock requests
                match self.requests.poll() {
                    // Lock request stream is closed, so rwlock task should end
                    Ok(Async::Ready(None)) => (Unlocked {
                        value: value,
                        completion: completion
                    }, Async::Ready(true)),
                    // Received a lock request
                    Ok(Async::Ready(Some(LockRequest::Exclusive(req)))) => {
                        // Create a channel to receive the "unlock" message
                        let (sender, receiver) = oneshot::channel();
                        // Send a guard to the lucky locker
                        let guard = RwLockWriteGuard::new(value, sender);
                        req.complete(self.poison(guard));
                        // Transition to write locked state
                        (LockedWrite {
                            unlock: receiver,
                            completion: completion
                        }, Async::Ready(false))
                    },
                    Ok(Async::Ready(Some(LockRequest::Shared(req)))) => {
                        let unlock = task::park();
                        let arc = Arc::new(value);
                        // Send a guard to the lucky locker
                        let guard = RwLockReadGuard::new(arc.clone(), unlock);
                        req.complete(self.poison(guard));
                        // Transition to read locked state
                        (LockedRead {
                            arc: arc,
                            completion: completion
                        }, Async::Ready(false))
                    },
                    // No requests outstanding
                    Ok(Async::NotReady) => (Unlocked {
                        value: value,
                        completion: completion
                    }, Async::NotReady),
                    // The mpsc::channel should never return errors...
                    Err(()) => unreachable!()
                }
            },
            // RwLock is currently read locked, with no pending write locks
            LockedRead { arc, completion } => {
                // Check for any lock requests
                match self.requests.poll() {
                    // Lock request stream is closed, so do nothing.
                    Ok(Async::Ready(None)) => {
                        // Check if all read locks have been released
                        match Arc::try_unwrap(arc) {
                            Ok(value) => (Unlocked {
                                value: value,
                                completion: completion
                            }, Async::Ready(true)),
                            Err(arc) => {
                                // Can't terminate while there are outstanding locks, so just keep polling.
                                // Hopefully the stream is fused...
                                (LockedRead {
                                    arc: arc,
                                    completion: completion
                                }, Async::NotReady)
                            }
                        }
                    },
                    // Received a write lock request
                    Ok(Async::Ready(Some(LockRequest::Exclusive(req)))) => (LockedReadPendingWrite {
                        arc: arc,
                        req: req,
                        completion: completion
                    }, Async::Ready(false)),
                    // Received a read lock request
                    Ok(Async::Ready(Some(LockRequest::Shared(req)))) => {
                        let unlock = task::park();
                        // Send a guard to the lucky locker
                        let guard = RwLockReadGuard::new(arc.clone(), unlock);
                        req.complete(self.poison(guard));
                        // Transition to read locked state
                        (LockedRead {
                            arc: arc,
                            completion: completion
                        }, Async::Ready(false))
                    },
                    // No requests outstanding
                    Ok(Async::NotReady) => {
                        // Check if all read locks have been released
                        match Arc::try_unwrap(arc) {
                            Ok(value) => (Unlocked {
                                value: value,
                                completion: completion
                            }, Async::Ready(false)),
                            Err(arc) => {
                                (LockedRead {
                                    arc: arc,
                                    completion: completion
                                }, Async::NotReady)
                            }
                        }
                    },
                    // The mpsc::channel should never return errors...
                    Err(()) => unreachable!()
                }
            },
            // RwLock is currently read locked, with a pending write lock
            LockedReadPendingWrite { arc, req, completion } => {
                // Check if all read locks have been released
                match Arc::try_unwrap(arc) {
                    Ok(value) => {
                        // Create a channel to receive the "unlock" message
                        let (sender, unlock) = oneshot::channel();
                        // Send a guard to the lucky locker
                        let guard = RwLockWriteGuard::new(value, sender);
                        req.complete(self.poison(guard));
                        // Transition to write locked state
                        (LockedWrite {
                            unlock: unlock,
                            completion: completion
                        }, Async::Ready(false))
                    },
                    Err(arc) => {
                        (LockedReadPendingWrite {
                            arc: arc,
                            req: req,
                            completion: completion
                        }, Async::NotReady)
                    }
                }
            },
            // RwLock is currently write locked
            LockedWrite { mut unlock, completion } => {
                // Check for an unlock notification
                match unlock.poll() {
                    // Received an unlock message
                    Ok(Async::Ready((value, poisoned))) => {
                        // Unlock and maybe poison the rwlock
                        self.is_poisoned |= poisoned;
                        (Unlocked {
                            value: value,
                            completion: completion
                        }, Async::Ready(false))
                    },
                    // No unlock message yet
                    Ok(Async::NotReady) => (LockedWrite {
                        unlock: unlock,
                        completion: completion
                    }, Async::NotReady),
                    // RwLockWriteGuard should never close the channel before sending an unlock message
                    Err(_) => unreachable!()
                }
            },
            // We should never be polled while in this transient state
            Invalid => unreachable!()
        }
    }
}

impl<T: Send> Future for RwLockTask<T> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let old_state = mem::replace(&mut self.state, Invalid);
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

impl<T: Send> Drop for RwLockTask<T> {
    fn drop(&mut self) {
        if let Unlocked { value, completion } = mem::replace(&mut self.state, Invalid) {
            completion.complete(self.poison(value));
        }
    }
}

/// A handle to a future-based RwLock
#[derive(Clone)]
pub struct RwLock<T: Send>(mpsc::Sender<LockRequest<T>>);

struct RwLockWriteGuardInner<T: Send> {
    value: T,
    unlock: oneshot::Sender<(T, bool)>
}

#[derive(Debug)]
struct RwLockReadGuardInner<T: Send> {
    value: Arc<T>,
    unlock: Task
}

/// A write guard mediating access to the RwLock
pub struct RwLockWriteGuard<T: Send>(Option<RwLockWriteGuardInner<T>>);
/// A read guard mediating access to the RwLock
#[derive(Debug)]
pub struct RwLockReadGuard<T: Send>(Option<RwLockReadGuardInner<T>>);

impl<T: Send> RwLockWriteGuard<T> {
    fn new(value: T, unlock: oneshot::Sender<(T, bool)>) -> Self {
        RwLockWriteGuard(Some(RwLockWriteGuardInner {
            value: value,
            unlock: unlock
        }))
    }
}

impl<T: Send> RwLockReadGuard<T> {
    fn new(value: Arc<T>, unlock: Task) -> Self {
        RwLockReadGuard(Some(RwLockReadGuardInner {
            value: value,
            unlock: unlock
        }))
    }
}

impl<T: Send> Drop for RwLockWriteGuard<T> {
    fn drop(&mut self) {
        let RwLockWriteGuardInner { value, unlock} = self.0.take().unwrap();
        unlock.complete((value, thread::panicking()));
    }
}

impl<T: Send> Drop for RwLockReadGuard<T> {
    fn drop(&mut self) {
        let RwLockReadGuardInner { value, unlock } = self.0.take().unwrap();
        // Must drop value before sending the notification
        drop(value);
        // Wake up the task
        unlock.unpark();
    }
}

impl<T: Send> Deref for RwLockWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.as_ref().unwrap().value
    }
}

impl<T: Send> Deref for RwLockReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.as_ref().unwrap().value
    }
}

impl<T: Send> DerefMut for RwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0.as_mut().unwrap().value
    }
}

/// Future which completes with the value contained within an RwLock
pub struct RwLockCompletion<T: Send>(oneshot::Receiver<LockResult<T>>);

impl<T: Send> Future for RwLockCompletion<T> {
    type Item = LockResult<T>;
    type Error = oneshot::Canceled;

    fn poll(&mut self) -> Poll<LockResult<T>, oneshot::Canceled> {
        self.0.poll()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "RwLock canceled")
    }
}

impl Error for Canceled {
    fn description(&self) -> &str {
        "RwLock canceled"
    }
}

impl<T: Send> RwLock<T> {
    /// Create a new rwlock and run it on the specified `Spawn` implementation
    pub fn new<S: Spawn<RwLockTask<T>>>(value: T, spawn: &S) -> (Self, RwLockCompletion<T>) {
        let (sender, receiver) = mpsc::channel(0);
        let (completion_sender, completion_receiver) = oneshot::channel();
        spawn.spawn_detached(RwLockTask {
            requests: receiver,
            state: Unlocked {
                value: value,
                completion: completion_sender
            },
            is_poisoned: false
        });
        (RwLock(sender), RwLockCompletion(completion_receiver))
    }

    /// Attempt to lock the RwLock for writing
    pub fn write(self) -> impl Future<Item=(RwLock<T>, LockResult<RwLockWriteGuard<T>>), Error=Canceled> {
        let (sender, receiver) = oneshot::channel();
        self.0.send(
            LockRequest::Exclusive(sender)
        ).map(|c| RwLock(c)).map_err(|_|Canceled).join(
            receiver.map_err(|_|Canceled)
        )
    }

    /// Attempt to lock the RwLock for reading
    pub fn read(self) -> impl Future<Item=(RwLock<T>, LockResult<RwLockReadGuard<T>>), Error=Canceled> {
        let (sender, receiver) = oneshot::channel();
        self.0.send(
            LockRequest::Shared(sender)
        ).map(|c| RwLock(c)).map_err(|_|Canceled).join(
            receiver.map_err(|_|Canceled)
        )
    }
}
