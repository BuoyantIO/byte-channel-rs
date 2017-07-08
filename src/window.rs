use futures::*;

/// Tracks window sizes.
#[derive(Debug)]
pub struct Window {
    pending_increment: usize,
    advertised: usize,
    underflow: usize,
    blocked: Option<task::Task>,
}

impl Window {
    pub fn new(pending_increment: usize) -> Window {
        Window {
            pending_increment,
            advertised: 0,
            underflow: 0,
            blocked: None,
        }
    }

    pub fn advertised(&self) -> usize {
        self.advertised
    }

    /// Saves a window increment to be applied when `poll_increment` is called.
    pub fn advertise_increment(&mut self, incr: usize) {
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
        self.pending_increment += incr;
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
        if self.pending_increment == 0 {
            return None;
        }

        let incr = self.pending_increment;
        self.pending_increment = 0;

        if self.underflow < incr {
            let incr = incr - self.underflow;
            debug_assert!(0 < incr);
            self.advertised += incr;
            self.underflow = 0;
            return Some(incr);
        }

        debug_assert_eq!(self.advertised, 0);
        self.underflow -= incr;
        None
    }

    /// Consumes capacity from the window.
    ///
    /// ## Panics
    ///
    /// This function panics when more bytes are claimed than have been advertised by
    /// `poll_interval`.
    pub fn claim_advertised(&mut self, decr: usize) {
        if decr == 0 {
            return;
        }

        // If there's enough available space, take from that.
        if self.advertised < decr {
            panic!("illegal window underflow");
        }
        self.advertised -= decr;
    }

    /// Eventually removes capacity from the window.
    ///
    /// Once all advertised capacity has been claimed, new increments will not add
    /// capacity until they have compensated for any underflow incurred by shrinking the
    /// window.
    ///
    /// ## Panics
    ///
    /// This function panics when more bytes are claimed than have been advertised by
    /// `poll_interval`.
    pub fn shrink(&mut self, decr: usize) {
        self.underflow += decr;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::{Async, Poll, Stream};
    use futures::executor::{self, Notify, NotifyHandle};
    use std::cell::RefCell;
    use std::fmt;
    use std::rc::Rc;
    use std::sync::Arc;

    // TODO test that the task is notified on state change.

    #[test]
    fn poll_applies_increment() {
        let win = Rc::new(RefCell::new(Window::new(0)));
        let mut wstream = WindowStream(win.clone());
        assert_eq!(win.borrow().advertised(), 0);

        sassert_empty(&mut wstream);
        win.borrow_mut().advertise_increment(8);
        assert_eq!(win.borrow().advertised(), 0);

        sassert_next(&mut wstream, 8);
        assert_eq!(win.borrow().advertised(), 8);
    }

    #[test]
    fn poll_not_ready_when_underflow() {
        let win = Rc::new(RefCell::new(Window::new(8)));
        let mut wstream = WindowStream(win.clone());

        assert_eq!(win.borrow().advertised(), 0);
        sassert_next(&mut wstream, 8);
        assert_eq!(win.borrow().advertised(), 8);

        win.borrow_mut().shrink(8);
        sassert_empty(&mut wstream);
        assert_eq!(win.borrow().advertised(), 8);

        win.borrow_mut().claim_advertised(7);
        sassert_empty(&mut wstream);
        assert_eq!(win.borrow().advertised(), 1);

        win.borrow_mut().advertise_increment(7);
        sassert_empty(&mut wstream);
        assert_eq!(win.borrow().advertised(), 1);

        win.borrow_mut().advertise_increment(2);
        assert_eq!(win.borrow().advertised(), 1);
        sassert_next(&mut wstream, 1);
        assert_eq!(win.borrow().advertised(), 2);
    }

    // from futures-rs.
    fn notify_noop() -> NotifyHandle {
        struct Noop;
        impl Notify for Noop {
            fn notify(&self, _id: usize) {}
        }
        const NOOP: &'static Noop = &Noop;
        NotifyHandle::from(NOOP)
    }
    fn notify_panic() -> NotifyHandle {
        struct Panic;
        impl Notify for Panic {
            fn notify(&self, _id: usize) {
                panic!("should not be notified");
            }
        }
        NotifyHandle::from(Arc::new(Panic))
    }
    // fn sassert_done<S: Stream>(s: &mut S) {
    //     match executor::spawn(s).poll_stream_notify(&notify_panic(), 0) {
    //         Ok(Async::Ready(None)) => {}
    //         Ok(Async::Ready(Some(_))) => panic!("stream had more elements"),
    //         Ok(Async::NotReady) => panic!("stream wasn't ready"),
    //         Err(_) => panic!("stream had an error"),
    //     }
    // }
    fn sassert_empty<S: Stream>(s: &mut S) {
        match executor::spawn(s).poll_stream_notify(&notify_noop(), 0) {
            Ok(Async::Ready(None)) => panic!("stream is at its end"),
            Ok(Async::Ready(Some(_))) => panic!("stream had more elements"),
            Ok(Async::NotReady) => {}
            Err(_) => panic!("stream had an error"),
        }
    }
    fn sassert_next<S: Stream>(s: &mut S, item: S::Item)
    where
        S::Item: Eq + fmt::Debug,
    {
        match executor::spawn(s).poll_stream_notify(&notify_panic(), 0) {
            Ok(Async::Ready(None)) => panic!("stream is at its end"),
            Ok(Async::Ready(Some(e))) => assert_eq!(e, item),
            Ok(Async::NotReady) => panic!("stream wasn't ready"),
            Err(_) => panic!("stream had an error"),
        }
    }
    // fn sassert_err<S: Stream>(s: &mut S, err: S::Error)
    // where
    //     S::Error: Eq + fmt::Debug,
    // {
    //     match executor::spawn(s).poll_stream_notify(&notify_panic(), 0) {
    //         Ok(Async::Ready(None)) => panic!("stream is at its end"),
    //         Ok(Async::Ready(Some(_))) => panic!("stream had more elements"),
    //         Ok(Async::NotReady) => panic!("stream wasn't ready"),
    //         Err(e) => assert_eq!(e, err),
    //     }
    // }

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
}
