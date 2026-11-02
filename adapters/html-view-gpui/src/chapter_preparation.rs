use async_channel::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use async_channel::Sender;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use futures::future::{Either, select};
use gpui::{App, BackgroundExecutor, Global};
use skrift_column_paginator::{BlockingJob, BlockingJobSpawner};
#[cfg(not(target_arch = "wasm32"))]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, OnceLock};
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::{pin::pin, time::Duration};

/// Shares one preparation worker across reader views, including the first
/// chapter loaded while opening a book. It retains no chapter results.
#[derive(Clone)]
pub struct ChapterPreparationWorker {
    #[cfg(not(target_arch = "wasm32"))]
    jobs: Arc<OnceLock<Sender<BlockingJob>>>,
    executor: BackgroundExecutor,
}

impl Global for ChapterPreparationWorker {}

impl ChapterPreparationWorker {
    pub fn for_app(cx: &mut App) -> Self {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::new(cx.background_executor()));
        }
        cx.global::<Self>().clone()
    }

    fn new(executor: &BackgroundExecutor) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            jobs: Arc::new(OnceLock::new()),
            executor: executor.clone(),
        }
    }

    /// Dropping the receiver skips work which has not started yet.
    pub fn prepare<T: Send + 'static>(
        &self,
        prepare: impl FnOnce() -> T + Send + 'static,
    ) -> Receiver<T> {
        let (send, receive) = async_channel::bounded(1);
        self.spawn(Box::new(move || {
            if !send.is_closed() {
                let _ = send.try_send(prepare());
            }
        }));
        receive
    }
}

impl BlockingJobSpawner for ChapterPreparationWorker {
    fn spawn(&self, job: BlockingJob) {
        #[cfg(not(target_arch = "wasm32"))]
        self.jobs
            .get_or_init(|| {
                let (jobs, receive) = async_channel::unbounded::<BlockingJob>();
                #[cfg(all(target_os = "linux", target_env = "gnu"))]
                let executor = self.executor.clone();
                self.executor
                    .spawn_dedicated(move |_| async move {
                        #[cfg(all(target_os = "linux", target_env = "gnu"))]
                        let mut reclaim_pending = false;
                        loop {
                            #[cfg(all(target_os = "linux", target_env = "gnu"))]
                            let next = if reclaim_pending {
                                // Give the UI time to install the prepared chapter and
                                // drop the previous one. New work cancels this wait.
                                match select(
                                    pin!(receive.recv()),
                                    pin!(executor.timer(Duration::from_secs(1))),
                                )
                                .await
                                {
                                    Either::Left((next, _)) => next,
                                    Either::Right(_) => {
                                        // SAFETY: malloc_trim has no pointer arguments
                                        // and glibc supports concurrent allocator use.
                                        // Keep a small top-chunk reserve for later work.
                                        unsafe { libc::malloc_trim(128 * 1024) };
                                        reclaim_pending = false;
                                        continue;
                                    }
                                }
                            } else {
                                receive.recv().await
                            };
                            #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
                            let next = receive.recv().await;
                            let Ok(job) = next else { break };
                            // A failed preparation disconnects its result channel;
                            // subsequent chapters must still have a working worker.
                            let _ = catch_unwind(AssertUnwindSafe(job));
                            #[cfg(all(target_os = "linux", target_env = "gnu"))]
                            {
                                reclaim_pending = true;
                            }
                        }
                    })
                    .detach();
                jobs
            })
            .try_send(job)
            .expect("chapter preparation worker stopped");
        #[cfg(target_arch = "wasm32")]
        self.executor.spawn(async move { job() }).detach();
    }
}
