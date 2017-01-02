use super::super::Spawn;
use futures::sync::{mpsc, oneshot};
use futures::{Future, Poll, Stream, Async, Sink};
use std::sync::{LockResult, PoisonError};
use std::{mem, thread, fmt};
use std::ops::{Deref, DerefMut};
use std::error::Error;
use self::MutexState::*;


type CompletionSender<T> = oneshot::Sender<LockResult<T>>;
type GuardSender<T> = oneshot::Sender<LockResult<MutexGuard<T>>>;
type StepResult<T> = (MutexState<T>, Async<bool>);

struct LockRequest<T: Send>(GuardSender<T>);

enum MutexState<T> {
    Unlocked(T, CompletionSender<T>),
    Locked(oneshot::Receiver<(T, bool)>, CompletionSender<T>),
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

    fn satisfy_lock_req(&mut self, value: T, req: GuardSender<T>, completion: CompletionSender<T>) -> StepResult<T> {
        // Create a channel to receive the "unlock" message
        let (sender, receiver) = oneshot::channel();
        // Send a mutex guard to the lucky locker
        let guard = MutexGuard::new(value, sender);
        req.complete(self.poison(guard));
        // Transition to "locked" state
        (Locked(receiver, completion), Async::Ready(false))
    }

    fn satisfy_unlock_req(&mut self, value: T, poisoned: bool, completion: CompletionSender<T>) -> StepResult<T> {
        // Unlock and maybe poison the mutex
        self.is_poisoned |= poisoned;
        (Unlocked(value, completion), Async::Ready(false))
    }

    fn step(&mut self, state: MutexState<T>) -> StepResult<T> {
        match state {
            // Mutex is currently unlocked
            Unlocked(value, completion) => {
                // Check for lock requests
                // The mpsc::channel should never return errors...
                match self.requests.poll().unwrap() {
                    // Lock request stream is closed, so mutex task should end
                    Async::Ready(None) => (Unlocked(value, completion), Async::Ready(true)),
                    // Received a lock request
                    Async::Ready(Some(LockRequest(req))) => self.satisfy_lock_req(value, req, completion),
                    // No requests outstanding
                    Async::NotReady => (Unlocked(value, completion), Async::NotReady),
                }
            },
            // Mutex is currently locked
            Locked(mut receiver, completion) => {
                // Check for an unlock notification
                // MutexGuard should never close the channel before sending an unlock message
                match receiver.poll().unwrap() {
                    // Received an unlock message
                    Async::Ready((value, poisoned)) => self.satisfy_unlock_req(value, poisoned, completion),
                    // No unlock message yet
                    Async::NotReady => (Locked(receiver, completion), Async::NotReady),
                }
            },
            // We should never be polled while in this transient state
            Invalid => unreachable!()
        }
    }
}

impl<T: Send> Future for MutexTask<T> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        // Run the state machine until it blocks
        loop {
            // Enter "Invalid" state temporarily
            let old_state = mem::replace(&mut self.state, Invalid);
            // Step the FSM
            let (new_state, result) = self.step(old_state);
            // Enter the new state
            self.state = new_state;
            // Check if we're done
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
        if let Unlocked(value, completion) = mem::replace(&mut self.state, Invalid) {
            completion.complete(self.poison(value));
        }
    }
}

/// A handle to a future-based mutex
#[derive(Clone)]
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Mutex canceled")
    }
}

impl Error for Canceled {
    fn description(&self) -> &str {
        "Mutex canceled"
    }
}

impl<T: Send> Mutex<T> {
    /// Create a new mutex and run it on the specified `Spawn` implementation
    pub fn new<S: Spawn<MutexTask<T>>>(value: T, spawn: &S) -> (Self, MutexCompletion<T>) {
        let (sender, receiver) = mpsc::channel(0);
        let (completion_sender, completion_receiver) = oneshot::channel();
        spawn.spawn_detached(MutexTask {
            requests: receiver,
            state: Unlocked(value, completion_sender),
            is_poisoned: false
        });
        (Mutex(sender), MutexCompletion(completion_receiver))
    }

    /// Attempt to lock the RwLock for writing
    pub fn lock(self) -> impl Future<Item=(Mutex<T>, LockResult<MutexGuard<T>>), Error=Canceled> {
        let (sender, receiver) = oneshot::channel();
        self.0.send(
            LockRequest(sender)
        ).map(|c| Mutex(c)).map_err(|_|Canceled).join(
            receiver.map_err(|_|Canceled)
        )
    }
}
