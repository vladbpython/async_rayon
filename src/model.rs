#[derive(Debug, Clone)]
pub struct PoolMetrics {
    pub active_tasks: usize,
    pub idle_workers: usize,
    pub queued_tasks: usize,
    pub total_spawned: usize,
    pub completed_tasks: usize,
    pub failed_tasks: usize,
}

impl PoolMetrics {
    pub fn utilization(&self) -> f64 {
        if self.active_tasks + self.idle_workers == 0 {
            return 0.0;
        }
        self.active_tasks as f64 / (self.active_tasks + self.idle_workers) as f64
    }

    pub fn queue_pressure(&self) -> f64 {
        self.queued_tasks as f64
    }

    pub fn success_rate(&self) -> f64 {
        let total = self.completed_tasks + self.failed_tasks;
        if total == 0 {
            return 1.0;
        }
        self.completed_tasks as f64 / total as f64
    }
}



#[derive(Debug, Clone)]
pub struct ScopeMetrics {
    pub pending: usize,
    pub completed: usize,
    pub failed: usize,
}

impl ScopeMetrics {
    pub fn total(&self) -> usize {
        self.pending + self.completed + self.failed
    }

    pub fn success_rate(&self) -> f64 {
        let finished = self.completed + self.failed;
        if finished == 0 {
            return 1.0;
        }
        self.completed as f64 / finished as f64
    }
}


pub enum JoinOrdering {
    Ordered,
    UnOrdered,    
}