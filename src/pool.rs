use super::{
    errors::SpawnError,
    result::SpawnResult,
    handle::{
        Task,
        JoinHandle,
    },
    model::{
        PoolMetrics,
        ScopeMetrics,
        JoinOrdering,
    },
};
use std::{
    future::Future,
    pin::Pin, 
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    }, 
};
use crossbeam::{
    deque::{Injector, Stealer, Worker},
    queue::ArrayQueue,
};
use tokio::{
    sync::{oneshot, Notify, Semaphore},
    time::Duration,
};
use futures::{
    FutureExt,
    stream::{
        FuturesOrdered,
        FuturesUnordered, 
        StreamExt
    }
};
use tokio_util::sync::CancellationToken;


/// Конфигурация пула потоков
#[derive(Debug, Clone)]
pub struct Config {
    pub num_threads: usize,
    pub max_pending: Option<usize>,
    pub enable_work_stealing: bool,
    pub task_timeout: Option<Duration>,
    pub enable_metrics: bool,
}

impl Default for Config {
    fn default() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus * 2, // Для I/O-bound задач
            max_pending: Some(num_cpus * 20),
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(30)),
            enable_metrics: true,
        }
    }
}

impl Config {
    pub fn cpu_bound() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus,
            max_pending: Some(num_cpus * 10),
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(60)),
            enable_metrics: true,
        }
    }

    pub fn io_bound() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus * 2,
            max_pending: None,
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(30)),
            enable_metrics: true
        }
    }

    pub fn io_bound_fast() -> Self{
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus * 2,
            max_pending: None,
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(30)),
            enable_metrics: false
        }
    }
}


pub type ThreadPool = Arc<ThreadPoolInner>;

#[inline(always)]
fn unlikely(b: bool) -> bool {
    #[cold]
    fn cold() {}
    if !b { cold() }
    b
}

// Основной пул потоков с оптимизациями для высоких нагрузок
/// Основной пул потоков с оптимизациями для высоких нагрузок
pub struct ThreadPoolInner {
   // Очереди для CPU-bound работы (spawn_blocking)
    global_queue: Arc<ArrayQueue<Task>>,
    local_queues: Vec<Arc<Injector<Task>>>,
    stealers: Vec<Stealer<Task>>,
    
    next_worker: AtomicUsize,
    
    global_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    
    // Счетчики для мониторинга
    active_spawned_tasks: Arc<AtomicUsize>,
    total_spawned: Arc<AtomicUsize>,
    completed_tasks: Arc<AtomicUsize>,
    failed_tasks: Arc<AtomicUsize>,
    all_spawned_tasks_completed: Arc<Notify>,
    idle_workers: Arc<AtomicUsize>,
    queued_tasks: Arc<AtomicUsize>,
    config: Config,
}

impl ThreadPoolInner {
    pub fn new(num_threads: usize, max_pending: Option<usize>) -> ThreadPool {
        let config = Config {
            num_threads,
            max_pending,
            ..Default::default()
        };
        Self::with_config(config)
    }

    pub fn ref_config(&self) -> &Config {
        &self.config
    }

    #[inline(always)]
    fn push_task(&self, task: Task) -> Result<(), Task> {
        match self.global_queue.push(task) {
            Ok(()) => {
                self.queued_tasks.fetch_add(1, Ordering::Relaxed);
                if unlikely(self.idle_workers.load(Ordering::Relaxed) > 0) {
                    self.global_notify.notify_one();
                }
                Ok(())
            }
            Err(task) => {
                let worker_id = self.next_worker.fetch_add(1, Ordering::Relaxed) 
                    % self.local_queues.len();
                self.local_queues[worker_id].push(task);
                
                if unlikely(self.idle_workers.load(Ordering::Relaxed) > 0) {
                    self.global_notify.notify_one();
                }
                Ok(())
            }
        }
    }

