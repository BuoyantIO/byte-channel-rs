use futures::*;
use std::sync::Arc;

use super::SharedWindow;

pub fn new(w: SharedWindow) -> WindowAdvertiser {
    WindowAdvertiser(w)
}

#[derive(Debug)]
pub struct WindowAdvertiser(SharedWindow);

impl WindowAdvertiser {
    fn is_orphaned(&self) -> bool {
        // `BytesSender` and `BytesReceiver` retain strong references to the window.
        // `Chunk` retains a weak reference.
        Arc::strong_count(&self.0) == 1 && Arc::weak_count(&self.0) == 0
    }
}

impl Stream for WindowAdvertiser {
    type Item = usize;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<usize>, ()> {
        let mut window = self.0.lock().expect("locking byte channel");

        if let Some(ref mut w) = *window {
            // If the window isn't closed, return either a new increment or indicate that
            // an increment isn't ready.  When poll_increment is not ready, it saves the task to be notified by a channel
            match w.poll_increment()? {
                Async::Ready(incr) => {
                    return Ok(Async::Ready(Some(incr)));
                }

                Async::NotReady => {
                    if !self.is_orphaned() {
                        return Ok(Async::NotReady);
                    }
                }
            }
        }

        // The window is no
        *window = None;
        Ok(Async::Ready(None))
    }
}
