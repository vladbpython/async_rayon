use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use num_cpus;
use crossbeam::deque::{Injector, Stealer, Worker};
use tokio::sync::{oneshot, Notify};
use tokio_util::sync::CancellationToken;

pub struct ThreadPoolInner {
    inject: Arc<Injector<Task>>,
    global_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    active_spawned_tasks: Arc<AtomicUsize>,
    all_spawned_tasks_completed: Arc<Notify>,
}

pub type ThreadPool = Arc<ThreadPoolInner>;

// Task is a boxed FnOnce closure that can be sent between threads and has a 'static lifetime.
// This is primarily for CPU-bound or blocking tasks that shouldn't block Tokio's runtime.
type Task = Box<dyn FnOnce() + Send + 'static>;

impl ThreadPoolInner {
    pub fn new(num_threads: usize) -> ThreadPool {
        let inject = Arc::new(Injector::new());
        let mut local_workers = Vec::new();
        let mut stealers_for_pool = Vec::new();
        let global_notify = Arc::new(Notify::new());
        let cancellation_token = CancellationToken::new();

        let active_spawned_tasks = Arc::new(AtomicUsize::new(0));
        let all_spawned_tasks_completed = Arc::new(Notify::new());

        for _ in 0..num_threads {
            let worker = Worker::new_fifo();
            stealers_for_pool.push(worker.stealer());
            local_workers.push(worker);
        }

        let pool_arc = Arc::new(ThreadPoolInner {
            inject,
            global_notify: global_notify.clone(),
            cancellation_token: cancellation_token.clone(),
            active_spawned_tasks: active_spawned_tasks.clone(),
            all_spawned_tasks_completed: all_spawned_tasks_completed.clone(),
        });

        for _ in 0..num_threads {
            let worker = local_workers.pop().expect("Should have enough workers");

            let stealers: Vec<_> = stealers_for_pool
                .iter()
                .filter_map(|s| Some(s.clone()))
                .collect();

            // Worker loop for the ThreadPool is now dedicated to FnOnce tasks.
            // These FnOnce tasks could potentially run tokio::task::spawn_blocking
            // if they contain blocking I/O, but generally they are CPU-bound.
            tokio::spawn(ThreadPoolInner::worker_loop(
                worker,
                stealers,
                pool_arc.inject.clone(),
                global_notify.clone(),
                cancellation_token.clone(),
                active_spawned_tasks.clone(),
                all_spawned_tasks_completed.clone(),
            ));
        }

        pool_arc
    }

    pub fn new_cpu() -> ThreadPool{
        Self::new(num_cpus::get_physical())
    }

    pub fn new_vcpu() -> ThreadPool {
        Self::new(num_cpus::get())
    }

    async fn worker_loop(
        worker: Worker<Task>,
        stealers: Vec<Stealer<Task>>,
        inject: Arc<Injector<Task>>,
        global_notify: Arc<Notify>,
        cancellation_token: CancellationToken,
        active_spawned_tasks: Arc<AtomicUsize>,
        all_spawned_tasks_completed: Arc<Notify>,
    ) {
        loop {
            // Check for cancellation early in the loop
            if cancellation_token.is_cancelled() {
                break;
            }

            let task = worker.pop()
                .or_else(|| inject.steal().success())
                .or_else(|| {
                    for s in &stealers {
                        if let Some(t) = s.steal().success() {
                            return Some(t);
                        }
                    }
                    None
                });

            if let Some(task) = task {
                // Execute the blocking/CPU-bound task
                task();

                // Decrement task counter after execution
                if active_spawned_tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
                    all_spawned_tasks_completed.notify_waiters();
                }
            } else {
                // If no task found, wait for a notification or cancellation
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        break; // Exit loop if cancelled
                    }
                    _ = global_notify.notified() => {
                        // A task was probably added, continue loop to try fetching
                    }
                }
            }
        }
    }

    // This method is for spawning *blocking* or *CPU-bound* tasks onto the ThreadPool
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        // Increment task counter BEFORE pushing the task
        self.active_spawned_tasks.fetch_add(1, Ordering::Relaxed); // Relaxed is often sufficient for increments
        self.inject.push(Box::new(f));
        self.global_notify.notify_waiters();
    }

    pub async fn scope_async<T, F, Fut>(self: &Arc<Self>, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(Arc<Scope>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        // Scope now owns a reference to the ThreadPool (self.clone())
        let scope = Arc::new(Scope::new(self.clone()));
        let fut = f(scope.clone());
        let out = fut.await;
        scope.wait().await;
        out
    }

    pub async fn shutdown(self: &Arc<Self>) {
        self.join_all_spawned().await;
        self.cancellation_token.cancel();
    }

    pub async fn join_all_spawned(self: &Arc<Self>) {
        // Use Acquire for load to ensure visibility of increments
        while self.active_spawned_tasks.load(Ordering::Acquire) > 0 {
            self.all_spawned_tasks_completed.notified().await;
        }
    }
}