    pub fn with_config(config: Config) -> ThreadPool {
        let capacity = config.max_pending.unwrap_or(10_000);
        let global_queue = Arc::new(ArrayQueue::new(capacity));
        let global_notify = Arc::new(Notify::new());
        let cancellation_token = CancellationToken::new();
        let active_spawned_tasks = Arc::new(AtomicUsize::new(0));
        let total_spawned = Arc::new(AtomicUsize::new(0));
        let completed_tasks = Arc::new(AtomicUsize::new(0));
        let failed_tasks = Arc::new(AtomicUsize::new(0));
        let all_spawned_tasks_completed = Arc::new(Notify::new());
        let idle_workers = Arc::new(AtomicUsize::new(0));
        let queued_tasks = Arc::new(AtomicUsize::new(0));
        let next_worker = AtomicUsize::new(0);

        let mut local_queues = Vec::with_capacity(config.num_threads);
        let mut workers = Vec::with_capacity(config.num_threads);
        let mut stealers = Vec::with_capacity(config.num_threads);
        
        for _ in 0..config.num_threads {
            let injector = Arc::new(Injector::new());
            let worker = Worker::new_fifo();
            
            stealers.push(worker.stealer());
            local_queues.push(injector);
            workers.push(worker);
        }

        let pool = Arc::new(ThreadPoolInner {
            global_queue,
            local_queues,
            stealers,
            next_worker,
            global_notify: global_notify.clone(),
            cancellation_token: cancellation_token.clone(),
            active_spawned_tasks: active_spawned_tasks.clone(),
            total_spawned: total_spawned.clone(),
            completed_tasks: completed_tasks.clone(),
            failed_tasks: failed_tasks.clone(),
            all_spawned_tasks_completed: all_spawned_tasks_completed.clone(),
            idle_workers: idle_workers.clone(),
            queued_tasks: queued_tasks.clone(),
            config,
        });

        let mut worker_handles = Vec::new();
        // Воркеры для CPU bound задач
        for (worker_id, worker) in workers.into_iter().enumerate() {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                pool_clone.worker_loop(worker_id, worker).await;
            });
            worker_handles.push(handle);
        }

        pool
    }

    
    async fn worker_loop(&self, worker_id: usize, local: Worker<Task>) {
        let enable_stealing = self.config.enable_work_stealing;
        let num_workers = self.local_queues.len();
        let my_injector = &self.local_queues[worker_id];
        
        let mut fast_random_state = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        
        let batch_size: usize = 16;
        let mut completed_batch = 0;
        let mut steal_attempts = 0;
        
        'outer: loop {
            if completed_batch >= batch_size {
                if self.cancellation_token.is_cancelled() {
                    break;
                }
                completed_batch = 0;
            }
            
            let task_opt = local.pop()
                .or_else(|| my_injector.steal().success())
                .or_else(|| self.global_queue.pop())
                .or_else(|| {
                    if !enable_stealing || num_workers <= 1 {
                        return None;
                    }
                    
                    let max_attempts = if steal_attempts < 5 {
                        2
                    } else if steal_attempts < 20 {
                        1
                    } else {
                        0
                    };
                    
                    for _ in 0..max_attempts {
                        fast_random_state ^= fast_random_state << 13;
                        fast_random_state ^= fast_random_state >> 7;
                        fast_random_state ^= fast_random_state << 17;
                        
                        let target_id = (fast_random_state as usize) % num_workers;
                        
                        if target_id != worker_id {
                            if let Some(task) = self.stealers[target_id].steal().success() {
                                steal_attempts = 0;
                                return Some(task);
                            }
                        }
                    }
                    
                    steal_attempts += 1;
                    None
                });

            if let Some(task) = task_opt {
                task.await;
                completed_batch += 1;
                steal_attempts = 0;
                
                let prev = self.active_spawned_tasks.fetch_sub(1, Ordering::Release);
                if unlikely(prev == 1) {
                    if self.active_spawned_tasks.load(Ordering::Acquire) == 0 {
                        self.all_spawned_tasks_completed.notify_waiters();
                    }
                }
            } else {
                self.idle_workers.fetch_add(1, Ordering::Release);
                
                if !local.is_empty() 
                    || !my_injector.is_empty() 
                    || !self.global_queue.is_empty() 
                {
                    self.idle_workers.fetch_sub(1, Ordering::Acquire);
                    steal_attempts = 0;
                    continue 'outer;
                }
                
                tokio::select! {
                    _ = self.global_notify.notified() => {
                        self.idle_workers.fetch_sub(1, Ordering::Acquire);
                        steal_attempts = 0;
                    }
                    _ = self.cancellation_token.cancelled() => {
                        self.idle_workers.fetch_sub(1, Ordering::Acquire);
                        break 'outer;
                    }
                }
                
                completed_batch = 0;
            }
        }
    }

    #[inline]
    pub fn metrics(&self) -> PoolMetrics {
        PoolMetrics {
            active_tasks: self.active_spawned_tasks.load(Ordering::Relaxed),
            idle_workers: self.idle_workers.load(Ordering::Relaxed),
            queued_tasks: self.queued_tasks.load(Ordering::Relaxed),
            total_spawned: self.total_spawned.load(Ordering::Relaxed),
            completed_tasks: self.completed_tasks.load(Ordering::Relaxed),
            failed_tasks: self.failed_tasks.load(Ordering::Relaxed),
        }
    }

    #[inline]
    pub async fn join_all(&self) {
        while self.active_spawned_tasks.load(Ordering::SeqCst) > 0 {
            self.all_spawned_tasks_completed.notified().await;
        }
    }

    pub async fn join_all_timeout(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.join_all()).await.is_ok()
    }

    pub async fn shutdown(&self) {
        self.join_all().await;
        self.cancellation_token.cancel();
    }

    pub async fn shutdown_timeout(&self, timeout: Duration) -> bool {
        if !self.join_all_timeout(timeout).await {
            self.cancellation_token.cancel();
            return false;
        }
        self.cancellation_token.cancel();
        true
    }

    pub async fn scope_async<T, F, Fut>(self: &Arc<Self>, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(Arc<Scope>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let scope = Arc::new(Scope::new(self.clone()));
        let fut = f(scope.clone());
        let out = fut.await;
        scope.wait().await;
        out
    }

    /// Мониторинг метрик с callback
    /// ВАЖНО: Вызовите token.cancel() для остановки мониторинга и освобождения памяти
    pub fn start_monitoring<F>(self: &Arc<Self>, interval: Duration, callback: F) -> CancellationToken
    where
        F: Fn(PoolMetrics) + Send + 'static,
    {
        let pool = Arc::clone(self);
        let token = CancellationToken::new();
        let token_clone = token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        let metrics = pool.metrics();
                        callback(metrics);
                    }
                    _ = token_clone.cancelled() => {
                        drop(pool);
                        break;
                    }
                }
            }
        });

        token
    }

    /// Остановить мониторинг и дропнуть все ссылки
    pub fn stop_monitoring(token: CancellationToken) {
        token.cancel();
    }
}


