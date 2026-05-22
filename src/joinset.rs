use futures_util::stream::{FuturesUnordered, StreamExt};
use smol::Task;
use std::future::Future;

pub struct JoinSet<T> {
    tasks: FuturesUnordered<Task<T>>,
}

impl<T> JoinSet<T> {
    pub fn new() -> Self {
        Self {
            tasks: FuturesUnordered::new(),
        }
    }

    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.tasks.push(smol::spawn(future));
    }

    pub async fn join_next(&mut self) -> Option<T> {
        self.tasks.next().await
    }
}

impl<T> Default for JoinSet<T> {
    fn default() -> Self {
        Self::new()
    }
}