pub struct Scope {
    counter: Arc<AtomicUsize>, // Tracks active tasks in this scope
    notify: Arc<Notify>,       // Notifies `wait()` when tasks complete
    // The pool is kept for consistency, but `Scope` now spawns `Future`s directly with `tokio::spawn`
    // If you explicitly needed to pass blocking tasks from `Scope` to `ThreadPool`, you would use `self.pool.spawn(...)`.
    _pool: ThreadPool, // Renamed to _pool to indicate it's not directly used for spawning futures
}

impl Scope {
    fn new(pool: ThreadPool) -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
            _pool: pool, // Store the pool if other methods need to access it
        }
    }

    // Spawns an async Future directly onto the Tokio runtime
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_with_handle(fut);
    }

    // Spawns an async Future directly onto the Tokio runtime and returns a JoinHandle
    pub fn spawn_with_handle<T, Fut>(&self, fut: Fut) -> JoinHandle<T>
    where
        T: Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let cancel_token = CancellationToken::new();
        let ct = cancel_token.clone();

        self.counter.fetch_add(1, Ordering::Relaxed); // Relaxed for simple increment

        let counter = self.counter.clone();
        let notify = self.notify.clone();

        // Directly spawn the async Future on the Tokio runtime
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = ct.cancelled() => {
                    None
                }
                val = fut => {
                    Some(val)
                }
            };

            if let Some(val) = result {
                let _ = tx.send(val);
            }

            if counter.fetch_sub(1, Ordering::Release) == 1 { // Release for decrement
                notify.notify_waiters();
            }
        });

        JoinHandle {
            cancel_token,
            receiver: rx,
        }
    }

    /// Spawns a blocking task onto Tokio's dedicated blocking thread pool.
    /// Returns a JoinHandle that can be awaited asynchronously.
    pub fn task_spawn_blocking<T, F>(&self, f: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let cancel_token = CancellationToken::new();
        let ct = cancel_token.clone();

        self.counter.fetch_add(1, Ordering::Relaxed);

        let counter = self.counter.clone();
        let notify = self.notify.clone();

        tokio::spawn(async move {
            // The fix is here: we don't bind `tokio::task::spawn_blocking(f)` to a variable
            // outside of the select. Instead, we await it directly in one of the branches.
            let result = tokio::select! {
                _ = ct.cancelled() => {
                    // This branch means we were cancelled.
                    // We can't meaningfully "abort" the blocking task from here if it's already running.
                    // Instead, we just don't await its result or send it.
                    None
                }
                val = tokio::task::spawn_blocking(f) => { // Await the spawned blocking task directly here
                    match val {
                        Ok(res) => Some(res),
                        Err(e) => {
                            // The blocking task might have panicked.
                            eprintln!("Blocking task panicked or was cancelled by Tokio: {:?}", e);
                            None
                        }
                    }
                }
            };

            if let Some(val) = result {
                let _ = tx.send(val);
            }

            if counter.fetch_sub(1, Ordering::Release) == 1 {
                notify.notify_waiters();
            }
        });

        JoinHandle {
            cancel_token,
            receiver: rx,
        }
    }

    pub async fn wait(&self) {
        while self.counter.load(Ordering::Acquire) > 0 { // Acquire for load
            self.notify.notified().await;
        }
    }

    pub async fn join<T, Fut>(&self, futures: impl IntoIterator<Item = Fut>) -> Vec<T>
    where
        T: Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let handles: Vec<JoinHandle<T>> = futures
            .into_iter()
            .map(|fut| self.spawn_with_handle(fut))
            .collect();

        // Collect the await_result futures
        let results_futures: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.await_result())
            .collect();

        // Await all of them concurrently
        let collected_results = futures::future::join_all(results_futures).await;

        collected_results
            .into_iter()
            .map(|res| res.expect("one or more tasks were cancelled or failed"))
            .collect()
    }

    pub async fn join_handles<T>(&self, handles: Vec<JoinHandle<T>>) -> Vec<T>
    where
        T: Send + 'static,
    {
        let results = futures::future::join_all(
            handles.into_iter().map(|h| h.await_result()),
        )
        .await;

        results.into_iter().filter_map(Result::ok).collect()
    }

    pub async fn par_map_async<T, U, F, Fut>(&self, items: &[T], f: F) -> Vec<U>
    where
        T: Sync,
        U: Send + 'static,
        F: Fn(&T) -> Fut + Sync + Send + 'static,
        Fut: Future<Output = U> + Send + 'static,
    {
        let handles: Vec<JoinHandle<U>> = items
            .iter()
            .map(|item| self.spawn_with_handle(f(item)))
            .collect();

        let results_futures: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.await_result())
            .collect();

        let collected_results = futures::future::join_all(results_futures).await;

        collected_results
            .into_iter()
            .map(|res| res.expect("task cancelled in par_map_async")) // Or handle Result::Err
            .collect()
    }
}

