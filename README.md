# async_rayon
async rayon implementetion tokio in development

# WARNING! LIBRARY IS NOT SUPPORTED!

# Why This Library?
`tokio::spawn` is fast but offers no resource control. `rayon` is excellent for CPU-bound work but blocking. This library sits in between - providing resource control with acceptable overhead (~4x slower than `tokio::spawn`).

## Advantages

- Resource Control: max_pending prevents OOM in production
- Production Ready: Graceful shutdown, metrics, monitoring
- Scope API: Automatic cleanup, cancellation support
- Work-Stealing: Efficient load balancing
- Flexible: Both fast and controlled modes available

## Disadvantages

- 4x Slower than tokio::spawn: Acceptable for I/O, problematic for CPU
- 100x Slower than rayon: For pure CPU work, use rayon instead
- Memory Overhead: Metrics and tracking structures
- Complexity: More complex than plain tokio::spawn

## When Overhead is Justified

- Processing 10k+ concurrent tasks
- Need OOM protection via max_pending
- Graceful shutdown is critical
- Metrics required for monitoring
- Production environment with unpredictable load spikes
- I/O-bound workloads (network, database, files)


# Comparison with Alternatives
## Tokio

| `Feature` | `tokio::spawn` | `async_rayon`|
| ---| ---| ---|
| Speed | 3.4242ms for 10k tasks ✅ | 9.6959ms for 10k tasks ⚠️ |
| Max_pending | ❌ No limit - can OOM | ✅ Protects against OOM |
| Metrics | ❌ Basic only  | ✅ Detailed (queue, utilization, etc) |
| Graceful shutdown | ⚠️ Manual tracking | ✅ Built-in with timeout |
| Cancellation | ⚠️ Manual AbortHandle | ✅ CancellationToken per task |
| Scope API | ❌ No | ✅ Auto-cleanup on drop|


### tokio example
```rust
// Fast but no protection against memory exhaustion
let handles: Vec<_> = (0..100_000)
    .map(|i| tokio::spawn(async move { work(i).await }))
    .collect();

// Manual tracking required
futures::future::join_all(handles).await;
```

### async_rayon example:
```rust

// Controlled resource usage
let pool = ThreadPoolInner::with_config(Config {
    max_pending: Some(10_000), // Limit concurrent tasks
    // ...
});
let scope = Scope::new(pool);

let handles: Vec<_> = (0..100_000)
    .map(|i| scope.spawn_with_handle(async move { work(i).await }))
    .collect();

// Auto-cleanup on scope drop

```

## Rayon

| `Feature` | `rayon` | `async_rayon`|
| ---| ---| ---|
| Speed (CPU) | 50ms for 5M tasks ✅ | 4.2s for 5M tasks ❌ |
| Blocking | ✅ Sync - no async overhead| ✅ Non-blocking async |
| I/O tasks | ❌ Blocks OS threads  | ✅ 10k+ concurrent I/O |
| Work-stealing | ✅ OS thread level | ✅ Async task level |

### rayon example

```rust
// Excellent for CPU-bound, but blocks on I/O
use rayon::prelude::*;
let results: Vec<_> = data.par_iter()
    .map(|x| {
        // This would block an OS thread:
        // reqwest::blocking::get(url).unwrap()
        x * x
    })
    .collect();
```

### async_rayon example

```rust
// Non-blocking - perfect for I/O
let results = scope.stream_process(
    urls,
    10_000,  // 10k concurrent requests
    |url| Box::pin(async move {
        reqwest::get(&url).await?.text().await
    })
).await;

```


# Performance Benchmarks

## 10,000 tasks
- `tokio::spawn`: 3.4ms  (2.96M tasks/sec) ← baseline
- `async_rayon`:  9.7ms  (1.03M/s tasks/sec)  ← 4.1x slower
- `rayon` (CPU-bound):      0.05ms (200M tasks/sec)  ← 70x faster for sync

## 5,000,000 tasks
- `tokio::spawn`: ~3-4s
- `async_rayon`: ~4.2-4.4s
- `rayon`: ~50ms (CPU-bound only)

# When to Use

## I/O-Bound Tasks with Backpressure

```rust
let pool = ThreadPoolInner::with_config(Config::io_bound());
let scope = Scope::new(pool);

scope.stream_process(
    urls,
    5_000,  // Max 5k concurrent - prevents server overload
    |url| Box::pin(async move {
        reqwest::get(&url).await?.text().await
    })
).await;
```

## Stable Services with Graceful Shutdown

