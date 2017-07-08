use futures::*;

#[derive(Debug)]
pub struct Window {
    available: usize,
    underflow: usize,
    pending: usize,
    blocked: Option<task::Task>,
}

impl Window {
    pub fn new(pending: usize) -> Window {
        Window {
            pending,
            available: 0,
            underflow: 0,
            blocked: None,
        }
    }

    pub fn available(&self) -> usize {
        self.available
    }

    /// Saves a window increment to be applied when `poll_increment` is called.
    pub fn push_increment(&mut self, incr: usize) {
        self.pending += incr;
        // TODO be more discrening about notifaction.  (Ensure some ratio between
        // available and pending or ...)
        if let Some(t) = self.blocked.take() {
            t.notify();
        }
    }

    /// Obtains and applies the next window increment.
    pub fn poll_increment(&mut self) -> Poll<usize, ()> {
        Ok(match self.apply_increment() {
            Some(incr) => Async::Ready(incr),
            None => {
                self.blocked = Some(task::current());
                Async::NotReady
            }
        })
    }

    ///
    fn apply_increment(&mut self) -> Option<usize> {
        let incr = self.pending;
        self.pending = 0;
        if incr == 0 {
            None
        } else if self.underflow < incr {
            let incr = incr - self.underflow;
            debug_assert!(0 < incr);
            self.underflow = 0;
            self.available += incr;
            Some(incr)
        } else {
            debug_assert_eq!(self.available, 0);
            self.underflow -= incr;
            None
        }
    }

    /// Consumes capacity from the window.
    ///
    /// The window may become negative.
    pub fn decrement(&mut self, decr: usize) {
        if decr == 0 {
            // do nothing
        } else if self.available < decr {
            self.underflow += decr - self.available;
            self.available = 0;
        } else {
            debug_assert_eq!(self.underflow, 0);
            self.available -= decr;
        }
    }
}
