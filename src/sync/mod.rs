use std::sync::{Arc, Mutex, Weak};

use buffer::ChannelBuffer;
use window::Window;

mod chunk;
mod receiver;
mod sender;
mod window;

pub use self::chunk::Chunk;
pub use self::sender::ByteSender;
pub use self::receiver::ByteReceiver;
pub use self::window::WindowAdvertiser;

/// Creates an asynchronous channel for transfering a stream of immutable `Bytes`.
///
/// A sender must be aware of the receiver's available window size and take care not to
pub fn new<E>(initial_window_size: usize) -> (WindowAdvertiser, ByteSender<E>, ByteReceiver<E>) {
    let buffer = Arc::new(Mutex::new(Some(ChannelBuffer::default())));
    let window = Arc::new(Mutex::new(Window::new(initial_window_size)));

    let wx = window::new(window.clone());
    let tx = sender::new(buffer.clone(), window.clone());
    let rx = receiver::new(buffer, window);
    (wx, tx, rx)
}

type SharedBuffer<E> = Arc<Mutex<Option<ChannelBuffer<E>>>>;
type SharedWindow = Arc<Mutex<Window>>;
type WeakWindow = Weak<Mutex<Window>>;

fn return_buffer_to_window<E>(buffer: &Option<ChannelBuffer<E>>, window: &SharedWindow) {
    let sz = buffer.as_ref().map(|b| b.len()).unwrap_or(0);
    if sz == 0 {
        return;
    }
    (*window.lock().expect("locking byte channel window")).advertise_increment(sz);
}
