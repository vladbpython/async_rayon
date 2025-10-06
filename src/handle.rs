use super::{
    errors::SpawnError,
    result::SpawnResult,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll}
};
use tokio::{
    sync::oneshot,
    time::Duration,
};
use tokio_util::sync::CancellationToken;


pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug)]
pub struct Cancelled;


/// Handle на задачу с поддержкой отмены и timeout
pub struct JoinHandle<T> {
    cancel_token: CancellationToken,
    receiver: oneshot::Receiver<SpawnResult<T>>,
}

impl<T> JoinHandle<T> {

    pub fn new
    (
        cancel_token: CancellationToken,
        receiver: oneshot::Receiver<SpawnResult<T>>,
    ) -> Self {
        Self { 
            cancel_token, 
            receiver 
        }
    }
    
    #[inline]
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    #[inline(always)]
    pub async fn await_result(self) -> Result<SpawnResult<T>, Cancelled> {
        self.receiver.await.map_err(|_| Cancelled)
    }

    pub async fn await_timeout(self, timeout: Duration) -> SpawnResult<T> {
        match tokio::time::timeout(timeout, self.receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SpawnError::ChannelClosed),
            Err(_) => Err(SpawnError::Timeout),
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = SpawnResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.receiver).poll(cx) {
            Poll::Ready(res) => Poll::Ready(res.unwrap_or(Err(SpawnError::ChannelClosed))),
            Poll::Pending => Poll::Pending,
        }
    }
}
