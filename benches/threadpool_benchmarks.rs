use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use async_rayon::pool::{ThreadPoolInner, Config as PoolConfig, Scope};
use tokio::time::Duration;
use std::hint::black_box;

fn create_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get())
        .enable_all()
        .build()
        .unwrap()
}

// Benchmark 1: Spawn overhead
fn bench_spawn_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_overhead");
    
    for size in [100, 1000, 10000] {
        group.throughput(Throughput::Elements(size as u64));
        
        // with_handle
        group.bench_with_input(
            BenchmarkId::new("with_handle", size),
            &size,
            |b, &size| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                
                let pool = rt.block_on(async {
                    ThreadPoolInner::with_config(PoolConfig::default())
                });
                
                // ✅ Создаем scope ОДИН РАЗ
                let scope = Scope::new(pool);
                
                b.to_async(&rt).iter(|| {
                    // ✅ Захватываем ссылку на scope
                    let scope = &scope;
                    async move {
                        let handles: Vec<_> = (0..size)
                            .map(|i| scope.spawn(async move { black_box(i) }))
                            .collect();
                        
                        for handle in handles {
                            black_box(handle.await.unwrap());
                        }
                    }
                });
            },
        );
        
        // fast_mode
        group.bench_with_input(
            BenchmarkId::new("fast_mode", size),
            &size,
            |b, &size| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                
                let pool = rt.block_on(async {
                    ThreadPoolInner::with_config(PoolConfig::io_bound())
                });
                
                let scope = Scope::new(pool);
                
                b.to_async(&rt).iter(|| {
                    let scope = &scope;
                    async move {
                        let handles: Vec<_> = (0..size)
                            .map(|i| scope.spawn(async move { black_box(i) }))
                            .collect();
                        
                        for handle in handles {
                            black_box(handle.await.unwrap());
                        }
                    }
                });
            },
        );
        
        // par_map
        group.bench_with_input(
            BenchmarkId::new("par_map", size),
            &size,
            |b, &size| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let data: Vec<_> = (0..size).collect();
                
                let pool = rt.block_on(async {
                    ThreadPoolInner::with_config(PoolConfig::io_bound_fast())
                });
                
                let scope = Scope::new(pool);
                
                b.to_async(&rt).iter(|| {
                    let scope = &scope;
                    let data = &data;
                    async move {
                        let results = scope.par_map_async(data, |&i| async move {
                            black_box(i)
                        }).await;
                        black_box(results);
                    }
                });
            },
        );
        
        // tokio baseline
        group.bench_with_input(
            BenchmarkId::new("tokio_spawn", size),
            &size,
            |b, &size| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                
                b.to_async(&rt).iter(|| async {
                    let handles: Vec<_> = (0..size)
                        .map(|i| tokio::spawn(async move { black_box(i) }))
                        .collect();
                    
                    for handle in handles {
                        black_box(handle.await.unwrap());
                    }
                });
            },
        );
    }
    
    group.finish();
}

// Benchmark 2: Batch processing throughput
fn bench_batch_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_processing");
    group.sample_size(20); // Меньше samples для быстрых тестов
    
    for (tasks, batch_size) in [(1000, 100), (10000, 500), (50000, 1000)].iter() {
        group.throughput(Throughput::Elements(*tasks as u64));
        
        // Regular batch_process
        group.bench_with_input(
            BenchmarkId::new("batch", format!("{}_{}", tasks, batch_size)), 
            &(*tasks, *batch_size), 
            |b, &(tasks, batch_size)| {
                let rt = create_runtime();
                b.to_async(&rt).iter(|| async move {
                    let pool = ThreadPoolInner::with_config(PoolConfig::io_bound());
                    let scope = Scope::new(pool.clone());
                    let items: Vec<_> = (0..tasks).collect();
                    
                    let _results = scope.batch_process(
                        items,
                        batch_size,
                        |x| Box::pin(async move { black_box(x * 2) })
                    ).await;
                });
            }
        );
    }
    
    group.finish();
}

// Benchmark 3: Stream processing vs Batch
fn bench_stream_vs_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_vs_batch");
    group.sample_size(20);
    
    let tasks = 10000;
    group.throughput(Throughput::Elements(tasks as u64));
    
    // Stream
    group.bench_function("stream_10k", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::cpu_bound());
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..tasks).collect();
            
            let _results = scope.stream_process(
                items,
                1000,
                |x| Box::pin(async move { black_box(x * 2) })
            ).await;
        });
    });
    
    // Batch
    group.bench_function("batch_10k", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::io_bound());
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..tasks).collect();
            
            let _results = scope.batch_process(
                items,
                500,
                |x| Box::pin(async move { black_box(x * 2) })
            ).await;
        });
    });
    
    group.finish();
}

