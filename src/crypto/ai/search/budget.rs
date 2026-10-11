use super::types::{SearchBudget, SearchProvider};
use std::sync::atomic::Ordering;

pub struct BudgetManager {
    budget: SearchBudget,
}

impl BudgetManager {
    pub fn new(budget: SearchBudget) -> Self {
        Self { budget }
    }

    pub fn try_consume(&self, provider: SearchProvider, limit: u8) -> bool {
        let counter = match provider {
            SearchProvider::Tavily => &self.budget.tavily_used,
            SearchProvider::Exa => &self.budget.exa_used,
            SearchProvider::Local => &self.budget.local_used,
        };

        loop {
            let current = counter.load(Ordering::SeqCst);
            if current >= limit {
                return false;
            }
            if counter
                .compare_exchange_weak(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    #[allow(dead_code)]
    pub fn remaining(&self, provider: SearchProvider, limit: u8) -> u8 {
        let counter = match provider {
            SearchProvider::Tavily => &self.budget.tavily_used,
            SearchProvider::Exa => &self.budget.exa_used,
            SearchProvider::Local => &self.budget.local_used,
        };
        let current = counter.load(Ordering::SeqCst);
        if current >= limit {
            0
        } else {
            limit - current
        }
    }
}