pub struct JoinHandle<T> {
    cancel_token: CancellationToken,
    receiver: oneshot::Receiver<T>,
}

impl<T> JoinHandle<T> {
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    pub async fn await_result(self) -> Result<T, Cancelled> {
        self.receiver.await.map_err(|_| Cancelled)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cancelled;

#[cfg(test)]
mod tests {
    use super::*; 
    use futures::FutureExt;
    use tokio::sync::Mutex; 
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use std::time::Duration; 

    #[tokio::test]
    async fn test_thread_pool_spawn_basic() {
        let pool = ThreadPoolInner::new(2); 
        let counter = Arc::new(AtomicUsize::new(0));

        let c1 = counter.clone();
        pool.spawn(move || {
            c1.fetch_add(1, Ordering::SeqCst);
            println!("Task 1 executed.");
        });

        let c2 = counter.clone();
        pool.spawn(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            println!("Task 2 executed.");
        });

        // Use `tokio::join!` to wait for both tasks to explicitly signal completion.
        // This is the most reliable way to ensure synchronous tasks spawned by `pool.spawn`
        // have run before asserting. 
        pool.shutdown().await;
        assert_eq!(counter.load(Ordering::SeqCst), 2, "Both tasks should have executed.");
    }

    #[tokio::test]
    async fn test_thread_pool_scope_async_basic() {
        let pool = ThreadPoolInner::new(2);
        let result_counter = Arc::new(AtomicUsize::new(0));

        // FIX: Create a clone *before* the `move` closure takes ownership of `result_counter`.
        // This `local_counter_ref` will be used for the assertion *after* the scope completes.
        let local_counter_ref = result_counter.clone(); 

        // Now, `move |scope|` will move the original `result_counter` (Arc) into the closure.
        // The `local_counter_ref` remains accessible in this test function.
        let final_value = pool.scope_async(move |scope| { 
            // Inside this closure, `result_counter` refers to the `Arc` that was just moved in.
            // Cloning it further for `spawn` calls is still correct.
            let r_clone = result_counter.clone(); 
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                r_clone.fetch_add(10, Ordering::SeqCst);
            });

            let r_clone2 = result_counter.clone(); // This is also a clone of the `Arc` inside the closure
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                r_clone2.fetch_add(20, Ordering::SeqCst);
            });

