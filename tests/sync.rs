extern crate futures;
extern crate futures_spawn;

use futures::Async;
use futures::executor;
use futures::future::Future;
use futures::executor::Spawn;
use futures::task::Unpark;
use futures_spawn::sync::{Mutex, RwLock};
use std::thread;
use std::sync::atomic::{AtomicBool, Ordering};
use std::cell::RefCell;
use std::sync::Arc;

struct ThreadUnpark {
    thread: thread::Thread,
    ready: AtomicBool,
}

impl ThreadUnpark {
    fn new(thread: thread::Thread) -> ThreadUnpark {
        ThreadUnpark {
            thread: thread,
            ready: AtomicBool::new(false),
        }
    }

    fn park(&self) {
        if !self.ready.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }
}

impl Unpark for ThreadUnpark {
    fn unpark(&self) {
        self.ready.store(true, Ordering::SeqCst);
        self.thread.unpark()
    }
}
struct SingleThreadExecutor {
    tasks: RefCell<Vec<Option<Spawn<Box<Future<Item=(), Error=()>>>>>>,
}

impl SingleThreadExecutor {
    fn new() -> Self {
        SingleThreadExecutor { tasks: RefCell::new(Vec::new()) }
    }
    fn wait<F: Future>(&self, f: F) -> Result<F::Item, F::Error> {
        let unpark = Arc::new(ThreadUnpark::new(thread::current()));
        let mut s = executor::spawn(f);
        loop {
            if let Async::Ready(e) = try!(s.poll_future(unpark.clone())) {
                return Ok(e)
            }

            for maybe_t in &mut *self.tasks.borrow_mut() {
                if let Some(mut t) = maybe_t.take() {
                    match t.poll_future(unpark.clone()) {
                        Ok(Async::NotReady) => {
                            *maybe_t = Some(t)
                        },
                        _ => {}
                    }
                }
            }

            unpark.park();
        }
    }
}

impl<F: Future<Item=(), Error=()> + 'static> futures_spawn::Spawn<F> for SingleThreadExecutor {
    fn spawn_detached(&self, f: F) {
        self.tasks.borrow_mut().push(Some(executor::spawn(Box::new(f))));
    }
}

#[test]
fn smoke_mutex() {
    use futures_spawn::Spawn;

    let executor = SingleThreadExecutor::new();
    let (mutex, completion) = Mutex::new(1, &executor);
    for x in 1..5 {
        executor.spawn_detached(mutex.clone().lock().map(move |(_, lock_result)| {
            let mut guard = lock_result.expect("Mutex was poisoned");
            assert_eq!(*guard, x);
            *guard += 1;
        }).map_err(|_| panic!("Mutex was canceled")));
    }
    drop(mutex);
    let result = executor.wait(completion).expect("Mutex did not complete!").expect("Mutex was poisoned");
    assert_eq!(result, 5);
}

#[test]
fn smoke_rwlock() {
    use futures_spawn::Spawn;

    let executor = SingleThreadExecutor::new();
    let (rwlock, completion) = RwLock::new(1, &executor);

    executor.spawn_detached(rwlock.clone().read().and_then(move |(rwlock, lock_result)| {
        let guard = lock_result.expect("RwLock was poisoned");
        assert_eq!(*guard, 1);
        
        let write_future = rwlock.clone().write().map(move |(_, lock_result)| {
            let mut guard = lock_result.expect("RwLock was poisoned");
            assert_eq!(*guard, 1);
            *guard += 1;
        });

        let read_future = rwlock.clone().read().map(move |(_, lock_result)| {
            let guard = lock_result.expect("RwLock was poisoned");
            assert_eq!(*guard, 2);
        });

        write_future.join(read_future).map(|((), ())| ())
    }).map_err(|_| panic!("RwLock was canceled")));

    drop(rwlock);
    let result = executor.wait(completion).expect("RwLock did not complete!").expect("RwLock was poisoned");
    assert_eq!(result, 2);
}
