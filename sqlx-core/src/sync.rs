// For types with identical signatures that don't require runtime support,
// we can just arbitrarily pick one to use based on what's enabled.
//
// We'll generally lean towards Tokio's types as those are more featureful
// (including `tokio-console` support) and more widely deployed.

use std::sync::Arc;
#[cfg(feature = "_rt-tokio")]
pub use tokio::sync::{
    Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, OwnedMutexGuard as AsyncMutexGuardArc,
    RwLock as AsyncRwLock,
};

#[cfg(all(feature = "_rt-async-lock", not(feature = "_rt-tokio")))]
pub use async_lock::{
    Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, MutexGuardArc as AsyncMutexGuardArc,
    RwLock as AsyncRwLock,
};

pub async fn lock_arc<T>(mutex: &Arc<AsyncMutex<T>>) -> AsyncMutexGuardArc<T> {
    #[cfg(feature = "_rt-tokio")]
    return mutex.clone().lock_owned().await;

    #[cfg(all(feature = "_rt-async-lock", not(feature = "_rt-tokio")))]
    return mutex.lock_arc().await;

    #[cfg(not(any(feature = "_rt-async-lock", feature = "_rt-tokio")))]
    return crate::rt::missing_rt(mutex);
}

pub fn try_lock_arc<T>(mutex: &Arc<AsyncMutex<T>>) -> Option<AsyncMutexGuardArc<T>> {
    #[cfg(feature = "_rt-tokio")]
    return mutex.clone().try_lock_owned().ok();

    #[cfg(all(feature = "_rt-async-lock", not(feature = "_rt-tokio")))]
    return mutex.try_lock_arc();

    #[cfg(not(any(feature = "_rt-async-lock", feature = "_rt-tokio")))]
    return crate::rt::missing_rt(mutex);
}

#[cfg(not(any(feature = "_rt-async-lock", feature = "_rt-tokio")))]
pub use noop::*;

#[cfg(not(any(feature = "_rt-async-lock", feature = "_rt-tokio")))]
mod noop {
    use crate::rt::missing_rt;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};
    use std::sync::Arc;

    pub struct AsyncMutex<T> {
        // `Sync` if `T: Send`
        _marker: PhantomData<std::sync::Mutex<T>>,
    }

    pub struct AsyncMutexGuard<'a, T> {
        inner: &'a AsyncMutex<T>,
    }

    pub struct AsyncMutexGuardArc<T> {
        inner: Arc<AsyncMutex<T>>,
    }

    impl<T> AsyncMutex<T> {
        pub fn new(val: T) -> Self {
            missing_rt(val)
        }

        pub fn lock(&self) -> AsyncMutexGuard<T> {
            missing_rt(self)
        }
    }

    impl<T> Deref for AsyncMutexGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            missing_rt(self)
        }
    }

    impl<T> DerefMut for AsyncMutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            missing_rt(self)
        }
    }

    impl<T> Deref for AsyncMutexGuardArc<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            missing_rt(self)
        }
    }

    impl<T> DerefMut for AsyncMutexGuardArc<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            missing_rt(self)
        }
    }
}