            async {
                42 
            }
        }).await;

        pool.shutdown().await;
        // Use `local_counter_ref` (the clone that stayed in this function) for assertion.
        assert_eq!(local_counter_ref.load(Ordering::SeqCst), 30, "Scope tasks should have summed correctly.");
        assert_eq!(final_value, 42, "scope_async should return the correct value.");

        
    }


    #[tokio::test]
    async fn test_scope_wait_functionality() {
        let pool = ThreadPoolInner::new(1);
        let task_tracker = Arc::new(AtomicUsize::new(0));
        let local_tracker = task_tracker.clone();

        // `scope_async` ensures waiting for all tasks within it.
        let _  = pool.scope_async(move |scope| {
            let tracker_clone1 = local_tracker.clone();
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await; 
                tracker_clone1.fetch_add(1, Ordering::SeqCst);
            });

            let tracker_clone2 = local_tracker.clone();
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await; 
                tracker_clone2.fetch_add(1, Ordering::SeqCst);
            });

            async {
                () 
            }
        }).await; 
        pool.shutdown().await;    
        assert_eq!(task_tracker.load(Ordering::SeqCst), 2, "All tasks within scope should have completed.");  
    }

    #[tokio::test]
    async fn test_join_functionality() {
        let pool = ThreadPoolInner::new(2);

        // `scope.join` explicitly waits for its constituent futures.
        let result = pool.scope_async(|scope| async move {
            scope.join(vec![
                    async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        100
                    }.boxed(),
                    async {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        200
                    }.boxed()
                ]
            ).await
        }).await;
        pool.shutdown().await;
        let val1 = *result.get(0).unwrap_or(&0);
        let val2 = *result.get(1).unwrap_or(&0);
        assert_eq!(val1, 100, "First joined task should return correct value.");
        assert_eq!(val2, 200, "Second joined task should return correct value.");
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn test_many_tasks(){

        let mut tasks = Vec::new();
        for n in 0..1_000_000{
            tasks.push(async move{
                n
            });
        }
        let count_tasks = tasks.len();

        let pool = ThreadPoolInner::new_vcpu();
        let result = pool.scope_async(move |scope| async move{
            scope.join(tasks).await

        }).await;

        assert_eq!(count_tasks,result.len());


    }

    #[tokio::test]
    async fn test_par_map_async_functionality() {
        let pool = ThreadPoolInner::new(4);
        let items = vec![1, 2, 3, 4, 5];
        let shared_items = items.clone();

        // `par_map_async` collects and awaits all results.
        let results = pool.scope_async(move |scope| async move{
            scope.par_map_async(&items, |&item| async move {
                tokio::time::sleep(Duration::from_millis(10 * item as u64)).await; 
                item * 2 
            }).await
        }).await;

        let mut expected_results = shared_items.iter().map(|&item| item * 2).collect::<Vec<_>>();
        expected_results.sort(); 
        let mut actual_results = results;
        actual_results.sort();
        pool.shutdown().await;
        assert_eq!(actual_results, expected_results, "par_map_async should transform all items correctly.");

        
    }

    #[tokio::test]
    async fn test_join_handle_cancellation() {
        let pool = ThreadPoolInner::new(1);
        let task_ran_counter = Arc::new(AtomicUsize::new(0));
        let shared_task_ran_counter = task_ran_counter.clone();

        // `JoinHandle` allows explicit awaiting or cancellation.
        let result = pool.scope_async(move |scope| {
            let tracker = shared_task_ran_counter.clone();
            let handle = scope.spawn_with_handle(async move {
                tokio::time::sleep(Duration::from_secs(1)).await; 
                tracker.fetch_add(1, Ordering::SeqCst); 
                "completed"
            });

            async move {
                handle.cancel(); 
                let res = handle.await_result().await;
                res
            }
        }).await;
        pool.shutdown().await;
        assert!(result.is_err(), "Task should have been cancelled.");
        assert_eq!(result.unwrap_err(), Cancelled, "Error should be 'Cancelled'.");
        assert_eq!(task_ran_counter.load(Ordering::SeqCst), 0, "Cancelled task should not have incremented counter.");

        
    }

    #[tokio::test]
    async fn test_pool_shutdown() {
        let pool = ThreadPoolInner::new(1);
        let task_count = Arc::new(AtomicUsize::new(0));

        // Get a clone of the pool's cancellation token to pass into the task
        let pool_cancellation_token = pool.cancellation_token.clone(); 

        let (tx_started, rx_started) = oneshot::channel();

        let tc_clone = task_count.clone();
        pool.spawn(move || {
            // We are already on a Tokio runtime thread because worker_loop is tokio::spawn'd.
            // So, we don't need `rt.block_on` here.
            // Instead, we spawn an async task that respects the pool's cancellation token.
            let local_cancellation_token = pool_cancellation_token; // Move the token into this closure

            tokio::spawn(async move {
                let _ = tx_started.send(()); // Signal that the async portion *started*

                tokio::select! {
                    _ = local_cancellation_token.cancelled() => {
                        println!("Long task cancelled!"); // Diagnostic
                        // Task was cancelled, do not increment count
                    }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        // Task completed normally without cancellation
                        tc_clone.fetch_add(1, Ordering::SeqCst);
                        println!("Long task completed!"); // Diagnostic
                    }
                }
            });
        });

        // Wait for the long task to signal it has started its async block.
        rx_started.await.expect("Long task should have started its async block.");

        pool.shutdown().await; // This will trigger cancellation

        // Give a very small buffer for cancellation to propagate and workers to exit.
        tokio::time::sleep(Duration::from_millis(10)).await; 

        assert_eq!(task_count.load(Ordering::SeqCst), 0, "Long task should have been cancelled or not finished after shutdown.");

        let tc_clone2 = task_count.clone();
        let (tx_after_shutdown, mut rx_after_shutdown) = oneshot::channel(); 
        pool.spawn(move || {
            // This task should not run because the pool is shutting down.
            // If it runs, it means the pool's shutdown is not effective at preventing new tasks.
            tc_clone2.fetch_add(100, Ordering::SeqCst); 
            let _ = tx_after_shutdown.send(()); 
        });
        
        tokio::time::sleep(Duration::from_millis(10)).await; 
        assert!(rx_after_shutdown.try_recv().is_err(), "Task spawned after shutdown should not run.");
        assert_eq!(task_count.load(Ordering::SeqCst), 0, "Tasks spawned after shutdown should not run.");
    }

    #[tokio::test]
    async fn test_scope_with_arc_references() {
        let pool = ThreadPoolInner::new(4);
        
        let shared_sum = Arc::new(Mutex::new(0)); 
        let numbers = vec![1, 2, 3, 4, 5];

        let numbers_for_assertion = numbers.clone();
        let shared_sum_for_assertion = shared_sum.clone();

        let result_from_scope = pool.scope_async(move |scope| async move { 
            let shared_sum_clone_for_outer_scope = shared_sum.clone(); 

            scope.par_map_async(&numbers, move |&item| { 
                let s_sum_clone_for_task = shared_sum_clone_for_outer_scope.clone(); 
                async move { 
                    tokio::time::sleep(Duration::from_millis(10 * item as u64)).await;
                    let mut sum = s_sum_clone_for_task.lock().await; 
                    *sum += item; 
                    item * 10 // <-- This is the actual transformation
                }
            }).await
        }).await;

        let final_sum = *shared_sum_for_assertion.lock().await; 
        assert_eq!(final_sum, numbers_for_assertion.iter().sum(), "The total sum should be correct.");

        // FIX: Calculate expected_results with `item * 10` to match the actual task transformation
        let mut expected_results = numbers_for_assertion.iter().map(|&item| item * 10).collect::<Vec<_>>();
        expected_results.sort(); 
        
        let mut actual_results = result_from_scope; 
        actual_results.sort();
        pool.shutdown().await;
        assert_eq!(actual_results, expected_results, "par_map_async results should be correct.");

        
    }

    #[tokio::test]
    async fn test_scope_with_immutable_arc_reference() {
        let pool = ThreadPoolInner::new(2);
        
        let shared_data = Arc::new(vec!["apple".to_string(), "banana".to_string(), "cherry".to_string()]);

        let results = pool.scope_async(|scope| async move {
            let data_clone = shared_data.clone(); 

            scope.par_map_async(&[0, 1, 2], move |&index| { 
                let d_clone_for_task = data_clone.clone(); 
                async move { 
                    format!("Fruit at index {}: {}", index, d_clone_for_task[index])
                }
            }).await
        }).await;

        let mut expected_results = vec![
            "Fruit at index 0: apple".to_string(),
            "Fruit at index 1: banana".to_string(),
            "Fruit at index 2: cherry".to_string(),
        ];
        expected_results.sort();
        let mut actual_results = results;
        actual_results.sort();
        pool.shutdown().await;
        assert_eq!(actual_results, expected_results, "Access to shared immutable data should be correct.");

        
    }
}
