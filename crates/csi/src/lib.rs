//! Trusted node-only CSI publisher for registered live Candidate FUSE mounts.
#![forbid(unsafe_code)]
mod local;
mod registry;
pub mod server;
pub mod wire;
pub use registry::{PodBinding, Registry};
pub mod rpc {
    pub mod identity {
        include!(concat!(env!("OUT_DIR"), "/csi.v1.Identity.rs"));
    }
    pub mod node {
        include!(concat!(env!("OUT_DIR"), "/csi.v1.Node.rs"));
    }
}
pub const DRIVER: &str = "csi.agent-computer.io";
pub const INSTANCE_KEY: &str = "agent-computer.io/mount-instance";
