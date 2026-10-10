//! Shared bounds for node-local continuous workers.
use agent_computer_store::{Error, Result};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct QueueOptions {
    pub(crate) concurrency: usize,
    pub(crate) poll_interval: Duration,
}
impl QueueOptions {
    pub fn new(concurrency: usize, poll_interval: Duration) -> Result<Self> {
        if !(1..=4).contains(&concurrency)
            || !(Duration::from_millis(250)..=Duration::from_secs(5)).contains(&poll_interval)
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        Ok(Self {
            concurrency,
            poll_interval,
        })
    }
}
impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            concurrency: 1,
            poll_interval: Duration::from_millis(250),
        }
    }
}
