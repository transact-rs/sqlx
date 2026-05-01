use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use cfg_if::cfg_if;
use pin_project_lite::pin_project;

#[cfg(feature = "_rt-async-io")]
pub mod rt_async_io;

#[cfg(feature = "_rt-tokio")]
pub mod rt_tokio;

#[derive(Debug, thiserror::Error)]
#[error("operation timed out")]
pub struct TimeoutError;

pub enum JoinHandle<T> {
    #[cfg(feature = "_rt-async-std")]
    AsyncStd(async_std::task::JoinHandle<T>),

    #[cfg(feature = "_rt-tokio")]
    Tokio(tokio::task::JoinHandle<T>),

    // Implementation shared by `smol` and `async-global-executor`
    #[cfg(feature = "_rt-async-task")]
    AsyncTask(Option<async_task::Task<T>>),

    // `PhantomData<T>` requires `T: Unpin`
    _Phantom(PhantomData<fn() -> T>),
}

pub async fn timeout<F: Future>(duration: Duration, f: F) -> Result<F::Output, TimeoutError> {
    #[cfg(debug_assertions)]
    let f = Box::pin(f);

    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return tokio::time::timeout(duration, f)
            .await
            .map_err(|_| TimeoutError);
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            rt_async_io::timeout(duration, f).await
        } else {
            missing_rt((duration, f))
        }
    }
}

pub async fn timeout_at<F: Future>(deadline: Instant, f: F) -> Result<F::Output, TimeoutError> {
    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return tokio::time::timeout_at(deadline.into(), f)
            .await
            .map_err(|_| TimeoutError);
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            rt_async_io::timeout_at(deadline, f).await
        } else {
            missing_rt((deadline, f))
        }
    }
}

pub async fn sleep(duration: Duration) {
    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return tokio::time::sleep(duration).await;
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            rt_async_io::sleep(duration).await
        } else {
            missing_rt(duration)
        }
    }
}

pub async fn sleep_until(instant: Instant) {
    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return tokio::time::sleep_until(instant.into()).await;
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            rt_async_io::sleep_until(instant).await
        } else {
            missing_rt(instant)
        }
    }
}

// https://github.com/taiki-e/pin-project-lite/issues/3
#[cfg(all(feature = "_rt-tokio", feature = "_rt-async-io"))]
pin_project! {
    #[project = IntervalProjected]
    pub enum Interval {
        Tokio {
            // Bespoke impl because `tokio::time::Interval` allocates when we could just pin instead
            #[pin]
            sleep: tokio::time::Sleep,
            period: Duration,
        },
        AsyncIo {
            #[pin]
            timer: async_io::Timer,
        },
    }
}

#[cfg(all(feature = "_rt-tokio", not(feature = "_rt-async-io")))]
pin_project! {
    #[project = IntervalProjected]
    pub enum Interval {
        Tokio {
            #[pin]
            sleep: tokio::time::Sleep,
            period: Duration,
        },
    }
}

#[cfg(all(not(feature = "_rt-tokio"), feature = "_rt-async-io"))]
pin_project! {
    #[project = IntervalProjected]
    pub enum Interval {
        AsyncIo {
            #[pin]
            timer: async_io::Timer,
        },
    }
}

#[cfg(not(any(feature = "_rt-tokio", feature = "_rt-async-io")))]
pub enum Interval {}

pub fn interval_after(period: Duration) -> Interval {
    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return Interval::Tokio {
            sleep: tokio::time::sleep(period),
            period,
        };
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            Interval::AsyncIo { timer: async_io::Timer::interval(period) }
        } else {
            missing_rt(period)
        }
    }
}

impl Interval {
    #[inline(always)]
    pub fn tick(mut self: Pin<&mut Self>) -> impl Future<Output = Instant> + use<'_> {
        std::future::poll_fn(move |cx| self.as_mut().poll_tick(cx))
    }

    #[inline(always)]
    pub fn as_timeout<F: Future>(self: Pin<&mut Self>, fut: F) -> AsTimeout<'_, F> {
        AsTimeout {
            interval: self,
            future: fut,
        }
    }

    #[inline(always)]
    pub fn poll_tick(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Instant> {
        cfg_if! {
            if #[cfg(any(feature = "_rt-tokio", feature = "_rt-async-io"))] {
                use std::task::ready;

                 match self.project() {
                    #[cfg(feature = "_rt-tokio")]
                    IntervalProjected::Tokio { mut sleep, period  } => {
                        ready!(sleep.as_mut().poll(cx));
                        let now = Instant::now();
                        sleep.reset((now + *period).into());
                        Poll::Ready(now)
                    }
                    #[cfg(feature = "_rt-async-io")]
                    IntervalProjected::AsyncIo { mut timer } => {
                        use futures_core::Stream;

                        Poll::Ready(ready!(timer
                            .as_mut()
                            .poll_next(cx))
                            .expect("BUG: `async_io::Timer::next()` should always yield"))
                    }
                }
            } else {
                unreachable!()
            }
        }
    }
}

