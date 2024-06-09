//! `tokio_with_wasm` is a Rust library that enables
//! seamless integration of asynchronous Rust tasks
//! and `Future`s in JavaScript,
//! resembling the familiar `tokio::task` patterns.
//! It leverages web workers to execute tasks in parallel,
//! making it ideal for high-performance web applications.

mod pool;

use futures_channel::oneshot;
use pool::*;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use wasm_bindgen::prelude::*;

thread_local! {
    pub(crate) static WORKER_POOL: WorkerPool = {
        let worker_pool=WorkerPool::new();
        spawn(manage_pool());
        worker_pool
    }
}

/// Manages the worker pool by periodically checking for
/// inactive web workers and queued tasks.
async fn manage_pool() {
    loop {
        WORKER_POOL.with(|worker_pool| {
            worker_pool.remove_inactive_workers();
            worker_pool.flush_queued_tasks();
        });
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            crate::common::set_timeout(&resolve, 100.0);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

/// Spawns a new asynchronous task, returning a
/// [`JoinHandle`] for it.
///
/// The provided future will start running in the JavaScript event loop
/// when `spawn` is called, even if you don't await the returned
/// `JoinHandle`.
///
/// Spawning a task enables the task to execute concurrently to other tasks. The
/// spawned task will always execute on the current web worker(thread),
/// as that's how JavaScript's `Promise` basically works.
///
/// # Examples
///
/// In this example, a server is started and `spawn` is used to start a new task
/// that processes each received connection.
///
/// ```no_run
/// use std::io;
///
/// async fn process() -> io::Result<()> {
///     // Some process...
/// }
///
/// async fn start() -> io::Result<()> {
///     let result = tokio_with_wasm::spawn(async move {
///         // Process this job concurrently.
///         process(socket).await
///     }).await?;;
/// }
/// ```
///
/// To run multiple tasks in parallel and receive their results, join
/// handles can be stored in a vector.
/// ```
/// # async fn start() {
/// async fn my_background_op(id: i32) -> String {
///     let s = format!("Starting background task {}.", id);
///     println!("{}", s);
///     s
/// }
///
/// let ops = vec![1, 2, 3];
/// let mut tasks = Vec::with_capacity(ops.len());
/// for op in ops {
///     // This call will make them start running in the background
///     // immediately.
///     tasks.push(tokio_with_wasm::spawn(my_background_op(op)));
/// }
///
/// let mut outputs = Vec::with_capacity(tasks.len());
/// for task in tasks {
///     match task.await {
///         Ok(output) => outputs.push(output),
///         Err(err) => {
///             println!("An error occurred: {}", err);
///         }
///     }
/// }
/// println!("{:?}", outputs);
/// # }
/// ```
/// This example pushes the tasks to `outputs` in the order they were
/// started in.
///
/// # Using `!Send` values from a task
///
/// The task supplied to `spawn` is not required to implement `Send`.
/// This is different from multi-threaded native async runtimes,
/// because JavaScript environment is inherently single-threaded.
///
/// For example, this will work:
///
/// ```
/// use std::rc::Rc;
///
/// fn use_rc(rc: Rc<()>) {
///     // Do stuff w/ rc
/// # drop(rc);
/// }
///
/// async fn start() {
///     tokio_with_wasm::spawn(async {
///         // Force the `Rc` to stay in a scope with no `.await`
///         {
///             let rc = Rc::new(());
///             use_rc(rc.clone());
///         }
///
///         tokio_with_wasm::yield_now().await;
///     }).await;
/// }
/// ```
///
/// This will work too, unlike multi-threaded native runtimes
/// where `!Send` values cannot live across `.await`:
///
/// ```
/// use std::rc::Rc;
///
/// fn use_rc(rc: Rc<()>) {
///     // Do stuff w/ rc
/// # drop(rc);
/// }
///
/// async fn start() {
///     tokio_with_wasm::spawn(async {
///         let rc = Rc::new(());
///
///         tokio_with_wasm::yield_now().await;
///
///         use_rc(rc.clone());
///     }).await;
/// }
/// ```
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: std::future::Future<Output = T> + 'static,
    T: 'static,
{
    let (join_sender, join_receiver) = oneshot::channel();
    let (cancel_sender, cancel_receiver) = oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let output = tokio::select! {
            returned = future => Ok(returned),
            _ = cancel_receiver => Err(JoinError { cancelled: true }),
        };
        let _ = join_sender.send(output);
    });
    JoinHandle {
        join_receiver,
        cancel_sender: Some(cancel_sender),
    }
}