/// Scope для асинхронного управления задачами с полным контролем
/// Поддерживает cancellation, получение результатов, обработку ошибок
pub struct Scope {
    counter: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    pool: ThreadPool,
    completed: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
}

impl Scope {
    pub fn new(pool: ThreadPool) -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
            pool,
            completed: Arc::new(AtomicUsize::new(0)),
            failed: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[inline]
    pub fn spawn<T, F>(&self, fut: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        self.spawn_with_handle(fut)
    }

    #[inline]
    pub fn spawn_with_handle<T, Fut>(&self, fut: Fut) -> JoinHandle<T>
    where
        T: Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        self.counter.fetch_add(1, Ordering::Relaxed);
        
        let counter = self.counter.clone();
        let notify = self.notify.clone();
        let completed = self.completed.clone();
        let failed = self.failed.clone();
        let enable_metrics = self.pool.config.enable_metrics;

        // Прямой tokio::spawn для async I/O
        tokio::spawn(async move {
            let result: SpawnResult<T> = tokio::select! {
                _ = cancel_clone.cancelled() => Err(SpawnError::Cancelled),
                res = std::panic::AssertUnwindSafe(fut).catch_unwind() => {
                    res.map_err(|e| SpawnError::Panic(format!("{:?}", e)))
                }
            };

            if enable_metrics {
                if result.is_ok() {
                    completed.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }

            let _ = tx.send(result);

            if counter.fetch_sub(1, Ordering::Release) == 1 {
                notify.notify_one();
            }
        });

        JoinHandle::new(cancel_token, rx)
    }

