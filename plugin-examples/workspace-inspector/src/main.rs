#[tokio::main]
async fn main() -> paseo_plugin::Result<()> {
    paseo_workspace_inspector::plugin()?.run().await
}
