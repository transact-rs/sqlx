use crate::connection::Connection;
use crate::database::Database;
use crate::pool::{Pool, PoolConnection, PoolOptions};
use crate::sync::AsyncOnceCell;
use cfg_if::cfg_if;
use pin_project_lite::pin_project;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct TestMasterPool<DB: Database> {
    inner: AsyncOnceCell<Inner<DB>>,
}

pub struct TestMasterConnection<DB: Database> {
    conn: PoolConnection<DB>,

    #[cfg(feature = "_rt-tokio")]
    handle: tokio::runtime::Handle,
}

struct Inner<DB: Database> {
    pool: Pool<DB>,

    #[cfg(feature = "_rt-tokio")]
    handle: tokio::runtime::Handle,
}

macro_rules! poll_with_handle(
    ($handle:expr, $fut:expr) => {
        PollWithHandle {
            fut: $fut,
            #[cfg(feature = "_rt-tokio")]
            handle: &$handle,
            #[cfg(not(feature = "_rt-tokio"))]
            _marker: std::marker::PhantomData,
        }
    }
);

impl<DB: Database> TestMasterPool<DB> {
    pub const fn new() -> Self {
        TestMasterPool {
            inner: AsyncOnceCell::const_new(),
        }
    }

    pub async fn connect(
        &self,
        opts: &<DB::Connection as Connection>::Options,
    ) -> crate::Result<TestMasterConnection<DB>> {
        self.inner
            .get_or_try_init::<_, _, crate::Error>(|| {
                let opts = opts.clone();

                async move {
                    #[cfg(feature = "_rt-tokio")]
                    let handle = spawn_test_runtime();

                    // Ensure this pool is linked to our master runtime so it can survive an individual
                    // test runtime shutting down.
                    let pool = poll_with_handle!(
                        handle,
                        PoolOptions::new()
                            // Tests don't need a master connection for very long
                            .max_connections(1)
                            .test_before_acquire(false)
                            .connect_with(opts)
                    )
                    .await?;

                    Ok(Inner {
                        pool,
                        #[cfg(feature = "_rt-tokio")]
                        handle,
                    })
                }
            })
            .await?
            .acquire()
            .await
    }

    /// # Panics
    /// If [`Self::connect()`] has not already completed successfully.
    pub async fn acquire(&self) -> crate::Result<TestMasterConnection<DB>> {
        self.inner
            .get()
            .expect("`TestMasterPool::connect()` has not been called")
            .acquire()
            .await
    }
}

impl<DB: Database> Deref for TestMasterConnection<DB> {
    type Target = PoolConnection<DB>;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl<DB: Database> DerefMut for TestMasterConnection<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl<DB: Database> Drop for TestMasterConnection<DB> {
    fn drop(&mut self) {
        cfg_if!(
            if #[cfg(feature = "_rt-tokio")] {
                self.handle.spawn(self.conn.release());
            } else {
                crate::rt::spawn(self.conn.release());
            }
        );
    }
}

impl<DB: Database> Inner<DB> {
    async fn acquire(&self) -> crate::Result<TestMasterConnection<DB>> {
        Ok(TestMasterConnection {
            // Ostensibly we only need to enter the runtime if the connection isn't already established
            conn: poll_with_handle!(self.handle, self.pool.acquire()).await?,
            #[cfg(feature = "_rt-tokio")]
            handle: self.handle.clone(),
        })
    }
}

// It's likely not advisable to hold an `EnterGuard` across an `.await` point,
// so we need to define an adapter that only enters the alternate runtime when it's polled.
#[cfg(feature = "_rt-tokio")]
pin_project! {
    struct PollWithHandle<'a, F> {
        #[pin]
        fut: F,
        handle: &'a tokio::runtime::Handle,
    }
}

#[cfg(not(feature = "_rt-tokio"))]
pin_project! {
    struct PollWithHandle<'a, F> {
        #[pin]
        fut: F,
        _marker: std::marker::PhantomData<&'a ()>,
    }
}

impl<F: Future> Future for PollWithHandle<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        #[cfg(feature = "_rt-tokio")]
        let _guard = this.handle.enter();

        this.fut.poll(cx)
    }
}

#[cfg(feature = "_rt-tokio")]
fn spawn_test_runtime() -> tokio::runtime::Handle {
    // Instead of forcing the `rt-multi-thread` feature on,
    // we just run a current-thread runtime in a background thread that we ourselves spawn.
    let rt = tokio::runtime::Builder::new_current_thread()
        .name("sqlx-test-master-pool")
        .enable_all()
        .build()
        .expect("failed to spawn master runtime");

    let handle = rt.handle().clone();

    std::thread::Builder::new()
        .name("sqlx-test-master-pool".into())
        .spawn(move || rt.block_on(std::future::pending::<()>()))
        .expect("failed to spawn thread for master runtime");

    handle
}
