#[cfg(test)]
mod tests {
    use async_rayon::{
    errors::SpawnError,
    model::JoinOrdering,
    pool::{
        Config,
        ThreadPoolInner,
        Scope
        },
    };
    use std::{
        future::Future,
        time::{Duration,Instant},
    };

    async fn measure<F, Fut, T>(name: &str, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let start = Instant::now();
        let result = f().await;
        let elapsed = start.elapsed();
        println!("✓ {}: {:?}", name, elapsed);
        result
    }

    #[tokio::test]
    async fn load_test_1_small_fast_tasks() {
        println!("\n=== LOAD TEST 1: 10k быстрых задач (100μs каждая) ===");
        let pool = ThreadPoolInner::with_config(Config::io_bound());
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..10_000).collect();
        
        let results = measure("10k tasks @ 100μs", || async {
            scope.batch_process(items,  100, |x| Box::pin(async move {
                tokio::time::sleep(Duration::from_micros(100)).await;
                x * 2
            }),JoinOrdering::UnOrdered).await
        }).await;

        assert_eq!(results.len(), 10_000);
        let metrics = pool.metrics();
        println!("  Успешно: {}/{}", metrics.completed_tasks, results.len());
        println!("  Утилизация: {:.1}%", metrics.utilization() * 100.0);
    }

    #[tokio::test]
    async fn load_test_2_medium_tasks() {
        println!("\n=== LOAD TEST 2: 5k средних задач (5ms каждая) ===");
        let pool = ThreadPoolInner::with_config(Config::io_bound());
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..5_000).collect();
        
        let results = measure("5k tasks @ 5ms", || async {
            scope.batch_process(items, 50, |x| Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                format!("result_{}", x)
            }),JoinOrdering::UnOrdered).await
        }).await;

        let successful = results.iter().filter(|r| r.is_ok()).count();
        println!("  Успешно: {}/{}", successful, results.len());
        
        let metrics = pool.metrics();
        println!("  Success rate: {:.1}%", metrics.success_rate() * 100.0);
    }

    #[tokio::test]
    async fn load_test_3_heavy_blocking() {
        println!("\n=== LOAD TEST 3: 1k блокирующих задач ===");
        let pool = ThreadPoolInner::with_config(Config::cpu_bound());
        let scope = Scope::new(pool.clone());

        let handles: Vec<_> = measure("1k blocking tasks", || async {
            (0..1_000)
                .map(|i| {
                    scope.spawn_blocking_with_handle(move || {
                        std::thread::sleep(Duration::from_millis(10));
                        i * i
                    })
                })
                .collect()
        }).await;

        let results = scope.join_handles(handles,JoinOrdering::UnOrdered).await;
        let successful = results.iter().filter(|r| r.is_ok()).count();
        println!("  Успешно: {}/{}", successful, results.len());
    }

    #[tokio::test]
    async fn load_test_4_mixed_workload() {
        println!("\n=== LOAD TEST 4: Смешанная нагрузка ===");
        let pool = ThreadPoolInner::with_config(Config::default());
        let scope = Scope::new(pool.clone());

        let (async_results, blocking_results, stream_results) = tokio::join!(
            measure("2k async tasks", || async {
                let items: Vec<_> = (0..2_000).collect();
                scope.batch_process(items, 100, |x| Box::pin(async move {
                    tokio::time::sleep(Duration::from_micros(500)).await;
                    x + 1
                }),JoinOrdering::UnOrdered).await
            }),
            
            measure("500 blocking tasks", || async {
                let handles: Vec<_> = (0..500)
                    .map(|i| scope.spawn_blocking_with_handle(move || {
                        std::thread::sleep(Duration::from_millis(2));
                        i * 2
                    }))
                    .collect();
                scope.join_handles(handles,JoinOrdering::UnOrdered).await
            }),
            
            measure("1k stream tasks", || async {
                let items: Vec<_> = (0..1_000).collect();
                scope.stream_process(items, 100, |x| Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    x * 3
                })).await
            })
        );

        println!("  Async успешно: {}/2000", async_results.iter().filter(|r| r.is_ok()).count());
        println!("  Blocking успешно: {}/500", blocking_results.iter().filter(|r| r.is_ok()).count());
        println!("  Stream успешно: {}/1000", stream_results.iter().filter(|r| r.is_ok()).count());
    }

    #[tokio::test]
    async fn load_test_5_extreme_concurrency() {
        println!("\n=== LOAD TEST 5: Экстремальная параллельность (50k задач) ===");
        let pool = ThreadPoolInner::with_config(Config::io_bound());
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..50_000).collect();
        
        let start = Instant::now();
        let results = scope.batch_process(items, 500, |x| Box::pin(async move {
            tokio::time::sleep(Duration::from_micros(50)).await;
            x % 1000
        }),JoinOrdering::UnOrdered).await;
        let elapsed = start.elapsed();

        let successful = results.iter().filter(|r| r.is_ok()).count();
        println!("  Время: {:?}", elapsed);
        println!("  Успешно: {}/{}", successful, results.len());
        println!("  Пропускная способность: {:.0} задач/сек", 
                 50_000.0 / elapsed.as_secs_f64());
        
        let metrics = pool.metrics();
        println!("  Итого запущено: {}", metrics.total_spawned);
        println!("  Утилизация: {:.1}%", metrics.utilization() * 100.0);
    }

    #[tokio::test]
    async fn load_test_6_stress_with_panics() {
        println!("\n=== LOAD TEST 6: Стресс-тест с паниками ===");
        
        // Подавляем вывод паник в тесте
        std::panic::set_hook(Box::new(|_| {}));
        
        let pool = ThreadPoolInner::new(8, Some(500));
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..1_000).collect();
        
        let results = measure("1k tasks (10% panic)", || async {
            scope.batch_process(items, 50, |x| Box::pin(async move {
                if x % 10 == 0 {
                    panic!("Intentional panic at {}", x);
                }
                tokio::time::sleep(Duration::from_micros(100)).await;
                x
            }),JoinOrdering::UnOrdered).await
        }).await;

        let successful = results.iter().filter(|r| r.is_ok()).count();
        let panicked = results.iter().filter(|r| matches!(r, Err(SpawnError::Panic(_)))).count();
        
        println!("  Успешно: {}", successful);
        println!("  Паник перехвачено: {}", panicked);
        println!("  Процент успеха: {:.1}%", (successful as f64 / results.len() as f64) * 100.0);
        
        let metrics = pool.metrics();
        println!("  Pool success rate: {:.1}%", metrics.success_rate() * 100.0);
        
        // Восстанавливаем стандартный обработчик паник
        let _ = std::panic::take_hook();
        
        assert!(successful >= 900);
    }

    #[tokio::test]
    async fn load_test_7_parallel_scopes() {
        println!("\n=== LOAD TEST 7: Параллельные scope ===");
        let pool = ThreadPoolInner::with_config(Config::io_bound());

        let results = measure("3 параллельных scope по 2k задач", || async {
            let (r1, r2, r3) = tokio::join!(
                pool.scope_async(|scope| async move {
                    let items: Vec<_> = (0..2_000).collect();
                    scope.batch_process(items, 100, |x| Box::pin(async move {
                        tokio::time::sleep(Duration::from_micros(200)).await;
                        x
                    }),JoinOrdering::UnOrdered).await
                }),
                
                pool.scope_async(|scope| async move {
                    let items: Vec<_> = (0..2_000).collect();
                    scope.batch_process(items, 100, |x| Box::pin(async move {
                        tokio::time::sleep(Duration::from_micros(200)).await;
                        x * 2
                    }),JoinOrdering::UnOrdered).await
                }),
                
                pool.scope_async(|scope| async move {
                    let items: Vec<_> = (0..2_000).collect();
                    scope.batch_process(items, 100, |x| Box::pin(async move {
                        tokio::time::sleep(Duration::from_micros(200)).await;
                        x * 3
                    }),JoinOrdering::UnOrdered).await
                })
            );
            
            (r1, r2, r3)
        }).await;

        println!("  Scope 1: {} успешных", results.0.iter().filter(|r| r.is_ok()).count());
        println!("  Scope 2: {} успешных", results.1.iter().filter(|r| r.is_ok()).count());
        println!("  Scope 3: {} успешных", results.2.iter().filter(|r| r.is_ok()).count());
    }

    #[tokio::test]
    async fn load_test_8_memory_efficiency() {
        println!("\n=== LOAD TEST 8: Эффективность памяти (stream vs batch) ===");
        let pool = ThreadPoolInner::with_config(Config::io_bound());
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..20_000).collect();
        let stream_results = measure("20k stream", || async {
            scope.stream_process(items, 200, |x| Box::pin(async move {
                tokio::time::sleep(Duration::from_micros(50)).await;
                x
            })).await
        }).await;

        let items: Vec<_> = (0..20_000).collect();
        let batch_results = measure("20k batch", || async {
            scope.batch_process(items, 200, |x| Box::pin(async move {
                tokio::time::sleep(Duration::from_micros(50)).await;
                x
            }),JoinOrdering::UnOrdered).await
        }).await;

        println!("  Stream успешно: {}/20000", stream_results.iter().filter(|r| r.is_ok()).count());
        println!("  Batch успешно: {}/20000", batch_results.iter().filter(|r| r.is_ok()).count());
    }
}