pin_project! {
    pub struct AsTimeout<'i, F> {
        interval: Pin<&'i mut Interval>,
        #[pin]
        future: F,
    }
}

impl<F> Future for AsTimeout<'_, F>
where
    F: Future,
{
    type Output = Option<F::Output>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(out) = this.future.poll(cx) {
            return Poll::Ready(Some(out));
        }

        this.interval.as_mut().poll_tick(cx).map(|_| None)
    }
}

#[track_caller]
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "_rt-tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return JoinHandle::Tokio(handle.spawn(fut));
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-global-executor")] {
            JoinHandle::AsyncTask(Some(async_global_executor::spawn(fut)))
        } else if #[cfg(feature = "_rt-smol")] {
            JoinHandle::AsyncTask(Some(smol::spawn(fut)))
        } else if #[cfg(feature = "_rt-async-std")] {
            JoinHandle::AsyncStd(async_std::task::spawn(fut))
        } else {
            missing_rt(fut)
        }
    }
}

pub fn try_spawn<F>(fut: F) -> Option<JoinHandle<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "_rt-tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return Some(JoinHandle::Tokio(handle.spawn(fut)));
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-global-executor")] {
            Some(JoinHandle::AsyncTask(Some(async_global_executor::spawn(fut))))
        } else if #[cfg(feature = "_rt-smol")] {
            Some(JoinHandle::AsyncTask(Some(smol::spawn(fut))))
        } else if #[cfg(feature = "_rt-async-std")] {
            Some(JoinHandle::AsyncStd(async_std::task::spawn(fut)))
        } else {
            None
        }
    }
}

#[track_caller]
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    #[cfg(feature = "_rt-tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return JoinHandle::Tokio(handle.spawn_blocking(f));
    }

    cfg_if! {
        if #[cfg(feature = "_rt-async-global-executor")] {
            JoinHandle::AsyncTask(Some(async_global_executor::spawn_blocking(f)))
        } else if #[cfg(feature = "_rt-smol")] {
            JoinHandle::AsyncTask(Some(smol::unblock(f)))
        } else if #[cfg(feature = "_rt-async-std")] {
            JoinHandle::AsyncStd(async_std::task::spawn_blocking(f))
        } else {
            missing_rt(f)
        }
    }
}

pub async fn yield_now() {
    #[cfg(feature = "_rt-tokio")]
    if rt_tokio::available() {
        return tokio::task::yield_now().await;
    }

    // `smol`, `async-global-executor`, and `async-std` all have the same implementation for this.
    //
    // By immediately signaling the waker and then returning `Pending`,
    // this essentially just moves the task to the back of the runnable queue.
    //
    // There isn't any special integration with the runtime, so we can save code by rolling our own.
    //
    // (Tokio's implementation is nearly identical too,
    //  but has additional integration with `tracing` which may be useful for debugging.)
    let mut yielded = false;

    std::future::poll_fn(|cx| {
        if !yielded {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await
}

#[track_caller]
pub fn test_block_on<F: Future>(f: F) -> F::Output {
    cfg_if! {
        if #[cfg(feature = "_rt-async-io")] {
            async_io::block_on(f)
        } else if #[cfg(feature = "_rt-tokio")] {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to start Tokio runtime")
                .block_on(f)
        } else {
            missing_rt(f)
        }
    }
}

#[track_caller]
pub const fn missing_rt<T>(_unused: T) -> ! {
    if cfg!(feature = "_rt-tokio") {
        panic!("this functionality requires an active Tokio runtime")
    }

    panic!("one of the `runtime` features of SQLx must be enabled")
}

impl<T: Send + 'static> Future for JoinHandle<T> {
    type Output = T;

    #[track_caller]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            #[cfg(feature = "_rt-async-std")]
            Self::AsyncStd(handle) => Pin::new(handle).poll(cx),

            #[cfg(feature = "_rt-async-task")]
            Self::AsyncTask(task) => Pin::new(task)
                .as_pin_mut()
                .expect("BUG: task taken")
                .poll(cx),

            #[cfg(feature = "_rt-tokio")]
            Self::Tokio(handle) => Pin::new(handle)
                .poll(cx)
                .map(|res| res.expect("spawned task panicked")),

            Self::_Phantom(_) => {
                let _ = cx;
                unreachable!("runtime should have been checked on spawn")
            }
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        match self {
            // `async_task` cancels on-drop by default.
            // We need to explicitly detach to match Tokio and `async-std`.
            #[cfg(feature = "_rt-async-task")]
            Self::AsyncTask(task) => {
                if let Some(task) = task.take() {
                    task.detach();
                }
            }
            _ => (),
        }
    }
}