    pub fn spawn_blocking_with_handle<T, F>(&self, f: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        self.counter.fetch_add(1, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(1, Ordering::Relaxed);
        
        let counter = self.counter.clone();
        let notify = self.notify.clone();
        let completed = self.completed.clone();
        let failed = self.failed.clone();
        let pool_completed = self.pool.completed_tasks.clone();
        let pool_failed = self.pool.failed_tasks.clone();
        let enable_metrics = self.pool.config.enable_metrics;
        
        if enable_metrics {
            self.pool.total_spawned.fetch_add(1, Ordering::Relaxed);
        }

        let task: Task = Box::pin(async move {
            let result: SpawnResult<T> = tokio::select! {
                _ = cancel_clone.cancelled() => Err(SpawnError::Cancelled),
                val = tokio::task::spawn_blocking(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                }) => {
                    match val {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(e)) => Err(SpawnError::Panic(format!("{:?}", e))),
                        Err(e) => Err(SpawnError::JoinFailed(e.to_string())),
                    }
                }
            };

            if enable_metrics {
                if result.is_ok() {
                    completed.fetch_add(1, Ordering::Relaxed);
                    pool_completed.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed.fetch_add(1, Ordering::Relaxed);
                    pool_failed.fetch_add(1, Ordering::Relaxed);
                }
            }

            let _ = tx.send(result);
            
            if counter.fetch_sub(1, Ordering::Release) == 1 {
                notify.notify_one();
            }
        });

        let _ = self.pool.push_task(task);

