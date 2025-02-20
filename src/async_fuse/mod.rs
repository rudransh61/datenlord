//! FUSE async implementation

use std::sync::Arc;

use datenlord::config::StorageParams;
use memfs::s3_wrapper::{DoNothingImpl, S3BackEndImpl};

use self::memfs::kv_engine::KVEngineType;
use crate::async_fuse::fuse::session;
use crate::AsyncFuseArgs;

pub mod fuse;
pub mod memfs;
/// Datenlord metrics
// Caused by prometheus macros
#[allow(clippy::ignored_unit_patterns)]
pub mod metrics;
pub mod proactor;
pub mod util;

/// Start async-fuse
pub async fn start_async_fuse(
    kv_engine: Arc<KVEngineType>,
    args: &AsyncFuseArgs,
) -> anyhow::Result<()> {
    metrics::start_metrics_server();

    memfs::kv_engine::kv_utils::register_node_id(
        &kv_engine,
        &args.node_id,
        &args.ip_address.to_string(),
        args.server_port,
    )
    .await?;

    let volume_info = serde_json::to_string(&args.storage_config)?;
    memfs::kv_engine::kv_utils::register_volume(&kv_engine, &args.node_id, &volume_info).await?;

    let mount_point = std::path::Path::new(&args.mount_dir);

    match args.storage_config.params {
        StorageParams::S3(_) => {
            let fs: memfs::MemFs<memfs::S3MetaData<S3BackEndImpl>> = memfs::MemFs::new(
                &args.mount_dir,
                args.storage_config.cache_capacity,
                &args.ip_address.to_string(),
                args.server_port,
                kv_engine,
                &args.node_id,
                &args.storage_config,
            )
            .await?;

            let ss = session::new_session_of_memfs(mount_point, fs).await?;
            ss.run().await?;
        }
        StorageParams::None(_) => {
            let fs: memfs::MemFs<memfs::S3MetaData<DoNothingImpl>> = memfs::MemFs::new(
                &args.mount_dir,
                args.storage_config.cache_capacity,
                &args.ip_address.to_string(),
                args.server_port,
                kv_engine,
                &args.node_id,
                &args.storage_config,
            )
            .await?;

            let ss = session::new_session_of_memfs(mount_point, fs).await?;
            ss.run().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    mod integration_tests;
    mod test_util;

    use std::{fs, io};

    use futures::StreamExt;
    use tracing::debug;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_iter() -> io::Result<()> {
        let dir = tokio::task::spawn_blocking(|| fs::read_dir(".")).await??;
        let mut dir = futures::stream::iter(dir);
        while let Some(entry) = dir.next().await {
            let path = entry?.path();
            if path.is_file() {
                debug!("read file: {:?}", path);
                let buf = tokio::fs::read(path).await?;
                let output_length = 16;
                if buf.len() > output_length {
                    debug!(
                        "first {} bytes: {:?}",
                        output_length,
                        &buf.get(..output_length)
                    );
                } else {
                    debug!("total bytes: {:?}", buf);
                }
            } else {
                debug!("skip directory: {:?}", path);
            }
        }
        Ok(())
    }
}