```rust

// Microservice with proper cleanup
let pool = ThreadPoolInner::new(16, Some(10_000));

// On SIGTERM
tokio::select! {
    _ = signal::ctrl_c() => {
        if pool.shutdown_timeout(Duration::from_secs(30)).await {
            println!("Graceful shutdown complete");
        }
    }
}

```

## ETL Pipelines

```rust

// Separate CPU and I/O pools for optimal resource usage
let cpu_pool = ThreadPoolInner::with_config(Config::cpu_bound());
let io_pool = ThreadPoolInner::with_config(Config::io_bound());

// Extract (I/O)
let data = io_scope.stream_process(sources, 1000, extract).await;
// Transform (CPU)
let transformed = cpu_scope.batch_process(data, 10, transform).await;
// Load (I/O)
io_scope.batch_process(transformed, 50, load).await;

```

## Cancellation Support

```rust

let handle = scope.spawn_with_handle(async move {
    long_running_task().await
});

// Cancel anytime
handle.cancel();

// Or with timeout
let result = handle.await_timeout(Duration::from_secs(5)).await;

```

## Detailed Metrics

```rust

let metrics = pool.metrics_fast();
println!("Utilization: {:.1}%", metrics.utilization() * 100.0);
println!("Queue pressure: {}", metrics.queue_pressure());
println!("Success rate: {:.2}%", metrics.success_rate() * 100.0);

```

# Examples

## Basic spawn

```rust

use async_rayon::{ThreadPoolInner, Scope, Config};

#[tokio::main]
async fn main() {
    let pool = ThreadPoolInner::with_config(Config::io_bound());
    let scope = Scope::new(pool.clone());

    let handles: Vec<_> = (0..1000)
        .map(|i| {
            scope.spawn_with_handle(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                i * 2
            })
        })
        .collect();

    let results = scope.join_handles(handles).await;
    println!("Processed: {}", results.len());
    
    pool.shutdown().await;
}

```

## Batch Processing

```rust

let pool = ThreadPoolInner::with_config(Config::cpu_bound());
let scope = Scope::new(pool);

let data: Vec<_> = (0..100_000).collect();

let results = scope.batch_process(
    data,
    50,      // max concurrent batches
    |item| Box::pin(async move {
        process_item(item).await
    })
).await;

```

## Stream Processing for Large Datasets

```rust

let pool = ThreadPoolInner::with_config(Config::io_bound());
let scope = Scope::new(pool);

let urls: Vec<_> = load_million_urls();

// Memory-efficient streaming
let results = scope.stream_process(
    urls,
    10_000,  // 10k concurrent
    |url| Box::pin(async move {
        reqwest::get(&url).await?.text().await
    })
).await;

```

## Real-Time Monitoring

```rust

let pool = ThreadPoolInner::new(8, Some(5_000));

let monitor = pool.start_monitoring(Duration::from_secs(1), |metrics| {
    println!("Active: {}, Queue: {}, Utilization: {:.1}%",
             metrics.active_tasks,
             metrics.queued_tasks,
             metrics.utilization() * 100.0);
});

// Do work...

ThreadPoolInner::stop_monitoring(monitor);

```

## Graceful Shutdown

```rust

let pool = ThreadPoolInner::new(16, Some(10_000));

// On shutdown signal
tokio::signal::ctrl_c().await.unwrap();
println!("Shutting down gracefully...");

if pool.shutdown_timeout(Duration::from_secs(30)).await {
    println!("All tasks completed");
} else {
    println!("Timeout - forcing shutdown");
}

```

## Parallel Map

```rust

let pool = ThreadPoolInner::with_config(Config::io_bound());
let scope = Scope::new(pool);

let user_ids = vec![1, 2, 3, 4, 5];

let results = scope.par_map_async(&user_ids, |&id| async move {
    fetch_user_data(id).await
}).await;

for result in results {
    match result {
        Ok(user) => println!("User: {:?}", user),
        Err(e) => eprintln!("Error: {:?}", e),
    }
}

```

## Configuration

```rust

// CPU-bound: threads = num_cores
let pool = ThreadPoolInner::with_config(Config::cpu_bound());

// I/O-bound: threads = num_cores × 2
let pool = ThreadPoolInner::with_config(Config::io_bound());

```

## Custom Configuration

```rust

let pool = ThreadPoolInner::with_config(Config {
    num_threads: 32,
    max_pending: Some(50_000),
    enable_work_stealing: true,
    task_timeout: Some(Duration::from_secs(30)),
});

```
