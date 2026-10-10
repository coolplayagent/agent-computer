use crate::{DRIVER, Registry, local, rpc, wire};
use futures_util::StreamExt;
use std::{
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tonic::{Request, Response, Status};

pub const SOCKET: &str = "/var/lib/kubelet/plugins/csi.agent-computer.io/csi.sock";
pub const REGISTRY: &str = "/var/lib/agent-computer-csi";

#[derive(Clone)]
struct Service {
    registry: Arc<Registry>,
    node: String,
    permits: Arc<tokio::sync::Semaphore>,
}
#[tonic::async_trait]
impl rpc::identity::identity_server::Identity for Service {
    async fn get_plugin_info(
        &self,
        _: Request<wire::Empty>,
    ) -> Result<Response<wire::PluginInfo>, Status> {
        Ok(Response::new(wire::PluginInfo {
            name: DRIVER.into(),
            vendor_version: env!("CARGO_PKG_VERSION").into(),
        }))
    }
    async fn get_plugin_capabilities(
        &self,
        _: Request<wire::Empty>,
    ) -> Result<Response<wire::Empty>, Status> {
        Ok(Response::new(wire::Empty {}))
    }
    async fn probe(&self, _: Request<wire::Empty>) -> Result<Response<wire::Empty>, Status> {
        Ok(Response::new(wire::Empty {}))
    }
}
#[tonic::async_trait]
impl rpc::node::node_server::Node for Service {
    async fn node_get_info(
        &self,
        _: Request<wire::Empty>,
    ) -> Result<Response<wire::NodeInfo>, Status> {
        Ok(Response::new(wire::NodeInfo {
            node_id: self.node.clone(),
            max_volumes_per_node: 32,
        }))
    }
    async fn node_get_capabilities(
        &self,
        _: Request<wire::Empty>,
    ) -> Result<Response<wire::Empty>, Status> {
        Ok(Response::new(wire::Empty {}))
    }
    async fn node_publish_volume(
        &self,
        request: Request<wire::Publish>,
    ) -> Result<Response<wire::Empty>, Status> {
        let node = self.node.clone();
        self.blocking(move |registry| registry.publish(request.into_inner(), &node))
            .await
    }
    async fn node_unpublish_volume(
        &self,
        request: Request<wire::Unpublish>,
    ) -> Result<Response<wire::Empty>, Status> {
        self.blocking(move |registry| registry.unpublish(request.into_inner()))
            .await
    }
}
impl Service {
    async fn blocking(
        &self,
        work: impl FnOnce(&Registry) -> io::Result<()> + Send + 'static,
    ) -> Result<Response<wire::Empty>, Status> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("node publisher busy"))?;
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || {
            // Keep capacity occupied even if the request is cancelled or times out.
            let _permit = permit;
            work(&registry)
        })
        .await
        .map_err(|_| Status::internal("node operation interrupted"))?
        .map_err(|e| {
            eprintln!("CSI operation rejected: {e}");
            Status::failed_precondition("node mount verification failed")
        })?;
        Ok(Response::new(wire::Empty {}))
    }
}
fn socket(path: &Path) -> io::Result<tokio::net::UnixListener> {
    local::directory(
        path.parent()
            .ok_or_else(|| local::invalid("socket has no parent"))?,
    )?;
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            use std::os::unix::fs::FileTypeExt;
            if !m.file_type().is_socket() || m.uid() != 0 || m.mode() & 0o7777 != 0o600 {
                return Err(local::invalid("foreign socket path"));
            }
            std::fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let socket = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(socket)
}
fn incoming(
    listener: tokio::net::UnixListener,
) -> impl futures_util::Stream<Item = io::Result<tokio::net::UnixStream>> {
    tokio_stream::wrappers::UnixListenerStream::new(listener).filter_map(|connection| async move {
        match connection {
            Ok(stream) if stream.peer_cred().is_ok_and(|cred| cred.uid() == 0) => Some(Ok(stream)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        }
    })
}
/// Root system service in the host mount namespace. No network listener or
/// controller RPC exists. Operator paths are fixed.
pub async fn serve(node: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !local::name(&node)
        || std::fs::metadata("/proc/self/ns/mnt")?.ino()
            != std::fs::metadata("/proc/1/ns/mnt")?.ino()
    {
        return Err(local::invalid("host node namespace required").into());
    }
    let registry = Arc::new(Registry::open(Path::new(REGISTRY))?);
    let plugin_dir = local::directory(Path::new(SOCKET).parent().expect("constant parent"))?;
    // Acquire lifetime ownership before removing stale Unix sockets.
    let _service_lock = local::lock(&plugin_dir)?;
    let csi = socket(Path::new(SOCKET))?;
    let service = Service {
        registry,
        node,
        permits: Arc::new(tokio::sync::Semaphore::new(32)),
    };
    let csi = tonic::transport::Server::builder()
        .concurrency_limit_per_connection(32)
        .timeout(Duration::from_secs(15))
        .add_service(
            rpc::identity::identity_server::IdentityServer::new(service.clone())
                .max_decoding_message_size(65536),
        )
        .add_service(
            rpc::node::node_server::NodeServer::new(service.clone())
                .max_decoding_message_size(65536),
        )
        .serve_with_incoming(incoming(csi));
    csi.await?;
    Ok(())
}
