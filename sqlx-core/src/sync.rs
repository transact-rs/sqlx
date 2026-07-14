use cfg_if::cfg_if;

// For types with identical signatures that don't require runtime support,
// we can just arbitrarily pick one to use based on what's enabled.
//
// We'll generally lean towards Tokio's types as those are more featureful
// (including `tokio-console` support) and more widely deployed.

pub struct AsyncSemaphore {
    // We use the semaphore from futures-intrusive as the one from async-lock
    // is missing the ability to add arbitrary permits, and is not guaranteed to be fair:
    // * https://github.com/smol-rs/async-lock/issues/22
    // * https://github.com/smol-rs/async-lock/issues/23
    //
    // We're on the look-out for a replacement, however, as futures-intrusive is not maintained
    // and there are some soundness concerns (although it turns out any intrusive future is unsound
    // in MIRI due to the necessitated mutable aliasing):
    // https://github.com/launchbadge/sqlx/issues/1668
    #[cfg(all(
        any(
            feature = "_rt-async-global-executor",
            feature = "_rt-async-std",
            feature = "_rt-smol"
        ),
        not(feature = "_rt-tokio")
    ))]
    inner: futures_intrusive::sync::Semaphore,

    #[cfg(feature = "_rt-tokio")]
    inner: tokio::sync::Semaphore,
}

impl AsyncSemaphore {
    #[track_caller]
    pub fn new(fair: bool, permits: usize) -> Self {
        if cfg!(not(any(
            feature = "_rt-async-global-executor",
            feature = "_rt-async-std",
            feature = "_rt-smol",
            feature = "_rt-tokio"
        ))) {
            crate::rt::missing_rt((fair, permits));
        }

        AsyncSemaphore {
            #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))]
            inner: futures_intrusive::sync::Semaphore::new(fair, permits),
            #[cfg(feature = "_rt-tokio")]
            inner: {
                debug_assert!(fair, "Tokio only has fair permits");
                tokio::sync::Semaphore::new(permits)
            },
        }
    }

    pub fn permits(&self) -> usize {
        cfg_if! {
            if #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))] {
                self.inner.permits()
            } else if #[cfg(feature = "_rt-tokio")] {
                self.inner.available_permits()
            } else {
                crate::rt::missing_rt(())
            }
        }
    }

    pub async fn acquire(&self, permits: u32) -> AsyncSemaphoreReleaser<'_> {
        cfg_if! {
            if #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))] {
                AsyncSemaphoreReleaser {
                    inner: self.inner.acquire(permits as usize).await,
                }
            } else if #[cfg(feature = "_rt-tokio")] {
                AsyncSemaphoreReleaser {
                    inner: self
                        .inner
                        // Weird quirk: `tokio::sync::Semaphore` mostly uses `usize` for permit counts,
                        // but `u32` for this and `try_acquire_many()`.
                        .acquire_many(permits)
                        .await
                        .expect("BUG: we do not expose the `.close()` method"),
                }
            } else {
                crate::rt::missing_rt(permits)
            }
        }
    }

    pub fn try_acquire(&self, permits: u32) -> Option<AsyncSemaphoreReleaser<'_>> {
        cfg_if! {
            if #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))] {
                Some(AsyncSemaphoreReleaser {
                    inner: self.inner.try_acquire(permits as usize)?,
                })
            } else if #[cfg(feature = "_rt-tokio")] {
                Some(AsyncSemaphoreReleaser {
                    inner: self.inner.try_acquire_many(permits).ok()?,
                })
            } else {
                crate::rt::missing_rt(permits)
            }
        }
    }

    pub fn release(&self, permits: usize) {
        cfg_if! {
            if #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))] {
                self.inner.release(permits);
            } else if #[cfg(feature = "_rt-tokio")] {
                self.inner.add_permits(permits);
            } else {
                crate::rt::missing_rt(permits);
            }
        }
    }
}

pub struct AsyncSemaphoreReleaser<'a> {
    // We use the semaphore from futures-intrusive as the one from async-std
    // is missing the ability to add arbitrary permits, and is not guaranteed to be fair:
    // * https://github.com/smol-rs/async-lock/issues/22
    // * https://github.com/smol-rs/async-lock/issues/23
    //
    // We're on the look-out for a replacement, however, as futures-intrusive is not maintained
    // and there are some soundness concerns (although it turns out any intrusive future is unsound
    // in MIRI due to the necessitated mutable aliasing):
    // https://github.com/launchbadge/sqlx/issues/1668
    #[cfg(all(
        any(
            feature = "_rt-async-global-executor",
            feature = "_rt-async-std",
            feature = "_rt-smol"
        ),
        not(feature = "_rt-tokio")
    ))]
    inner: futures_intrusive::sync::SemaphoreReleaser<'a>,

    #[cfg(feature = "_rt-tokio")]
    inner: tokio::sync::SemaphorePermit<'a>,

    #[cfg(not(any(
        feature = "_rt-async-global-executor",
        feature = "_rt-async-std",
        feature = "_rt-smol",
        feature = "_rt-tokio"
    )))]
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl AsyncSemaphoreReleaser<'_> {
    pub fn disarm(self) {
        cfg_if! {
            if #[cfg(all(
                any(
                    feature = "_rt-async-global-executor",
                    feature = "_rt-async-std",
                    feature = "_rt-smol"
                ),
                not(feature = "_rt-tokio")
            ))] {
                let mut this = self;
                this.inner.disarm();
            } else if #[cfg(feature = "_rt-tokio")] {
                self.inner.forget();
            } else {
                crate::rt::missing_rt(());
            }
        }
    }
}
