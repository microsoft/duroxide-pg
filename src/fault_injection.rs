//! Fault injection for testing resilience scenarios.
//!
//! This module provides a comprehensive fault injection mechanism to simulate
//! failure conditions in tests without complex runtime manipulation.
//!
//! ## Supported Faults
//!
//! - `disable_notifier`: Prevents the notifier thread from starting
//! - `refresh_delay`: Adds artificial delay to refresh queries
//! - `force_reconnect`: Triggers a reconnection in the notifier
//! - `refresh_should_error`: Makes the next refresh query fail
//! - `notifier_should_panic`: Simulates a panic in the notifier thread

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Fault injector for testing resilience scenarios.
///
/// Thread-safe structure that can be shared across provider and tests
/// to inject faults dynamically during test execution.
#[derive(Debug, Default)]
pub struct FaultInjector {
    /// If true, the notifier thread will not be spawned
    notifier_disabled: AtomicBool,

    /// Artificial delay (in milliseconds) to add to refresh queries
    refresh_delay_ms: AtomicU64,

    /// If true, forces the notifier to simulate a connection drop and reconnect
    force_reconnect: AtomicBool,

    /// If true, the next refresh query should return an error
    refresh_should_error: AtomicBool,

    /// If true, simulates a panic in the notifier thread
    notifier_should_panic: AtomicBool,
}

impl FaultInjector {
    /// Create a new fault injector with no faults enabled.
    pub fn new() -> Self {
        Self::default()
    }

    // =========================================================================
    // Notifier Control
    // =========================================================================

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

    /// Set whether the notifier should panic on next iteration.
    pub fn set_notifier_should_panic(&self, should_panic: bool) {
        self.notifier_should_panic.store(should_panic, Ordering::SeqCst);
    }

    /// Check if the notifier should panic.
    pub fn should_notifier_panic(&self) -> bool {
        self.notifier_should_panic.swap(false, Ordering::SeqCst)
    }

    // =========================================================================
    // Refresh Query Control
    // =========================================================================

    /// Set artificial delay for refresh queries (simulates slow database).
    pub fn set_refresh_delay(&self, delay: Duration) {
        self.refresh_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// Get the current refresh delay.
    pub fn get_refresh_delay(&self) -> Duration {
        Duration::from_millis(self.refresh_delay_ms.load(Ordering::SeqCst))
    }

    /// Set whether the next refresh query should return an error.
    pub fn set_refresh_should_error(&self, should_error: bool) {
        self.refresh_should_error.store(should_error, Ordering::SeqCst);
    }

    /// Check and consume the refresh error flag.
    pub fn should_refresh_error(&self) -> bool {
        self.refresh_should_error.swap(false, Ordering::SeqCst)
    }

    // =========================================================================
    // Connection Control
    // =========================================================================

    /// Force the notifier to reconnect (simulates connection drop).
    pub fn trigger_reconnect(&self) {
        self.force_reconnect.store(true, Ordering::SeqCst);
    }

    /// Check and consume the reconnect flag.
    pub fn should_reconnect(&self) -> bool {
        self.force_reconnect.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fault_injector_default() {
        let fi = FaultInjector::new();
        assert!(!fi.is_notifier_disabled());
        assert_eq!(fi.get_refresh_delay(), Duration::ZERO);
        assert!(!fi.should_reconnect());
    }

    #[test]
    fn test_disable_notifier() {
        let fi = FaultInjector::new();
        fi.disable_notifier();
        assert!(fi.is_notifier_disabled());
    }

    #[test]
    fn test_refresh_delay() {
        let fi = FaultInjector::new();
        fi.set_refresh_delay(Duration::from_secs(5));
        assert_eq!(fi.get_refresh_delay(), Duration::from_secs(5));
    }

    #[test]
    fn test_reconnect_flag() {
        let fi = FaultInjector::new();
        assert!(!fi.should_reconnect());
        fi.trigger_reconnect();
        assert!(fi.should_reconnect());
        // Flag should be consumed
        assert!(!fi.should_reconnect());
    }

    #[test]
    fn test_refresh_error_flag() {
        let fi = FaultInjector::new();
        assert!(!fi.should_refresh_error());
        fi.set_refresh_should_error(true);
        assert!(fi.should_refresh_error());
        // Flag should be consumed
        assert!(!fi.should_refresh_error());
    }

    #[test]
    fn test_notifier_panic_flag() {
        let fi = FaultInjector::new();
        assert!(!fi.should_notifier_panic());
        fi.set_notifier_should_panic(true);
        assert!(fi.should_notifier_panic());
        // Flag should be consumed
        assert!(!fi.should_notifier_panic());
    }
}
