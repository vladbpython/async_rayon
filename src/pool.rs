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
use crossbeam::deque::{Injector, Stealer, Worker};
use tokio::{
    sync::{oneshot, Notify, Semaphore},
    time::Duration,
};
use futures::{
    FutureExt,
    stream::{FuturesUnordered, StreamExt}
};
use tokio_util::sync::CancellationToken;


/// Конфигурация пула потоков
#[derive(Debug, Clone)]
pub struct Config {
    pub num_threads: usize,
    pub max_pending: Option<usize>,
    pub enable_work_stealing: bool,
    pub task_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus * 2, // Для I/O-bound задач
            max_pending: Some(num_cpus * 20),
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(30)),
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
        }
    }

    pub fn io_bound() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            num_threads: num_cpus * 2,
            max_pending: None,
            enable_work_stealing: true,
            task_timeout: Some(Duration::from_secs(30)),
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
    inject: Arc<Injector<Task>>,
    global_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    active_spawned_tasks: Arc<AtomicUsize>,
    total_spawned: Arc<AtomicUsize>,
    completed_tasks: Arc<AtomicUsize>,
    failed_tasks: Arc<AtomicUsize>,
    all_spawned_tasks_completed: Arc<Notify>,
    semaphore: Option<Arc<Semaphore>>,
    idle_workers: Arc<AtomicUsize>,
    stealers: Vec<Stealer<Task>>,
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

    #[inline(always)]
    fn push_task(&self, task: Task) {
        self.queued_tasks.fetch_add(1, Ordering::Relaxed);
        self.inject.push(task);
        
        if unlikely(self.idle_workers.load(Ordering::Relaxed) > 0) {
            self.global_notify.notify_one();
        }
    }

    pub fn with_config(config: Config) -> ThreadPool {
        let inject = Arc::new(Injector::new());
        let global_notify = Arc::new(Notify::new());
        let cancellation_token = CancellationToken::new();
        let active_spawned_tasks = Arc::new(AtomicUsize::new(0));
        let total_spawned = Arc::new(AtomicUsize::new(0));
        let completed_tasks = Arc::new(AtomicUsize::new(0));
        let failed_tasks = Arc::new(AtomicUsize::new(0));
        let all_spawned_tasks_completed = Arc::new(Notify::new());
        let idle_workers = Arc::new(AtomicUsize::new(0));
        let queued_tasks = Arc::new(AtomicUsize::new(0));
        let semaphore = config.max_pending.map(|mp| Arc::new(Semaphore::new(mp)));

        let mut workers = Vec::new();
        let mut stealers = Vec::new();
        
        for _ in 0..config.num_threads {
            let w = Worker::new_fifo();
            stealers.push(w.stealer());
            workers.push(w);
        }

        let pool = Arc::new(ThreadPoolInner {
            inject: inject.clone(),
            global_notify: global_notify.clone(),
            cancellation_token: cancellation_token.clone(),
            active_spawned_tasks: active_spawned_tasks.clone(),
            total_spawned: total_spawned.clone(),
            completed_tasks: completed_tasks.clone(),
            failed_tasks: failed_tasks.clone(),
            all_spawned_tasks_completed: all_spawned_tasks_completed.clone(),
            semaphore,
            idle_workers: idle_workers.clone(),
            stealers: stealers.clone(),
            queued_tasks: queued_tasks.clone(),
            config,
        });

        // Запускаем воркеры
        for worker in workers {
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                pool_clone.worker_loop(worker).await;
            });
        }

        pool
    }

    async fn worker_loop(&self, local: Worker<Task>) {
        let enable_stealing = self.config.enable_work_stealing;
        let num_stealers = self.stealers.len();
        
        let mut fast_random_state = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        
        const BATCH_SIZE: usize = 16;
        let mut completed_batch = 0;
        
        'outer: loop {
            if completed_batch >= BATCH_SIZE {
                if self.cancellation_token.is_cancelled() {
                    break;
                }
                completed_batch = 0;
            }
            
            let task_opt = local.pop()
                .or_else(|| {
                    self.inject.steal().success().map(|t| {
                        self.queued_tasks.fetch_sub(1, Ordering::Relaxed);
                        t
                    })
                })
                .or_else(|| {
                    if !enable_stealing || num_stealers == 0 {
                        return None;
                    }
                    
                    let total_queued = self.queued_tasks.load(Ordering::Relaxed);
                    let active = self.active_spawned_tasks.load(Ordering::Relaxed);
                    
                    if total_queued == 0 && active < 2 {
                        return None;
                    }
                    
                    fast_random_state ^= fast_random_state << 13;
                    fast_random_state ^= fast_random_state >> 7;
                    fast_random_state ^= fast_random_state << 17;
                    let idx = (fast_random_state as usize) % num_stealers;
                    
                    self.stealers[idx].steal().success()
                });

            if let Some(task) = task_opt {
                task.await;
                completed_batch += 1;
                
                let prev = self.active_spawned_tasks.fetch_sub(1, Ordering::Release);
                if unlikely(prev == 1) {
                    if self.active_spawned_tasks.load(Ordering::Acquire) == 0 {
                        self.all_spawned_tasks_completed.notify_waiters();
                    }
                }
            } else {
                self.idle_workers.fetch_add(1, Ordering::Release);
                
                for _ in 0..2 {
                    if !local.is_empty() || !self.inject.is_empty() {
                        self.idle_workers.fetch_sub(1, Ordering::Acquire);
                        continue 'outer;
                    }
                    std::hint::spin_loop();
                }
                
                tokio::select! {
                    _ = self.global_notify.notified() => {
                        self.idle_workers.fetch_sub(1, Ordering::Acquire);
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
        let (tx, rx) = oneshot::channel::<SpawnResult<T>>();
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        // Один fetch_add вместо трех
        self.counter.fetch_add(1, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(1, Ordering::Relaxed);
        self.pool.total_spawned.fetch_add(1, Ordering::Relaxed);
        
        let counter = self.counter.clone();
        let notify = self.notify.clone();
        let completed = self.completed.clone();
        let failed = self.failed.clone();
        let pool_completed = self.pool.completed_tasks.clone();
        let pool_failed = self.pool.failed_tasks.clone();

        let task_fut = async move {
            let result: SpawnResult<T> = tokio::select! {
                _ = cancel_clone.cancelled() => Err(SpawnError::Cancelled),
                res = tokio::spawn(async move {
                    std::panic::AssertUnwindSafe(fut)
                        .catch_unwind()
                        .await
                        .map_err(|panic_info| {
                            SpawnError::Panic(format!("{:?}", panic_info))
                        })
                }) => {
                    res.unwrap_or_else(|join_err| {
                        if join_err.is_panic() {
                            Err(SpawnError::Panic("panic in spawned task".into()))
                        } else {
                            Err(SpawnError::JoinFailed(join_err.to_string()))
                        }
                    })
                }
            };

            if result.is_ok() {
                completed.fetch_add(1, Ordering::Relaxed);
                pool_completed.fetch_add(1, Ordering::Relaxed);
            } else {
                failed.fetch_add(1, Ordering::Relaxed);
                pool_failed.fetch_add(1, Ordering::Relaxed);
            }

            let _ = tx.send(result);

            if counter.fetch_sub(1, Ordering::Release) == 1 {
                notify.notify_one();
            }
        };

        // Используем быстрый push
        self.pool.push_task(Box::pin(task_fut));

        JoinHandle::new(cancel_token,rx )
    }

    pub fn spawn_blocking_with_handle<T, F>(&self, f: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<SpawnResult<T>>();
        let counter = self.counter.clone();
        let notify = self.notify.clone();
        let cancel_token = CancellationToken::new();
        let ct = cancel_token.clone();
        let permit_sema = self.pool.semaphore.clone();
        let completed = self.completed.clone();
        let failed = self.failed.clone();
        let pool_completed = self.pool.completed_tasks.clone();
        let pool_failed = self.pool.failed_tasks.clone();

        self.counter.fetch_add(1, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(1, Ordering::Relaxed);
        self.pool.total_spawned.fetch_add(1, Ordering::Relaxed);

        let task: Task = Box::pin(async move {
            let _permit = if let Some(s) = permit_sema {
                match s.acquire_owned().await {
                    Ok(p) => Some(p),
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        pool_failed.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(Err(SpawnError::SemaphoreClosed));
                        if counter.fetch_sub(1, Ordering::Release) == 1 {
                            notify.notify_one();
                        }
                        return;
                    }
                }
            } else { None };

            let result: SpawnResult<T> = tokio::select! {
                _ = ct.cancelled() => Err(SpawnError::Cancelled),
                val = tokio::task::spawn_blocking(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                }) => {
                    match val {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(panic_info)) => {
                            Err(SpawnError::Panic(format!("{:?}", panic_info)))
                        },
                        Err(join_err) => {
                            Err(SpawnError::JoinFailed(join_err.to_string()))
                        },
                    }
                }
            };

            if result.is_ok() {
                completed.fetch_add(1, Ordering::Relaxed);
                pool_completed.fetch_add(1, Ordering::Relaxed);
            } else {
                failed.fetch_add(1, Ordering::Relaxed);
                pool_failed.fetch_add(1, Ordering::Relaxed);
            }

            let _ = tx.send(result);
            drop(_permit);
            
            if counter.fetch_sub(1, Ordering::Release) == 1 {
                notify.notify_one();
            }
        });

        self.pool.push_task(task);

        JoinHandle::new(cancel_token,rx )
    }

    /// Ультра-быстрый spawn для простых CPU-bound задач
    #[inline]
    pub async fn spawn_bulk_compute<T, R, F>(
        &self,
        items: Vec<T>,
        compute: F,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + Clone + 'static,
    {
        let len = items.len();
        if len == 0 {
            return Vec::new();
        }

        // Один batch update всех счетчиков
        self.counter.fetch_add(len, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(len, Ordering::Relaxed);
        self.pool.total_spawned.fetch_add(len, Ordering::Relaxed);

        let compute = Arc::new(compute);
        let mut handles = Vec::with_capacity(len);
        
        for item in items {
            let comp = Arc::clone(&compute);
            let (tx, rx) = oneshot::channel();
            
            let counter = self.counter.clone();
            let notify = self.notify.clone();
            let completed = self.completed.clone();
            let pool_completed = self.pool.completed_tasks.clone();
            
            // Синхронное вычисление внутри async
            let task_fut = async move {
                // Выполняем вычисление синхронно
                let result = comp(item);
                completed.fetch_add(1, Ordering::Relaxed);
                pool_completed.fetch_add(1, Ordering::Relaxed);
                
                let _ = tx.send(Ok(result));
                
                if counter.fetch_sub(1, Ordering::Release) == 1 {
                    notify.notify_one();
                }
            };
            
            self.pool.push_task(Box::pin(task_fut));
            handles.push(rx);
        }
        
        // Собираем результаты
        let mut results = Vec::with_capacity(len);
        for rx in handles {
            let result = rx.await.unwrap_or(Err(SpawnError::ChannelClosed));
            results.push(result);
        }
        
        results
    }


    /// Оптимизированная пакетная обработка
    #[inline]
    pub async fn batch_process<T, R, F, Fut>(
        &self,
        items: Vec<T>,
        max_concurrent_batches: usize,
        processor: F,
    ) -> Vec<SpawnResult<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = R> + Send + 'static,
    {
        let total = items.len();
        if total == 0 {
            return Vec::new();
        }

        let semaphore = Arc::new(Semaphore::new(max_concurrent_batches));
        let processor = Arc::new(processor);
        
        // Pre-allocate все handles
        let mut handles = Vec::with_capacity(total);
        
        // Batch update счетчиков один раз
        self.counter.fetch_add(total, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(total, Ordering::Relaxed);
        self.pool.total_spawned.fetch_add(total, Ordering::Relaxed);
        
        for item in items {
            let sem = Arc::clone(&semaphore);
            let proc = Arc::clone(&processor);
            
            let (tx, rx) = oneshot::channel();
            let cancel_token = CancellationToken::new();
            let cancel_clone = cancel_token.clone();
            
            let counter = self.counter.clone();
            let notify = self.notify.clone();
            let completed = self.completed.clone();
            let failed = self.failed.clone();
            let pool_completed = self.pool.completed_tasks.clone();
            let pool_failed = self.pool.failed_tasks.clone();
            
            let task_fut = async move {
                let result: SpawnResult<R> = tokio::select! {
                    _ = cancel_clone.cancelled() => Err(SpawnError::Cancelled),
                    res = tokio::spawn(async move {
                        let _permit = sem.acquire().await.unwrap();
                        proc(item).await
                    }) => {
                        res.map_err(|e| {
                            if e.is_panic() {
                                SpawnError::Panic("panic".into())
                            } else {
                                SpawnError::JoinFailed(e.to_string())
                            }
                        })
                    }
                };
                
                if result.is_ok() {
                    completed.fetch_add(1, Ordering::Relaxed);
                    pool_completed.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed.fetch_add(1, Ordering::Relaxed);
                    pool_failed.fetch_add(1, Ordering::Relaxed);
                }
                
                let _ = tx.send(result);
                
                if counter.fetch_sub(1, Ordering::Release) == 1 {
                    notify.notify_one();
                }
            };
            
            self.pool.push_task(Box::pin(task_fut));
            handles.push(JoinHandle::new(cancel_token,rx ));
        }

        self.join_handles(handles).await
    }

    /// Оптимизированная потоковая обработка
    #[inline]
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
        
        // Заполняем начальную партию
        for _ in 0..max_concurrent {
            if let Some(item) = items_iter.next() {
                let proc = Arc::clone(&processor);
                let sem = Arc::clone(&semaphore);
                
                let fut = self.spawn_with_handle(async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(item).await
                });
                
                futures.push(Box::pin(fut) as TaskFuture<R>);
            } else {
                break;
            }
        }
        
        // Streaming processing
        while let Some(result) = futures.next().await {
            results.push(result);
            
            if let Some(item) = items_iter.next() {
                let proc = Arc::clone(&processor);
                let sem = Arc::clone(&semaphore);
                
                let fut = self.spawn_with_handle(async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(item).await
                });
                
                futures.push(Box::pin(fut) as TaskFuture<R>);
            }
        }
        
        results
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

    pub fn cancel_all<T>(&self, handles: &[JoinHandle<T>]) {
        for h in handles {
            h.cancel();
        }
    }

    /// Оптимизированное ожидание handles
    #[inline]
    pub async fn join_handles<T>(&self, handles: Vec<JoinHandle<T>>) -> Vec<SpawnResult<T>>
    where
        T: Send + 'static,
    {
        if handles.is_empty() {
            return Vec::new();
        }

        let len = handles.len();
        let mut futures = FuturesUnordered::from_iter(handles);
        let mut results = Vec::with_capacity(len);
        
        while let Some(result) = futures.next().await {
            results.push(result);
        }

        results
    }

    /// Оптимизированный параллельный map
    #[inline]
    pub async fn par_map_async<T, U, F, Fut>(&self, items: &[T], f: F) -> Vec<SpawnResult<U>>
    where
        T: Sync,
        U: Send + 'static,
        F: Fn(&T) -> Fut + Sync + Send + 'static,
        Fut: Future<Output = U> + Send + 'static,
    {
        if items.is_empty() {
            return Vec::new();
        }

        let len = items.len();
        
        // Batch update счетчиков
        self.counter.fetch_add(len, Ordering::Relaxed);
        self.pool.active_spawned_tasks.fetch_add(len, Ordering::Relaxed);
        self.pool.total_spawned.fetch_add(len, Ordering::Relaxed);
        
        let handles: Vec<_> = items.iter()
            .map(|item| {
                let (tx, rx) = oneshot::channel();
                let cancel_token = CancellationToken::new();
                let cancel_clone = cancel_token.clone();
                
                let fut = f(item);
                let counter = self.counter.clone();
                let notify = self.notify.clone();
                let completed = self.completed.clone();
                let failed = self.failed.clone();
                let pool_completed = self.pool.completed_tasks.clone();
                let pool_failed = self.pool.failed_tasks.clone();
                
                let task_fut = async move {
                    let result: SpawnResult<U> = tokio::select! {
                        _ = cancel_clone.cancelled() => Err(SpawnError::Cancelled),
                        res = tokio::spawn(fut) => {
                            res.map_err(|e| {
                                if e.is_panic() {
                                    SpawnError::Panic("panic".into())
                                } else {
                                    SpawnError::JoinFailed(e.to_string())
                                }
                            })
                        }
                    };
                    
                    if result.is_ok() {
                        completed.fetch_add(1, Ordering::Relaxed);
                        pool_completed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        pool_failed.fetch_add(1, Ordering::Relaxed);
                    }
                    
                    let _ = tx.send(result);
                    
                    if counter.fetch_sub(1, Ordering::Release) == 1 {
                        notify.notify_one();
                    }
                };
                
                self.pool.push_task(Box::pin(task_fut));
                JoinHandle::new(cancel_token, rx)
            })
            .collect();

        self.join_handles(handles).await
    }

    #[inline]
    pub fn metrics(&self) -> ScopeMetrics {
        ScopeMetrics {
            pending: self.counter.load(Ordering::Acquire),
            completed: self.completed.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }
}