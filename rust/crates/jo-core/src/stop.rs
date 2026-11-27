//! A cloneable stop signal with interruptible waits (Python's
//! `threading.Event` as used for Quit/update shutdown).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Stop(Arc<(Mutex<bool>, Condvar)>);

impl Stop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cvar.notify_all();
    }

    pub fn is_set(&self) -> bool {
        *self.0.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sleep up to `timeout`; returns `true` as soon as stop is requested.
    pub fn wait(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.0;
        let deadline = Instant::now() + timeout;
        let mut set = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*set {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            set = cvar.wait_timeout(set, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_is_interrupted() {
        let stop = Stop::new();
        assert!(!stop.wait(Duration::from_millis(10)));
        let other = stop.clone();
        let t = std::thread::spawn(move || other.wait(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(20));
        stop.set();
        assert!(t.join().unwrap());
        assert!(stop.is_set());
    }
}
