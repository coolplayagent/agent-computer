use crate::{Output, Result, renewal, streaming};
use std::time::Instant;

pub(crate) enum Control {
    Renewal(Box<renewal::Channel>),
    Streaming {
        channel: Box<streaming::Channel>,
        renewable: bool,
    },
}
impl Control {
    pub(crate) fn deadline(&self) -> Instant {
        match self {
            Self::Renewal(c) => c.window.deadline(),
            Self::Streaming { channel, .. } => channel.window.deadline(),
        }
    }
    pub(crate) fn poll(&mut self, outputs: [&Output; 2], running: bool) -> Result<()> {
        match self {
            Self::Renewal(c) if running => c.poll(),
            Self::Renewal(_) => Ok(()),
            Self::Streaming { channel, .. } => channel.poll(outputs, running),
        }
    }
    pub(crate) fn output_complete(&self, outputs: [&Output; 2]) -> bool {
        match self {
            Self::Renewal(c) => !c.window.awaiting_grant(),
            Self::Streaming { channel, .. } => channel.complete(outputs),
        }
    }
    pub(crate) fn renewal(&self) -> Option<renewal::Progress> {
        match self {
            Self::Renewal(c) => Some(c.window.progress().clone()),
            Self::Streaming {
                channel,
                renewable: true,
            } => Some(channel.window.progress().clone()),
            Self::Streaming {
                renewable: false, ..
            } => None,
        }
    }
    pub(crate) fn stream(&self) -> Result<Option<streaming::Progress>> {
        match self {
            Self::Renewal(_) => Ok(None),
            Self::Streaming { channel, .. } => channel.progress().map(Some),
        }
    }
}
