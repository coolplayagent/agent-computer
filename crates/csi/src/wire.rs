//! Wire-compatible bounded subset of CSI v1.12.
//! Unimplemented RPCs return gRPC UNIMPLEMENTED; there is no controller service.
//! Field numbers: https://github.com/container-storage-interface/spec/blob/v1.12.0/csi.proto
use prost::Message;
use std::collections::HashMap;

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}
#[derive(Clone, PartialEq, Message)]
pub struct PluginInfo {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub vendor_version: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct NodeInfo {
    #[prost(string, tag = "1")]
    pub node_id: String,
    #[prost(int64, tag = "2")]
    pub max_volumes_per_node: i64,
}
#[derive(Clone, PartialEq, Message)]
pub struct Publish {
    #[prost(string, tag = "1")]
    pub volume_id: String,
    #[prost(map = "string, string", tag = "2")]
    pub publish_context: HashMap<String, String>,
    #[prost(string, tag = "3")]
    pub staging_target_path: String,
    #[prost(string, tag = "4")]
    pub target_path: String,
    #[prost(message, optional, tag = "5")]
    pub volume_capability: Option<Capability>,
    #[prost(bool, tag = "6")]
    pub readonly: bool,
    #[prost(map = "string, string", tag = "7")]
    pub secrets: HashMap<String, String>,
    #[prost(map = "string, string", tag = "8")]
    pub volume_context: HashMap<String, String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Capability {
    #[prost(oneof = "capability::Access", tags = "1,2")]
    pub access: Option<capability::Access>,
    #[prost(message, optional, tag = "3")]
    pub access_mode: Option<AccessMode>,
}
pub mod capability {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Access {
        #[prost(message, tag = "1")]
        Block(super::Empty),
        #[prost(message, tag = "2")]
        Mount(super::Mount),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct Mount {
    #[prost(string, tag = "1")]
    pub fs_type: String,
    #[prost(string, repeated, tag = "2")]
    pub mount_flags: Vec<String>,
    #[prost(string, tag = "3")]
    pub volume_mount_group: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct AccessMode {
    #[prost(int32, tag = "1")]
    pub mode: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct Unpublish {
    #[prost(string, tag = "1")]
    pub volume_id: String,
    #[prost(string, tag = "2")]
    pub target_path: String,
}
