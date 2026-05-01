use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    #[project = RaceProject]
    pub struct Race<L, R> {
        #[pin]
        left: L,
        #[pin]
        right: R,
    }
}

impl<L, R> Future for Race<L, R>
where
    L: Future,
    R: Future,
{
    type Output = Result<L::Output, R::Output>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        if let Poll::Ready(left) = this.left.as_mut().poll(cx) {
            return Poll::Ready(Ok(left));
        }

        this.right.as_mut().poll(cx).map(Err)
    }
}

#[inline(always)]
pub fn race<L, R>(left: L, right: R) -> Race<L, R> {
    Race { left, right }
}
