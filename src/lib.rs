//! Высокопроизводительный async thread pool с оптимизациями для высоких нагрузок
//! 
//! # Features
//! - Work-stealing для балансировки нагрузки
//! - Адаптивный батчинг и stream processing
//! - Graceful shutdown с таймаутами
//! - Отмена задач и обработка паник
//! - Детальные метрики и мониторинг
//! - Конфигурация для CPU-bound и I/O-bound workloads

pub mod errors;
pub mod handle;
pub mod model;
pub mod pool;
pub mod result;
pub mod scope;

pub use pool::{ThreadPoolInner,Config,Scope};