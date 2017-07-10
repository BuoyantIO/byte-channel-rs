extern crate bytes;
#[cfg_attr(test, macro_use)]
extern crate futures;
#[cfg(test)]
extern crate test_futures;

mod buffer;
pub mod sync;
mod window;

#[derive(Copy, Clone, Debug)]
pub struct LostReceiver;