/// Runs the provided closure on a web worker(thread) where blocking is acceptable.
///
/// In general, issuing a blocking call or performing a lot of compute in a
/// future without yielding is problematic, as it may prevent the JavaScript runtime from
/// driving other futures forward. This function runs the provided closure on a
/// web worker dedicated to blocking operations.
///
/// More and more web workers will be spawned when they are requested through this
/// function until the upper limit of 512 is reached.
/// After reaching the upper limit, the tasks will wait for
/// any of the web workers to become idle.
/// When a web worker remains idle for 10 seconds, it will be terminated
/// and get removed from the worker pool, which is a similiar behavior to that of `tokio`.
/// The web worker limit is very large by default, because `spawn_blocking` is often
/// used for various kinds of IO operations that cannot be performed
/// asynchronously.  When you run CPU-bound code using `spawn_blocking`, you
/// should keep this large upper limit in mind.
///
/// This function is intended for non-async operations that eventually finish on
/// their own. Because web workers do not share memory like threads do,
/// synchronization primitives such as mutex, channels, and global static variables
/// might not work as expected. Each web worker is completely isolated
/// because that's how the web works.
///
/// # Examples
///
/// Pass an input value and receive result of computation:
///
/// ```
/// # async fn start() -> Result<(), Box<dyn std::error::Error>>{
/// // Initial input
/// let mut data = "Hello, ".to_string();
/// let output = tokio_with_wasm::spawn_blocking(move || {
///     // Stand-in for compute-heavy work or using synchronous APIs
///     data.push_str("world");
///     // Pass ownership of the value back to the asynchronous context
///     data
/// }).await?;
///
/// // `output` is the value returned from the thread
/// assert_eq!(output.as_str(), "Hello, world");
/// Ok(())
/// }
/// ```
pub fn spawn_blocking<C, T>(callable: C) -> JoinHandle<T>
where
    C: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (join_sender, join_receiver) = oneshot::channel();
    let (returned_sender, returned_receiver) = oneshot::channel();
    let (cancel_sender, cancel_receiver) = oneshot::channel();
    WORKER_POOL.with(|worker_pool| {
        worker_pool.queue_task(move || {
            let returned = callable();
            let _ = returned_sender.send(returned);
        })
    });
    wasm_bindgen_futures::spawn_local(async move {
        let output = tokio::select! {
            received = returned_receiver => {
                match received{
                    Ok(inner) => Ok(inner),
                    Err(_) => Err(JoinError { cancelled : false})
                }
            },
            _ = cancel_receiver => Err(JoinError { cancelled: true }),
        };
        let _ = join_sender.send(output);
    });
    JoinHandle {
        join_receiver,
        cancel_sender: Some(cancel_sender),
    }
}

/// Yields execution back to the JavaScript event loop.
///
/// To avoid blocking inside a long-running function,
/// you have to yield to the async event loop regularly.
///
/// The async task may resume when it has its turn back.
/// Meanwhile, any other pending tasks will be scheduled
/// by the JavaScript runtime.
pub async fn yield_now() {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = queueMicrotask)]
        fn queue_microtask(callback: &js_sys::Function);
    }
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        queue_microtask(&resolve);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// An owned permission to join on a task (awaiting its termination).
///
/// This can be thought of as the equivalent of
/// [`std::thread::JoinHandle`] or `tokio::task::JoinHandle` for
/// a task that is executed concurrently.
///
/// A `JoinHandle` *detaches* the associated task when it is dropped, which
/// means that there is no longer any handle to the task, and no way to `join`
/// on it.
///
/// This struct is created by the [crate::spawn] and [crate::spawn_blocking]
/// functions.
///
/// # Examples
///
/// Creation from [`crate::spawn`]:
///
/// ```
/// # async fn start() {
/// let join_handle: tokio_with_wasm::JoinHandle<_> = tokio_with_wasm::spawn(async {
///     // some work here
/// });
/// # }
/// ```
///
/// Creation from [`crate::spawn_blocking`]:
///
/// ```
/// # async fn start() {
/// let join_handle: tokio_with_wasm::JoinHandle<_> = tokio_with_wasm::spawn_blocking(|| {
///     // some blocking work here
/// });
/// # }
/// ```
///
/// Child being detached and outliving its parent:
///
/// ```no_run
/// # async fn start() {
/// let original_task = tokio_with_wasm::spawn(async {
///     let _detached_task = tokio_with_wasm::spawn(async {
///         // Here we sleep to make sure that the first task returns before.
///         // Assume that code takes a few seconds to execute here.
///         // This will be called, even though the JoinHandle is dropped.
///         println!("♫ Still alive ♫");
///     });
/// });
///
/// original_task.await;
/// println!("Original task is joined.");
/// # }
/// ```
pub struct JoinHandle<T> {
    join_receiver: oneshot::Receiver<Result<T, JoinError>>,
    cancel_sender: Option<oneshot::Sender<()>>,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}
impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if let Ok(received) = self.join_receiver.try_recv() {
            if let Some(payload) = received {
                Poll::Ready(payload)
            } else {
                let waker = context.waker().clone();
                let wake_future = async move {
                    waker.wake();
                };
                wasm_bindgen_futures::spawn_local(wake_future);
                Poll::Pending
            }
        } else {
            Poll::Ready(Err(JoinError { cancelled: false }))
        }
    }
}

impl<T> fmt::Debug for JoinHandle<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("JoinHandle").finish()
    }
}

impl<T> JoinHandle<T> {
    pub fn abort(&mut self) {
        let option = self.cancel_sender.take();
        if let Some(cancel_sender) = option {
            let _ = cancel_sender.send(());
        }
    }
}

/// Returned when a task failed to execute to completion.
#[derive(Debug)]
pub struct JoinError {
    cancelled: bool,
}

impl fmt::Display for JoinError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("task failed to execute to completion")
    }
}

impl std::error::Error for JoinError {}

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        return self.cancelled;
    }
}
