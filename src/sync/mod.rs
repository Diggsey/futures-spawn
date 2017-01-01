mod mutex;
mod rwlock;

pub use self::mutex::{Mutex, MutexGuard, MutexCompletion, MutexTask};
pub use self::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard, RwLockCompletion, RwLockTask};
