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
        time::Duration,
        sync::Arc,
    };

    #[tokio::test]
    async fn test_cancellation() {
        println!("\n=== TEST: Отмена задач ===");
        let pool = ThreadPoolInner::new(4, None);
        let scope = Scope::new(pool.clone());

        // Тест 1: проверка что cancel() работает
        let handle = scope.spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            42
        });

        assert!(!handle.is_cancelled(), "Задача не должна быть отменена");
        handle.cancel();
        assert!(handle.is_cancelled(), "Задача должна быть помечена как отмененная");
        
        println!("  ✓ Cancellation API работает");
        
        // Тест 2: быстрая задача завершается до отмены
        let handle2 = scope.spawn(async {
            10
        });
        
        tokio::time::sleep(Duration::from_millis(50)).await;
        
        match handle2.await {
            Ok(val) => {
                println!("  ✓ Быстрая задача завершилась: {}", val);
                assert_eq!(val, 10);
            }
            Err(e) => {
                println!("  Задача вернула ошибку: {:?}", e);
            }
        }
        
        // Даем scope завершиться корректно
        scope.wait().await;
    }

    #[tokio::test]
    async fn test_no_memory_leaks() {
        println!("\n=== TEST: Проверка утечек памяти ===");
        
        // Тест 1: Arc правильно дропается после scope
        {
            let pool = ThreadPoolInner::new(4, None);
            let arc_count = Arc::strong_count(&pool);
            println!("  Initial Arc count: {}", arc_count);
            
            {
                let scope = Scope::new(pool.clone());
                let inner_count = Arc::strong_count(&pool);
                println!("  With scope Arc count: {}", inner_count);
                assert_eq!(inner_count, arc_count + 1, "Scope должен увеличить счетчик");
                
                // Запускаем задачи
                let handles: Vec<_> = (0..10)
                    .map(|i| scope.spawn(async move { i }))
                    .collect();
                
                // Ждем результаты
                for handle in handles {
                    let _ = handle.await;
                }
                
                // ИЛИ используйте wait (не оба!)
                scope.wait().await;
            } // scope дропнут
            
            // Задержка для завершения async tasks
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            let final_count = Arc::strong_count(&pool);
            println!("  After scope Arc count: {}", final_count);
            
            // ВАЖНО: воркеры держат Arc на pool!
            // Ожидаемое значение = initial + num_workers
            let expected = arc_count + pool.ref_config().num_threads;
            assert!(
                final_count <= expected + 1,
                "Arc count слишком большой: {} (ожидалось <= {})",
                final_count, expected + 1
            );
        }
        
        // Тест 2: Мониторинг правильно останавливается
        {
            let pool = ThreadPoolInner::new(4, None);
            let initial_count = Arc::strong_count(&pool);
            
            let monitor_token = pool.start_monitoring(
                Duration::from_millis(10),
                |_| { /* no-op */ }
            );
            
            tokio::time::sleep(Duration::from_millis(50)).await;
            
            let with_monitor_count = Arc::strong_count(&pool);
            println!("  With monitor Arc count: {}", with_monitor_count);
            assert!(with_monitor_count > initial_count, "Monitor должен держать Arc");
            
            // Останавливаем мониторинг
            ThreadPoolInner::stop_monitoring(monitor_token);
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            let after_stop_count = Arc::strong_count(&pool);
            println!("  After stop monitor Arc count: {}", after_stop_count);
            assert_eq!(after_stop_count, initial_count, "Мониторинг должен освободить Arc");
        }
        
        // Тест 3: Shutdown освобождает все ресурсы
        {
            let pool = ThreadPoolInner::new(4, None);
            let scope = Scope::new(pool.clone());
            
            let handles: Vec<_> = (0..100)
                .map(|i| scope.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    i
                }))
                .collect();
            
            // Ждем завершения
            for handle in handles {
                let _ = handle.await;
            }
            
            scope.wait().await;
            
            // Shutdown с таймаутом
            let shutdown_ok = pool.shutdown_timeout(Duration::from_secs(5)).await;
            assert!(shutdown_ok, "Shutdown должен завершиться успешно");
            
            let metrics = pool.metrics();
            assert_eq!(metrics.active_tasks, 0, "Не должно быть активных задач");
            
            println!("  ✓ Shutdown завершен успешно");
        }
        
        println!("  ✓ Утечек памяти не обнаружено");
    }

    #[tokio::test]
    async fn test_timeout() {
        println!("\n=== TEST: Timeout задач ===");
        let pool = ThreadPoolInner::new(4, None);
        let scope = Scope::new(pool.clone());

        let handle = scope.spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            42
        });

        let result = handle.await_timeout(Duration::from_millis(100)).await;
        
        match result {
            Err(SpawnError::Timeout) => {
                println!("  ✓ Timeout обработан корректно");
            }
            Ok(val) => {
                panic!("Ожидали timeout, получили результат: {}", val);
            }
            Err(e) => {
                panic!("Ожидали timeout, получили другую ошибку: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_metrics_tracking() {
        println!("\n=== TEST: Отслеживание метрик ===");
        
        // Подавляем панику в этом тесте
        let _guard = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        
        let pool = ThreadPoolInner::with_config(Config::cpu_bound());
        let scope = Scope::new(pool.clone());

        let items: Vec<_> = (0..1000).collect();
        let _results = scope.batch_process(items, 50, |x| Box::pin(async move {
            tokio::time::sleep(Duration::from_micros(100)).await;
            if x % 10 == 0 {
                panic!("Test panic");
            }
            x
        }),JoinOrdering::Ordered).await;

        // Даем время метрикам обновиться
        tokio::time::sleep(Duration::from_millis(100)).await;

        let pool_metrics = pool.metrics();
        let scope_metrics = scope.metrics();

        println!("  Pool метрики:");
        println!("    Всего запущено: {}", pool_metrics.total_spawned);
        println!("    Завершено: {}", pool_metrics.completed_tasks);
        println!("    Провалено: {}", pool_metrics.failed_tasks);
        println!("    Success rate: {:.1}%", pool_metrics.success_rate() * 100.0);
        
        println!("  Scope метрики:");
        println!("    Завершено: {}", scope_metrics.completed);
        println!("    Провалено: {}", scope_metrics.failed);
        
        assert!(scope_metrics.total() >= 1000, "Должно быть запущено минимум 1000 задач");
        assert!(scope_metrics.completed > 0, "Должны быть завершенные задачи");
        assert!(scope_metrics.failed > 0, "Должны быть проваленные задачи (из-за паник)");
        
        // Восстанавливаем panic handler
        drop(_guard);
    }

    #[tokio::test]
    async fn test_monitoring_cpu() {
        println!("\n=== TEST: Мониторинг в реальном времени ===");
        let pool = ThreadPoolInner::new(8, Some(100));
        let monitor_token = pool.start_monitoring(Duration::from_millis(100), |metrics| {
            if metrics.active_tasks > 0 {
                println!("  [Monitor] Active: {}, Queue: {}, Utilization: {:.1}%",
                         metrics.active_tasks, metrics.queued_tasks, metrics.utilization() * 100.0);
            }
        });

        let scope = Scope::new(pool.clone());
        let items: Vec<_> = (0..500).collect();
        
        let _results = scope.batch_process_cpu(items, 20,  |x| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            x
        }).await;

        monitor_token.cancel();
        println!("  ✓ Мониторинг завершен");
    }

    #[tokio::test]
    async fn test_monitoring_scope() {
        println!("\n=== TEST: Мониторинг в реальном времени ===");
        let pool = ThreadPoolInner::new(8, Some(100));
        let scope = Scope::new(pool.clone());
        let monitor_token = scope.start_monitoring(Duration::from_millis(100), |metrics| {
                println!("  [Monitor] completed: {}, pending: {}, failed: {:.1}%",
                         metrics.completed, metrics.pending, metrics.failed);
        });
        let items: Vec<_> = (0..500).collect();
        
        let _results = scope.batch_process(items, 20,  |x| Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            x
        }),JoinOrdering::Ordered).await;

        monitor_token.cancel();
        println!("  ✓ Мониторинг завершен");
    }
}