//! Linux subprocess and scheduling support.

mod linux;
pub(crate) mod process;
pub(crate) use linux::{PriorityGuard, configure_pipe_size, nonblocking};