        JoinHandle::new(cancel_token, rx)
    }

    /// Ультра-быстрый spawn для простых async задач
    pub async fn spawn_bulk_compute<T, R, F, Fut>(
        &self,
        items: Vec<T>,
        compute: F,
        ordering: JoinOrdering,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = R> + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }

        // Используем spawn для каждой async задачи
        let handles: Vec<_> = items.into_iter()
            .map(|item| {
                let compute = compute.clone();
                self.spawn(async move { compute(item).await })
            })
            .collect();

        self.join_handles(handles,ordering).await
    }

    /// Ультра-быстрый spawn для простых cpu bound задач
    pub async fn spawn_bulk_compute_cpu<T, R, F>(
        &self,
        items: Vec<T>,
        compute: F,
        ordering: JoinOrdering,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + Clone + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }

        let compute = Arc::new(compute);
        
        // Используем spawn_blocking_with_handle для каждого элемента
        let handles: Vec<_> = items.into_iter()
            .map(|item| {
                let comp = Arc::clone(&compute);
                self.spawn_blocking_with_handle(move || comp(item))
            })
            .collect();

        self.join_handles(handles,ordering).await
    }


    /// Оптимизированная пакетная обработка
    pub async fn batch_process<T, R, F, Fut>(
        &self,
        items: Vec<T>,
        max_concurrent: usize,
        processor: F,
        ordering: JoinOrdering,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = R> + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }

        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        let processor = Arc::new(processor);
        
        let handles: Vec<_> = items.into_iter()
            .map(|item| {
                let sem = Arc::clone(&semaphore);
                let proc = Arc::clone(&processor);
                
                self.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(item).await
                })
            })
            .collect();
        
        self.join_handles(handles,ordering).await
    }

    /// Оптимизированная пакетная обработка CPU
    pub async fn batch_process_cpu<T, R, F>(
        &self,
        items: Vec<T>,
        chunk_size: usize,
        processor: F,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + Clone + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }
        
        let processor = Arc::new(processor);
        let mut chunk_handles = Vec::new();
        let mut items = items;
        
        // Разбиваем на чанки и обрабатываем
        while !items.is_empty() {
            let take = chunk_size.min(items.len());
            let chunk: Vec<T> = items.drain(..take).collect();
            let proc = Arc::clone(&processor);
            
            // Используем spawn_blocking_with_handle
            let handle = self.spawn_blocking_with_handle(move || {
                chunk.into_iter()
                    .map(|item| proc(item))
                    .collect::<Vec<R>>()
            });
            
            chunk_handles.push(handle);
        }
        
        // Flatten результаты
        let mut all_results = Vec::with_capacity(items.len());
        
        for handle in chunk_handles {
            match handle.await {
                Ok(chunk_results) => {
                    for result in chunk_results {
                        all_results.push(Ok(result));
                    }
                }
                Err(e) => {
                    for _ in 0..chunk_size {
                        all_results.push(Err(e.clone()));
                    }
                }
            }
        }
        
        all_results
    }

    /// Оптимизированная потоковая обработка
        pub async fn stream_process<T, R, F, Fut>(
        &self,
        items: Vec<T>,
        max_concurrent: usize,
        processor: F,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }

        let processor = Arc::new(processor);
        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        
        type TaskFuture<R> = Pin<Box<dyn Future<Output = SpawnResult<R>> + Send>>;
        
        let mut futures: FuturesUnordered<TaskFuture<R>> = FuturesUnordered::new();
        let mut results = Vec::with_capacity(items.len());
        let mut items_iter = items.into_iter();
        
        for _ in 0..max_concurrent {
            if let Some(item) = items_iter.next() {
                let proc = Arc::clone(&processor);
                let sem = Arc::clone(&semaphore);
                
                let fut = self.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(item).await
                });
                
                futures.push(Box::pin(fut) as TaskFuture<R>);
            } else {
                break;
            }
        }
        
        while let Some(result) = futures.next().await {
            results.push(result);
            
            if let Some(item) = items_iter.next() {
                let proc = Arc::clone(&processor);
                let sem = Arc::clone(&semaphore);
                
                let fut = self.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(item).await
                });
                
                futures.push(Box::pin(fut) as TaskFuture<R>);
            }
        }
        
        results
    }

    /// Оптимизированный параллельный map
    pub async fn par_map_async<T, U, F, Fut>(
        &self,
        items: &[T],
        f: F,
        ordering: JoinOrdering,
    ) -> Vec<SpawnResult<U>>
    where
        T: Sync,
        U: Send + 'static,
        F: Fn(&T) -> Fut + Sync + Send + 'static,
        Fut: Future<Output = U> + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }
        
        // Создаем все задачи через spawn
        let handles: Vec<_> = items.iter()
            .map(|item| self.spawn(f(item)))
            .collect();
        
        // Ждем результаты
        self.join_handles(handles,ordering).await
    }

    async fn collect_chunk_results<U>(
        &self,
        chunk_handles: Vec<JoinHandle<Vec<U>>>,
        chunk_size: usize,
        total: usize,
    ) -> Vec<SpawnResult<U>>
    where
        U: Send + 'static,
    {
        let mut all_results = Vec::with_capacity(total);
        
        for handle in chunk_handles {
            match handle.await {
                Ok(chunk_results) => {
                    for result in chunk_results {
                        all_results.push(Ok(result));
                    }
                }
                Err(e) => {
                    for _ in 0..chunk_size.min(total - all_results.len()) {
                        all_results.push(Err(e.clone()));
                    }
                }
            }
        }
        
        all_results
    }

    /// Оптимизированный параллельный map cpu bound
        pub async fn par_map_cpu<T, U, F>(
        &self,
        mut items: Vec<T>,
        chunk_size: usize,
        f: F,
    ) -> Vec<SpawnResult<U>>
    where
        T: Send + 'static,
        U: Send + 'static,
        F: Fn(T) -> U + Sync + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }
        
        let total = items.len();
        let f = Arc::new(f);
        let mut chunk_handles = Vec::new();
        
        while !items.is_empty() {
            let take = chunk_size.min(items.len());
            let chunk: Vec<T> = items.drain(..take).collect();
            let func = Arc::clone(&f);
            
            let handle = self.spawn_blocking_with_handle(move || {
                chunk.into_iter()
                    .map(|item| func(item))
                    .collect::<Vec<U>>()
            });
            
            chunk_handles.push(handle);
        }
        
        self.collect_chunk_results(chunk_handles, chunk_size, total).await
    }

    pub async fn wait(&self) {
        while self.counter.load(Ordering::Acquire) > 0 {
            self.notify.notified().await;
        }
    }

    pub async fn wait_timeout(&self, dur: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + dur;
        while self.counter.load(Ordering::Acquire) > 0 {
            if tokio::time::timeout_at(deadline, self.notify.notified()).await.is_err() {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn cancel_all<T>(&self, handles: &[JoinHandle<T>]) {
        for h in handles {
            h.cancel();
        }
    }

    async fn join_handles_ordered<T>(
        &self, 
        handles: Vec<JoinHandle<T>>,
    ) ->  Vec<SpawnResult<T>>{
        let len = handles.len();
        let mut futures = FuturesOrdered::from_iter(handles);
        let mut results = Vec::with_capacity(len);
        
        while let Some(result) = futures.next().await {
            results.push(result);
        }
        results
    }

    async fn join_handles_unordered<T>(
        &self, 
        handles: Vec<JoinHandle<T>>,
    ) ->  Vec<SpawnResult<T>>{
        let len = handles.len();
        let mut futures = FuturesUnordered::from_iter(handles);
        let mut results = Vec::with_capacity(len);
        
        while let Some(result) = futures.next().await {
            results.push(result);
        }
        results
    }

    /// Оптимизированное ожидание handles
    #[inline]
    pub async fn join_handles<T>(
        &self, 
        handles: Vec<JoinHandle<T>>,
        ordering: JoinOrdering,
    ) -> Vec<SpawnResult<T>>
    where
        T: Send + 'static,
    {
        if handles.is_empty() {
            return Vec::new();
        }

        match ordering {
            JoinOrdering::Ordered => self.join_handles_ordered(handles).await,
            JoinOrdering::UnOrdered => self.join_handles_unordered(handles).await
            
        }
    
    }

    #[inline]
    pub fn metrics(&self) -> ScopeMetrics {
        ScopeMetrics {
            pending: self.counter.load(Ordering::Acquire),
            completed: self.completed.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }


    /// Мониторинг метрик с callback
    /// ВАЖНО: Вызовите token.cancel() для остановки мониторинга и освобождения памяти
    pub fn start_monitoring<F>(&self, interval: Duration, callback: F) -> CancellationToken
    where
        F: Fn(ScopeMetrics) + Send + 'static,
    {
        let counter = self.counter.clone();
        let completed = self.completed.clone();
        let failed = self.failed.clone();
        
        let token = CancellationToken::new();
        let token_clone = token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        let metrics = ScopeMetrics {
                            pending: counter.load(Ordering::Relaxed),
                            completed: completed.load(Ordering::Relaxed),
                            failed: failed.load(Ordering::Relaxed),
                        };
                        callback(metrics);
                    }
                    _ = token_clone.cancelled() => {
                        break;
                    }
                }
            }
        });
        token
    }

    /// Остановить мониторинг и дропнуть все ссылки
    pub fn stop_monitoring(token: CancellationToken) {
        token.cancel();
    }
}