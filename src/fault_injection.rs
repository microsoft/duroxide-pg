//! Fault injection for testing resilience scenarios.
//!
//! This module provides a simple fault injection mechanism to simulate
//! failure conditions in tests without complex runtime manipulation.

use std::sync::atomic::{AtomicBool, Ordering};

/// Fault injector for testing resilience scenarios.
///
/// Currently supports:
/// - `disable_notifier`: Prevents the notifier thread from starting
#[derive(Debug, Default)]
pub struct FaultInjector {
    /// If true, the notifier thread will not be spawned
    notifier_disabled: AtomicBool,
}

impl FaultInjector {
    /// Create a new fault injector with no faults enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Disable the notifier - prevents it from being spawned.
    ///
    /// When called before provider creation, the notifier thread will not start,
    /// simulating a notifier failure scenario.
    pub fn disable_notifier(&self) {
        self.notifier_disabled.store(true, Ordering::SeqCst);
    }

    /// Check if the notifier is disabled.
    pub fn is_notifier_disabled(&self) -> bool {
        self.notifier_disabled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fault_injector_default() {
        let fi = FaultInjector::new();
        assert!(!fi.is_notifier_disabled());
    }

    #[test]
    fn test_disable_notifier() {
        let fi = FaultInjector::new();
        fi.disable_notifier();
        assert!(fi.is_notifier_disabled());
    }
}
