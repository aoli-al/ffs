use datafusion::prelude::*;

#[tokio::main]
async fn main() -> datafusion::error::Result<()> {
    let ctx = SessionContext::new();
    let df = ctx.sql("select 1").await?;
    df.show().await?;
    Ok(())
}
