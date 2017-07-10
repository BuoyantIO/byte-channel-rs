use futures::*;
use std::sync::Arc;

use super::SharedWindow;

pub fn new(w: SharedWindow) -> WindowAdvertiser {
    WindowAdvertiser(w)
}

/// Publishes window increments on the channel.
#[derive(Debug)]
pub struct WindowAdvertiser(SharedWindow);

impl WindowAdvertiser {
    fn is_orphaned(&self) -> bool {
        // `BytesSender` and `BytesReceiver` each retain a strong reference to the window.
        // Each `Chunk` produced by `ByteReceiver` retains a weak reference.
        Arc::strong_count(&self.0) == 1 && Arc::weak_count(&self.0) == 0
    }
}

impl Stream for WindowAdvertiser {
    type Item = usize;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<usize>, ()> {
        // If the window isn't closed, return either a new increment or indicate that
        // an increment isn't ready.  When poll_increment is not ready, it saves the
        // task to be notified by a channel.
        match (*self.0.lock().expect("locking byte channel"))
            .poll_increment()? {
            Async::Ready(incr) => Ok(Async::Ready(Some(incr))),

            Async::NotReady => {
                if self.is_orphaned() {
                    Ok(Async::Ready(None))
                } else {
                    Ok(Async::NotReady)
                }
            }
        }
    }
}
