use futures::*;

/// Tracks window sizes.
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
        if incr == 0 {
            return;
        }

        // Apply the increment to the underflow immediately (because we only care about
        // notifying when available space is created).
        if incr <= self.underflow {
            self.underflow -= incr;
            return;
        }

        // If this increment adds available space to the window, save this increment to be
        // applied by `poll_increment`.
        let incr = incr - self.underflow;
        self.underflow = 0;
        self.pending += incr;
        debug_assert!(0 < incr);

        // TODO be more discrening about notifaction.  (Ensure some ratio between
        // available and pending or ...)
        if let Some(t) = self.blocked.take() {
            t.notify();
        }
    }

    /// Obtains and applies the next window increment.
    ///
    /// If no increment is available, the current task is saved to be notified when the
    /// window is open.
    pub fn poll_increment(&mut self) -> Poll<usize, ()> {
        Ok(match self.apply_increment() {
            Some(incr) => Async::Ready(incr),
            None => {
                self.blocked = Some(task::current());
                Async::NotReady
            }
        })
    }

    /// If a non-zero increment is pending, apply it to the window and return the amount
    /// of available space added.
    fn apply_increment(&mut self) -> Option<usize> {
        if self.pending == 0 {
            return None;
        }

        let incr = self.pending;
        self.pending = 0;

        if self.underflow < incr {
            let incr = incr - self.underflow;
            debug_assert!(0 < incr);
            self.underflow = 0;
            self.available += incr;
            return Some(incr);
        }

        debug_assert_eq!(self.available, 0);
        self.underflow -= incr;
        None
    }

    /// Consumes capacity from the window.
    ///
    /// The window may underflow. When this occurs, poll_increment will not return updates
    /// until increments have been pushed that restore a positive window size.
    pub fn decrement(&mut self, mut decr: usize) {
        if decr == 0 {
            return;
        }

        // Decrement from pending before looking at available updates.
        if decr <= self.pending {
            self.pending -= decr;
            return;
        }
        decr -= self.pending;
        self.pending = 0;

        // If there's enough available space, take from that.
        if decr <= self.available {
            debug_assert_eq!(self.underflow, 0);
            self.available -= decr;
            return;
        }

        // Otherwise, we have to go into overflow.
        self.underflow += decr - self.available;
        self.available = 0;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::{self, Async, Poll, Future, Stream};
    use futures::executor::{self, Notify, NotifyHandle};
    use std::cell::RefCell;
    use std::rc::Rc;

    // from futures-rs.
    pub fn notify_noop() -> NotifyHandle {
        struct Noop;
        impl Notify for Noop {
            fn notify(&self, _id: usize) {}
        }
        const NOOP: &'static Noop = &Noop;
        NotifyHandle::from(NOOP)
    }

    struct WindowStream(Rc<RefCell<Window>>);
    impl Stream for WindowStream {
        type Item = usize;
        type Error = ();
        fn poll(&mut self) -> Poll<Option<usize>, ()> {
            let mut win = self.0.borrow_mut();
            let sz = try_ready!(win.poll_increment());
            Ok(Async::Ready(Some(sz)))
        }
    }

    #[test]
    fn poll_applies_increment() {
        let win = Rc::new(RefCell::new(Window::new(0)));

        let mut task = {
            let win = win.clone();
            executor::spawn(futures::lazy(move || {
                let w = WindowStream(win);
                w.into_future().map(|(up, _)| up).map_err(|_| {})
            }))
        };

        assert!(
            task.poll_future_notify(&notify_noop(), 0)
                .unwrap()
                .is_not_ready()
        );
        assert_eq!(win.borrow().available(), 0);

        win.borrow_mut().push_increment(8);
        assert_eq!(win.borrow().available(), 0);

        // TODO test that the task is notified on state change.
        assert_eq!(
            task.poll_future_notify(&notify_noop(), 0).unwrap(),
            Async::Ready(Some(8))
        );
        assert_eq!(win.borrow().available(), 8);
    }

    #[test]
    fn poll_not_ready_when_underflow() {
        let win = Rc::new(RefCell::new(Window::new(0)));

        let mut task = {
            let win = win.clone();
            executor::spawn(futures::lazy(move || {
                let w = WindowStream(win);
                w.into_future().map(|(up, _)| up).map_err(|_| {})
            }))
        };

        win.borrow_mut().decrement(8);
        assert_eq!(win.borrow().available(), 0);
        assert!(
            task.poll_future_notify(&notify_noop(), 0)
                .unwrap()
                .is_not_ready()
        );

        win.borrow_mut().push_increment(8);
        assert_eq!(win.borrow().available(), 0);
        assert!(
            task.poll_future_notify(&notify_noop(), 0)
                .unwrap()
                .is_not_ready()
        );

        // TODO test that the task is notified on state change.
        win.borrow_mut().push_increment(8);
        assert_eq!(win.borrow().available(), 0);
        assert_eq!(
            task.poll_future_notify(&notify_noop(), 0).unwrap(),
            Async::Ready(Some(8))
        );
        assert_eq!(win.borrow().available(), 8);
    }
}
