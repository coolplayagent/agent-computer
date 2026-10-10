use tonic_build::manual::{Builder, Method, Service};
fn service(package: &str, name: &str, methods: &[(&str, &str, &str, &str)]) -> Service {
    let mut builder = Service::builder().name(name).package(package);
    for (method, route, input, output) in methods {
        builder = builder.method(
            Method::builder()
                .name(method)
                .route_name(route)
                .input_type(format!("crate::wire::{input}"))
                .output_type(format!("crate::wire::{output}"))
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        );
    }
    builder.build()
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    Builder::new().compile(&[
        service(
            "csi.v1",
            "Identity",
            &[
                ("get_plugin_info", "GetPluginInfo", "Empty", "PluginInfo"),
                (
                    "get_plugin_capabilities",
                    "GetPluginCapabilities",
                    "Empty",
                    "Empty",
                ),
                ("probe", "Probe", "Empty", "Empty"),
            ],
        ),
        service(
            "csi.v1",
            "Node",
            &[
                (
                    "node_publish_volume",
                    "NodePublishVolume",
                    "Publish",
                    "Empty",
                ),
                (
                    "node_unpublish_volume",
                    "NodeUnpublishVolume",
                    "Unpublish",
                    "Empty",
                ),
                (
                    "node_get_capabilities",
                    "NodeGetCapabilities",
                    "Empty",
                    "Empty",
                ),
                ("node_get_info", "NodeGetInfo", "Empty", "NodeInfo"),
            ],
        ),
    ]);
}
