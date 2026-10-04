mod frame;
mod probe;
pub mod runtime;
pub mod runtime_memory;

pub use frame::*;
pub use probe::*;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BODY: usize = 1_048_576;