// Benchmark 4: Work-stealing efficiency
fn bench_work_stealing(c: &mut Criterion) {
    let mut group = c.benchmark_group("work_stealing");
    
    // С work-stealing
    group.bench_function("with_stealing", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig {
                num_threads: num_cpus::get(),
                max_pending: Some(5000),
                enable_work_stealing: true,
                task_timeout: Some(Duration::from_secs(30)),
                ..Default::default()

            });
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..5000).collect();
            
            let _results = scope.batch_process(
                items,
                250,
                |x| Box::pin(async move {
                    // Неравномерная нагрузка
                    if x % 10 == 0 {
                        tokio::time::sleep(Duration::from_micros(10)).await;
                    }
                    black_box(x)
                })
            ).await;
        });
    });
    
    // Без work-stealing
    group.bench_function("without_stealing", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig {
                num_threads: num_cpus::get(),
                max_pending: Some(5000),
                enable_work_stealing: false,
                task_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            });
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..5000).collect();
            
            let _results = scope.batch_process(
                items,
                250,
                |x| Box::pin(async move {
                    if x % 10 == 0 {
                        tokio::time::sleep(Duration::from_micros(10)).await;
                    }
                    black_box(x)
                })
            ).await;
        });
    });
    
    group.finish();
}

// Benchmark 5: Thread count scaling
fn bench_thread_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("thread_scaling");
    group.sample_size(20);
    
    let tasks = 5000;
    group.throughput(Throughput::Elements(tasks as u64));
    
    for threads in [2, 4, 8, 16].iter() {
        if *threads <= num_cpus::get() * 2 {
            group.bench_with_input(
                BenchmarkId::new("threads", threads), 
                threads, 
                |b, &threads| {
                    let rt = create_runtime();
                    b.to_async(&rt).iter(|| async move {
                        let pool = ThreadPoolInner::with_config(PoolConfig {
                            num_threads: threads,
                            max_pending: Some(2000),
                            enable_work_stealing: true,
                            task_timeout: Some(Duration::from_secs(30)),
                            ..Default::default()
                        });
                        let scope = Scope::new(pool.clone());
                        let items: Vec<_> = (0..tasks).collect();
                        
                        let _results = scope.batch_process(
                            items,
                            250,
                            |x| Box::pin(async move {
                                tokio::task::yield_now().await;
                                black_box(x * 2)
                            })
                        ).await;
                    });
                }
            );
        }
    }
    
    group.finish();
}

// Benchmark 6: CPU-bound vs I/O-bound config
fn bench_config_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("config_comparison");
    group.sample_size(20);
    
    let tasks = 5000;
    
    // CPU-bound config
    group.bench_function("cpu_bound", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::cpu_bound());
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..tasks).collect();
            
            let _results = scope.batch_process(
                items,
                250,
                |x| Box::pin(async move { black_box(x * x * x) })
            ).await;
        });
    });
    
    // I/O-bound config
    group.bench_function("io_bound", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::io_bound());
            let scope = Scope::new(pool.clone());
            let items: Vec<_> = (0..tasks).collect();
            
            let _results = scope.batch_process(
                items,
                250,
                |x| Box::pin(async move {
                    tokio::task::yield_now().await;
                    black_box(x * 2)
                })
            ).await;
        });
    });
    
    group.finish();
}

// Benchmark 7: Blocking tasks
fn bench_blocking_tasks(c: &mut Criterion) {
    let mut group = c.benchmark_group("blocking_tasks");
    group.sample_size(10);
    
    for tasks in [100, 500, 1000].iter() {
        group.throughput(Throughput::Elements(*tasks as u64));
        
        group.bench_with_input(BenchmarkId::new("blocking", tasks), tasks, |b, &tasks| {
            let rt = create_runtime();
            b.to_async(&rt).iter(|| async move {
                let pool = ThreadPoolInner::with_config(PoolConfig::cpu_bound());
                let scope = Scope::new(pool.clone());
                
                let handles: Vec<_> = (0..tasks)
                    .map(|i| scope.spawn_blocking_with_handle(move || {
                        // Simulate CPU work
                        let mut sum = 0u64;
                        for j in 0..1000 {
                            sum = sum.wrapping_add(i as u64 * j);
                        }
                        black_box(sum)
                    }))
                    .collect();
                
                let _results = scope.join_handles(handles).await;
            });
        });
    }
    
    group.finish();
}

// Benchmark 8: Latency под нагрузкой
fn bench_latency_under_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("latency_under_load");
    group.sample_size(50);
    
    // Одна задача без нагрузки
    group.bench_function("single_no_load", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::io_bound());
            let scope = Scope::new(pool.clone());
            
            let handle = scope.spawn_with_handle(async { black_box(42) });
            let _result = handle.await;
        });
    });
    
    // Одна задача при 1000 активных
    group.bench_function("single_with_load", |b| {
        let rt = create_runtime();
        b.to_async(&rt).iter(|| async {
            let pool = ThreadPoolInner::with_config(PoolConfig::io_bound());
            let scope = Scope::new(pool.clone());
            
            // Создаем фоновую нагрузку
            let _bg_handles: Vec<_> = (0..1000)
                .map(|i| scope.spawn_with_handle(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    i
                }))
                .collect();
            
            // Измеряем latency этой задачи
            let handle = scope.spawn_with_handle(async { black_box(42) });
            let _result = handle.await;
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_spawn_overhead,
    bench_batch_processing,
    bench_stream_vs_batch,
    bench_work_stealing,
    bench_thread_scaling,
    bench_config_comparison,
    bench_blocking_tasks,
    bench_latency_under_load,
);

criterion_main!(benches);