#![forbid(unsafe_code)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let node = args.next().ok_or("usage: agent-computer-csi NODE_NAME")?;
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    agent_computer_csi::server::serve(node).await
}
