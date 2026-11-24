use async_lock::{Mutex, Semaphore};

/// A fair semaphore built on [`async_lock::Semaphore`].
///
///
pub struct FairSemaphore {
    semaphore: Semaphore,
    // `Mutex` is
    mutex: Mutex<()>,
}

impl FairSemaphore {
    pub fn new(permits: usize) -> Self {}
}
