//!
//! Pool of threads to run lightweight processes
//!
//! We spawn futures onto the pool with [spawn] method of global run queue or
//! with corresponding [Worker]'s spawn method.

use crate::thread_manager::{DynamicPoolManager, DynamicRunner};
use crate::worker;
use crossbeam_channel::{unbounded, Receiver, Sender};
use lazy_static::lazy_static;
use lightproc::lightproc::LightProc;
use lightproc::proc_stack::ProcStack;
use lightproc::recoverable_handle::RecoverableHandle;
use once_cell::sync::{Lazy, OnceCell};
use std::future::Future;
use std::iter::Iterator;
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};
#[cfg(feature = "runtime-tokio")]
use tokio::runtime;
use tracing::trace;

///
/// Spawn a process (which contains future + process stack) onto the executor from the global level.
///
/// # Example
/// ```rust
/// use bastion_executor::prelude::*;
/// use lightproc::prelude::*;
///
/// let pid = 1;
/// let stack = ProcStack::default().with_pid(pid);
///
/// let handle = spawn(
///     async {
///         panic!("test");
///     },
///     stack.clone(),
/// );
///
/// run(
///     async {
///         handle.await;
///     },
///     stack.clone(),
/// );
/// ```
pub fn spawn<F, T>(future: F, stack: ProcStack) -> RecoverableHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (task, handle) = LightProc::recoverable(future, worker::schedule, stack);
    task.schedule();
    handle
}

/// Spawns a blocking task.
///
/// The task will be spawned onto a thread pool specifically dedicated to blocking tasks.
pub fn spawn_blocking<F, R>(future: F, stack: ProcStack) -> RecoverableHandle<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let (task, handle) = LightProc::recoverable(future, schedule, stack);
    task.schedule();
    handle
}

///
/// Acquire the static Pool reference
#[inline]
pub fn get() -> &'static Pool {
    &*POOL
}

impl Pool {
    ///
    /// Spawn a process (which contains future + process stack) onto the executor via [Pool] interface.
    pub fn spawn<F, T>(&self, future: F, stack: ProcStack) -> RecoverableHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Log this `spawn` operation.
        let _child_id = stack.get_pid() as u64;
        let _parent_id = worker::get_proc_stack(|t| t.get_pid() as u64).unwrap_or(0);

        let (task, handle) = LightProc::recoverable(future, worker::schedule, stack);
        task.schedule();
        handle
    }
}

/// Enqueues work, attempting to send to the thread pool in a
/// nonblocking way and spinning up needed amount of threads
/// based on the previous statistics without relying on
/// if there is not a thread ready to accept the work or not.
pub(crate) fn schedule(t: LightProc) {
    if let Err(err) = POOL.sender.try_send(t) {
        // We were not able to send to the channel without
        // blocking.
        POOL.sender.send(err.into_inner()).unwrap();
    }
    // Add up for every incoming scheduled task
    DYNAMIC_POOL_MANAGER.get().unwrap().increment_frequency();
}

///
/// Low watermark value, defines the bare minimum of the pool.
/// Spawns initial thread set.
/// Can be configurable with env var `BASTION_BLOCKING_THREADS` at runtime.
#[inline]
fn low_watermark() -> &'static u64 {
    lazy_static! {
        static ref LOW_WATERMARK: u64 = {
            env::var_os("BASTION_BLOCKING_THREADS")
                .map(|x| x.to_str().unwrap().parse::<u64>().unwrap())
                .unwrap_or(DEFAULT_LOW_WATERMARK)
        };
    }

    &*LOW_WATERMARK
}

/// If low watermark isn't configured this is the default scaler value.
/// This value is used for the heuristics of the scaler
const DEFAULT_LOW_WATERMARK: u64 = 2;

/// Pool interface between the scheduler and thread pool
#[derive(Debug)]
pub struct Pool {
    sender: Sender<LightProc>,
    receiver: Receiver<LightProc>,
}

struct AsyncRunner {}

impl DynamicRunner for AsyncRunner {
    fn run_static(&self, park_timeout: Duration) -> ! {
        #[cfg(feature = "runtime-tokio")]
        {
            let thread_runtime = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("static thread: couldn't spawn tokio runtime");
            thread_runtime.block_on(async move { self._static_loop(park_timeout) })
        }
        #[cfg(not(feature = "runtime-tokio"))]
        {
            self._static_loop(park_timeout)
        }
    }
    fn run_dynamic(&self, parker: &dyn Fn()) -> ! {
        #[cfg(feature = "runtime-tokio")]
        {
            let thread_runtime = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("dynamic thread: couldn't spawn tokio runtime");
            thread_runtime.block_on(async move { self._dynamic_loop(parker) })
        }
        #[cfg(not(feature = "runtime-tokio"))]
        {
            self._dynamic_loop(parker)
        }
    }
    fn run_standalone(&self) {
        #[cfg(feature = "runtime-tokio")]
        {
            let thread_runtime = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("standalone thread: couldn't spawn tokio runtime");
            thread_runtime.block_on(async move { self._standalone() })
        }
        #[cfg(not(feature = "runtime-tokio"))]
        {
            self._standalone()
        }
        self._standalone()
    }
}

impl AsyncRunner {
    fn _static_loop(&self, park_timeout: Duration) -> ! {
        loop {
            for task in &POOL.receiver {
                trace!("static: running task");
                task.run();
            }

            trace!("static: empty queue, parking with timeout");
            thread::park_timeout(park_timeout);
        }
    }
    fn _dynamic_loop(&self, parker: &dyn Fn()) -> ! {
        loop {
            while let Ok(task) = POOL.receiver.try_recv() {
                trace!("dynamic thread: running task");
                task.run();
            }
            trace!(
                "dynamic thread: parking - {:?}",
                std::thread::current().id()
            );
            parker();
        }
    }
    fn _standalone(&self) {
        while let Ok(task) = POOL.receiver.try_recv() {
            task.run();
        }
        trace!("standalone thread: quitting.");
    }
}

static DYNAMIC_POOL_MANAGER: OnceCell<DynamicPoolManager> = OnceCell::new();

static POOL: Lazy<Pool> = Lazy::new(|| {
    let runner = Arc::new(AsyncRunner {});

    DYNAMIC_POOL_MANAGER
        .set(DynamicPoolManager::new(*low_watermark() as usize, runner))
        .expect("couldn't create dynamic pool manager");
    DYNAMIC_POOL_MANAGER
        .get()
        .expect("couldn't get static pool manager")
        .initialize();

    let (sender, receiver) = unbounded();
    Pool { sender, receiver }
});
