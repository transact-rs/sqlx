use crate::ext::future::race;
use crate::rt::TimeoutError;
use std::{
    future::Future,
    time::{Duration, Instant},
};

pub async fn sleep(duration: Duration) {
    async_io::Timer::after(duration).await;
}

pub async fn sleep_until(deadline: Instant) {
    async_io::Timer::at(deadline).await;
}

pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, TimeoutError> {
    race(future, sleep(duration))
        .await
        .map_err(|_| TimeoutError)
}

pub async fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, TimeoutError> {
    race(future, sleep_until(deadline))
        .await
        .map_err(|_| TimeoutError)
}
