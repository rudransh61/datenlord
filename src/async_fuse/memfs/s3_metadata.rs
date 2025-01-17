use std::collections::BTreeMap;
use std::fmt::Debug;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use clippy_utilities::{Cast, OverflowArithmetic};
use datenlord::config::{StorageConfig, StorageParams};
use nix::errno::Errno;
use nix::sys::stat::SFlag;
use parking_lot::RwLock as SyncRwLock; // conflict with tokio RwLock
use tokio::sync::Mutex;
use tracing::{debug, info, instrument};

use super::cache::{GlobalCache, IoMemBlock};
use super::dir::DirEntry;
use super::dist::client as dist_client;
use super::dist::server::CacheServer;
use super::fs_util::{self, FileAttr, NEED_CHECK_PERM};
use super::id_alloc_used::INumAllocator;
use super::kv_engine::{KVEngine, KVEngineType, KeyType, MetaTxn, ValueType};
use super::metadata::{error, MetaData, ReqContext};
use super::node::Node;
use super::s3_node::S3Node;
use super::s3_wrapper::S3BackEnd;
use super::{check_type_supported, CreateParam, RenameParam, SetAttrParam};
#[cfg(feature = "abi-7-18")]
use crate::async_fuse::fuse::fuse_reply::FuseDeleteNotification;
use crate::async_fuse::fuse::fuse_reply::{ReplyDirectory, StatFsParam};
use crate::async_fuse::fuse::protocol::{FuseAttr, INum, FUSE_ROOT_ID};
use crate::async_fuse::memfs::check_name_length;
use crate::async_fuse::util::build_error_result_from_errno;
use crate::common::error::DatenLordResult;
use crate::common::error::{Context as DatenLordContext, DatenLordError}; // conflict with anyhow::Context
use crate::function_name;

/// A helper function to build [`DatenLordError::InconsistentFS`] with default
/// context and get the function name automatic.
macro_rules! build_inconsistent_fs {
    ($ino:expr) => {
        error::build_inconsistent_fs($ino, function_name!())
    };
}

/// The time-to-live seconds of FUSE attributes
const MY_TTL_SEC: u64 = 3600; // TODO: should be a long value, say 1 hour
/// The generation ID of FUSE attributes
const MY_GENERATION: u64 = 1; // TODO: find a proper way to set generation
#[allow(dead_code)]
/// The limit of transaction commit retrying times.
const TXN_RETRY_LIMIT: u32 = 5;

/// File system in-memory meta-data
#[derive(Debug)]
#[allow(dead_code)]
pub struct S3MetaData<S: S3BackEnd + Send + Sync + 'static> {
    /// S3 backend
    pub(crate) s3_backend: Arc<S>,
    /// Global data cache
    pub(crate) data_cache: Arc<GlobalCache>,
    /// Current available fd, it'll increase after using
    pub(crate) cur_fd: AtomicU32,
    /// Current service id
    pub(crate) node_id: Arc<str>,
    /// Storage config
    pub(crate) storage_config: Arc<StorageConfig>,
    /// Fuse fd
    fuse_fd: Mutex<RawFd>,
    /// KV engine
    pub(crate) kv_engine: Arc<KVEngineType>,
    /// Inum allocator
    inum_allocator: INumAllocator<KVEngineType>,
}

#[async_trait]
impl<S: S3BackEnd + Sync + Send + 'static> MetaData for S3MetaData<S> {
    type N = S3Node<S>;

    #[instrument(skip(self))]
    async fn release(
        &self,
        ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        flush: bool,
    ) -> DatenLordResult<()> {
        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let mut inode = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            inode.close(ino, fh, flush).await;
            txn.set(
                &KeyType::INum2Node(ino),
                &ValueType::Node(inode.into_serial_node()),
            );
            (txn.commit().await, ())
        })?;
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    async fn readdir(
        &self,
        context: ReqContext,
        ino: u64,
        _fh: u64,
        offset: i64,
        reply: &mut ReplyDirectory,
    ) -> DatenLordResult<()> {
        let mut readdir_helper = |data: &BTreeMap<String, DirEntry>| -> usize {
            let mut num_child_entries = 0;
            for (i, (child_name, child_entry)) in data.iter().enumerate().skip(offset.cast()) {
                let child_ino = child_entry.ino();
                reply.add(
                    child_ino,
                    offset.overflow_add(i.cast()).overflow_add(1), /* i + 1 means the index of
                                                                    * the next entry */
                    child_entry.entry_type(),
                    child_name,
                );
                num_child_entries = num_child_entries.overflow_add(1);
                debug!(
                    "readdir() found one child of ino={}, name={:?}, offset={}, and entry={:?} \
                        under the directory of ino={}",
                    child_ino,
                    child_name,
                    offset.overflow_add(i.cast()).overflow_add(1),
                    child_entry,
                    ino,
                );
            }
            num_child_entries
        };

        let inode = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;
        inode
            .get_attr()
            .check_perm(context.user_id, context.group_id, 5)?;
        if inode.need_load_dir_data() {
            inode.load_data(0_usize, 0_usize).await?;
        }
        let num_child_entries = inode.read_dir(&mut readdir_helper);
        debug!(
            "readdir() successfully read {} entries \
                under the directory of ino={} and name={:?}",
            num_child_entries,
            ino,
            inode.get_name(),
        );
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    async fn opendir(&self, context: ReqContext, ino: u64, flags: u32) -> DatenLordResult<RawFd> {
        let result = retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let node = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            let o_flags = fs_util::parse_oflag(flags);
            node.open_pre_check(o_flags, context.user_id, context.group_id)?;

            let result = node.dup_fd(o_flags).await?;
            txn.set(
                &KeyType::INum2Node(ino),
                &ValueType::Node(node.into_serial_node()),
            );
            (txn.commit().await, result)
        })?;
        Ok(result)
    }

    #[instrument(skip(self))]
    async fn readlink(&self, ino: u64) -> DatenLordResult<Vec<u8>> {
        let node = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;
        Ok(node.get_symlink_target().as_os_str().to_owned().into_vec())
    }

    #[instrument(skip(self), err, ret)]
    async fn statfs(&self, context: ReqContext, ino: u64) -> DatenLordResult<StatFsParam> {
        let node = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;
        node.get_attr()
            .check_perm(context.user_id, context.group_id, 5)?;
        node.statefs().await
    }

    #[instrument(skip(self))]
    async fn flush(&self, ino: u64, fh: u64) -> DatenLordResult<()> {
        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let mut inode = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            inode.flush(ino, fh).await;
            txn.set(
                &KeyType::INum2Node(ino),
                &ValueType::Node(inode.into_serial_node()),
            );
            (txn.commit().await, ())
        })?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn releasedir(&self, ino: u64, fh: u64) -> DatenLordResult<()> {
        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let node = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            node.closedir(ino, fh).await;
            let is_deleted = self.delete_check(&node).await?;
            if is_deleted {
                txn.delete(&KeyType::INum2Node(ino));
            } else {
                txn.set(
                    &KeyType::INum2Node(ino),
                    &ValueType::Node(node.into_serial_node()),
                );
            }
            (txn.commit().await, ())
        })?;
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    async fn read_helper(
        &self,
        ino: INum,
        _fh: u64,
        offset: i64,
        size: u32,
    ) -> DatenLordResult<Vec<IoMemBlock>> {
        let inode = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;

        let size: u64 =
            if offset.cast::<u64>().overflow_add(size.cast::<u64>()) > inode.get_attr().size {
                inode.get_attr().size.overflow_sub(offset.cast::<u64>())
            } else {
                size.cast()
            };

        if inode.need_load_file_data(offset.cast(), size.cast()).await {
            inode.load_data(offset.cast(), size.cast()).await?;
        }
        return Ok(inode.get_file_data(offset.cast(), size.cast()).await);
    }

    #[instrument(skip(self), err, ret)]
    async fn open(&self, context: ReqContext, ino: u64, flags: u32) -> DatenLordResult<RawFd> {
        // TODO: handle open flags
        // <https://pubs.opengroup.org/onlinepubs/9699919799/functions/open.html>
        // let open_res = if let SFlag::S_IFLNK = node.get_type() {
        //     node.open_symlink_target(o_flags).await.add_context(format!(
        //         "open() failed to open symlink target={:?} with flags={}",
        //         node.get_symlink_target(),
        //         flags,
        //     ))
        // } else {
        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let node = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            let o_flags = fs_util::parse_oflag(flags);
            node.open_pre_check(o_flags, context.user_id, context.group_id)?;

            let result = node.dup_fd(o_flags).await;
            txn.set(
                &KeyType::INum2Node(ino),
                &ValueType::Node(node.into_serial_node()),
            );
            (txn.commit().await, result)
        })?
    }

    #[instrument(skip(self), err, ret)]
    async fn getattr(&self, ino: u64) -> DatenLordResult<(Duration, FuseAttr)> {
        let inode = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;
        let attr = inode.get_attr();
        let ttl = Duration::new(MY_TTL_SEC, 0);
        let fuse_attr = fs_util::convert_to_fuse_attr(attr);
        Ok((ttl, fuse_attr))
    }

    #[instrument(skip(self))]
    async fn forget(&self, ino: u64, nlookup: u64) -> DatenLordResult<()> {
        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let inode = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            inode.dec_lookup_count_by(nlookup);
            let is_deleted = self.delete_check(&inode).await?;
            if is_deleted {
                txn.delete(&KeyType::INum2Node(ino));
            } else {
                txn.set(
                    &KeyType::INum2Node(ino),
                    &ValueType::Node(inode.into_serial_node()),
                );
            }
            (txn.commit().await, ())
        })?;
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    async fn setattr_helper(
        &self,
        context: ReqContext,
        ino: u64,
        param: &SetAttrParam,
    ) -> DatenLordResult<(Duration, FuseAttr)> {
        let ttl = Duration::new(MY_TTL_SEC, 0);
        let file_attr = retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let mut inode = self.get_inode_from_txn(txn.as_mut(), ino).await?;
            let (attr_changed, file_attr) = inode
                .setattr_precheck(param, context.user_id, context.group_id)
                .await?;
            debug!("setattr_helper() attr_changed={}", attr_changed);
            if attr_changed {
                inode.set_attr(file_attr);
            }
            txn.set(
                &KeyType::INum2Node(ino),
                &ValueType::Node(inode.into_serial_node()),
            );
            (txn.commit().await, file_attr)
        })?;
        Ok((ttl, fs_util::convert_to_fuse_attr(file_attr)))
    }

    #[instrument(skip(self), err, ret)]
    async fn unlink(&self, context: ReqContext, parent: INum, name: &str) -> DatenLordResult<()> {
        let entry_type = {
            let parent_node = self.get_node_from_kv_engine(parent).await?.ok_or_else(|| {
                error::build_inconsistent_fs_with_context(
                    function_name!(),
                    format!("parent of ino={parent} should be in cache before remove its child"),
                )
            })?;
            let child_entry = parent_node.get_entry(name).ok_or_else(|| error::build_inconsistent_fs_with_context(
                function_name!(),
                format!("the child entry name={name:?} to remove is not under parent of ino={parent}"
                )
            ))?;
            let entry_type = child_entry.entry_type();
            debug_assert_ne!(
                SFlag::S_IFDIR,
                entry_type,
                "unlink() should not remove sub-directory name={name:?} under parent ino={parent}",
            );
            entry_type
        };

        self.remove_node_helper(context, parent, name, entry_type)
            .await
    }

    async fn new(
        capacity: usize,
        ip: &str,
        port: u16,
        kv_engine: Arc<KVEngineType>,
        node_id: &str,
        storage_config: &StorageConfig,
    ) -> DatenLordResult<(Arc<Self>, Option<CacheServer>)> {
        let s3_config = match storage_config.params {
            StorageParams::S3(ref config) => config,
            StorageParams::None(ref fake_s3_config) => fake_s3_config,
        };
        let bucket_name = &s3_config.bucket_name;
        let endpoint = &s3_config.endpoint_url;
        let access_key = &s3_config.access_key_id;
        let secret_key = &s3_config.secret_access_key;

        let s3_backend = Arc::new(
            S::new_backend(bucket_name, endpoint, access_key, secret_key)
                .await
                .context("Failed to create s3 backend.")?,
        );
        let data_cache = Arc::new(GlobalCache::new_dist_with_bz_and_capacity(
            10_485_760, // 10 * 1024 * 1024
            capacity,
            Arc::clone(&kv_engine),
            node_id,
        ));

        let meta = Arc::new(Self {
            s3_backend: Arc::clone(&s3_backend),
            data_cache: Arc::<GlobalCache>::clone(&data_cache),
            cur_fd: AtomicU32::new(4),
            node_id: Arc::<str>::from(node_id.to_owned()),
            storage_config: Arc::<StorageConfig>::from(storage_config.clone()),
            fuse_fd: Mutex::new(-1_i32),
            inum_allocator: INumAllocator::new(Arc::clone(&kv_engine)),
            kv_engine,
        });

        let server = CacheServer::new(ip.to_owned(), port.to_owned(), data_cache);

        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = meta.kv_engine.new_meta_txn().await;
            let prev = meta
                .try_get_inode_from_txn(txn.as_mut(), FUSE_ROOT_ID)
                .await?;
            if let Some(prev_root_node) = prev {
                info!(
                    "[init] root node already exists root_node file_attr {:?}, skip init",
                    prev_root_node.get_attr()
                );
                // We already see a prev root node, we don't have write operation
                // Txn is not needed for such read-only operation
                (Ok(true), ())
            } else {
                info!("[init] root node not exists, init root node");
                let root_inode = S3Node::open_root_node(
                    FUSE_ROOT_ID,
                    "/",
                    Arc::<S>::clone(&s3_backend),
                    Arc::clone(&meta),
                )
                .await
                .add_context("failed to open FUSE root node")?;
                // insert (FUSE_ROOT_ID -> root_inode) into KV engine
                txn.set(
                    &KeyType::INum2Node(FUSE_ROOT_ID),
                    &ValueType::Node(root_inode.into_serial_node()),
                );
                (txn.commit().await, ())
            }
        })?;
        Ok((meta, Some(server)))
    }

    /// Set fuse fd into `MetaData`
    #[tracing::instrument(skip(self))]
    async fn set_fuse_fd(&self, fuse_fd: RawFd) {
        *self.fuse_fd.lock().await = fuse_fd;
    }

    #[instrument(skip(self, node), ret)]
    /// Try to delete node that is marked as deferred deletion
    async fn delete_check(&self, node: &S3Node<S>) -> DatenLordResult<bool> {
        let is_deleted = if node.get_open_count() == 0 && node.get_lookup_count() == 0 {
            if let SFlag::S_IFREG = node.get_type() {
                self.data_cache.remove_file_cache(node.get_ino()).await;
            }
            true
        } else {
            false
        };
        debug!(
            "try_delete_node()  is_deleted={} i-node of ino={} and name={:?} open_count={} lookup_count={}",
            is_deleted,
            node.get_ino(),
            node.get_name(),
            node.get_open_count(),
            node.get_lookup_count(),
        );
        Ok(is_deleted)
    }

    #[instrument(skip(self), err, ret)]
    // Create and open a file
    // If the file does not exist, first create it with
    // the specified mode, and then open it.
    #[allow(clippy::too_many_lines)]
    async fn mknod(&self, param: CreateParam) -> DatenLordResult<(Duration, FuseAttr, u64)> {
        check_name_length(&param.name)?;
        check_type_supported(&param.node_type)?;
        let parent_ino = param.parent;
        // pre-check : check whether the child name is valid
        let mut parent_node = self
            .get_node_from_kv_engine(parent_ino)
            .await?
            .ok_or_else(|| {
                error::build_inconsistent_fs_with_context(
                    function_name!(),
                    format!(
                        "parent of ino={parent_ino} should be in cache before create its child"
                    ),
                )
            })?;
        parent_node.check_name_availability(&param.name)?;
        // allocate a new i-node number
        let new_inum = self.alloc_inum().await?;

        let new_node = parent_node
            .create_child_node(
                &param,
                new_inum,
                Arc::<GlobalCache>::clone(&self.data_cache),
            )
            .await?;

        let new_ino = new_node.get_ino();
        let new_node_attr = new_node.get_attr();
        let fuse_attr = fs_util::convert_to_fuse_attr(new_node_attr);
        self.set_node_to_kv_engine(new_ino, new_node).await?;
        self.set_node_to_kv_engine(parent_ino, parent_node).await?;

        let ttl = Duration::new(MY_TTL_SEC, 0);
        Ok((ttl, fuse_attr, MY_GENERATION))
    }

    #[instrument(skip(self), err, ret)]
    /// Helper function to remove node
    async fn remove_node_helper(
        &self,
        context: ReqContext,
        parent: INum,
        node_name: &str,
        node_type: SFlag,
    ) -> DatenLordResult<()> {
        debug!(
            "remove_node_helper() about to remove parent ino={:?}, \
            child_name={:?}, child_type={:?}",
            parent, node_name, node_type
        );
        self.remove_node_local(context, parent, node_name, node_type, false)
            .await?;
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    /// Helper function to lookup
    #[allow(clippy::too_many_lines)]
    async fn lookup_helper(
        &self,
        context: ReqContext,
        parent: INum,
        child_name: &str,
    ) -> DatenLordResult<(Duration, FuseAttr, u64)> {
        let pre_check_res = self
            .lookup_pre_check(parent, child_name, context.user_id, context.group_id)
            .await;
        let (child_ino, _, _) = match pre_check_res {
            Ok((ino, child_type, child_attr)) => (ino, child_type, child_attr),
            Err(e) => {
                debug!("lookup() failed to pre-check, the error is: {:?}", e);
                return Err(e);
            }
        };

        let ttl = Duration::new(MY_TTL_SEC, 0);
        let child_node = self
            .get_node_from_kv_engine(child_ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(child_ino))?;
        let attr = child_node.lookup_attr();
        debug!(
            "ino={} lookup_count={} lookup_attr={:?}",
            child_ino,
            child_node.get_lookup_count(),
            attr
        );
        self.set_node_to_kv_engine(child_ino, child_node).await?;
        let fuse_attr = fs_util::convert_to_fuse_attr(attr);
        Ok((ttl, fuse_attr, MY_GENERATION))
    }

    /// Rename helper for exchange rename
    #[instrument(skip(self), err, ret)]
    #[cfg(feature = "abi-7-23")]
    async fn rename_exchange_helper(
        &self,
        context: ReqContext,
        param: RenameParam,
    ) -> DatenLordResult<()> {
        let old_parent = param.old_parent;
        let old_name = param.old_name.as_str();
        let new_parent = param.new_parent;
        let new_name = param.new_name.as_str();
        let flags = param.flags;
        let no_replace = flags == 1; // RENAME_NOREPLACE

        if no_replace {
            return build_error_result_from_errno(
                Errno::EINVAL,
                "Both RENAME_NOREPLACE and RENAME_EXCHANGE were specified in flags.".into(),
            );
        }

        retry_txn!(TXN_RETRY_LIMIT, {
            let mut txn = self.kv_engine.new_meta_txn().await;
            let check_res = self
                .exchange_pre_check(
                    txn.as_mut(),
                    &context,
                    old_parent,
                    old_name,
                    new_parent,
                    new_name,
                )
                .await?;
            let (mut old_parent_node, old_ino, new_parent_node, new_ino) = check_res;

            // If the old node is the same file as the new node, do nothing
            if old_ino == new_ino {
                return Ok(());
            }

            let mut old_node = self.get_inode_from_txn(txn.as_mut(), old_ino).await?;
            let mut new_node = self.get_inode_from_txn(txn.as_mut(), new_ino).await?;

            old_node.set_parent_ino(new_parent);
            new_node.set_parent_ino(old_parent);
            old_node.set_name(new_name);
            new_node.set_name(old_name);

            txn.set(
                &KeyType::INum2Node(old_ino),
                &ValueType::Node(old_node.into_serial_node()),
            );
            txn.set(
                &KeyType::INum2Node(new_ino),
                &ValueType::Node(new_node.into_serial_node()),
            );

            if let Some(mut new_parent_node) = new_parent_node {
                // The two parent node is not the same node
                let old_entry = old_parent_node.get_dir_data_mut().remove(old_name).unwrap_or_else(||unreachable!("Impossible case when exchange,\
                                                                                                                                    as {old_name} under {old_parent} is checked to be existed."));
                let new_entry = new_parent_node.get_dir_data_mut().remove(new_name).unwrap_or_else(||unreachable!("Impossible case when exchange,\
                                                                                                                                    as {new_name} under {new_parent} is checked to be existed."));
                old_parent_node.insert_entry_for_rename(DirEntry::new(
                    old_name.into(),
                    Arc::clone(new_entry.file_attr_arc_ref()),
                ));
                new_parent_node.insert_entry_for_rename(DirEntry::new(
                    new_name.into(),
                    Arc::clone(old_entry.file_attr_arc_ref()),
                ));

                txn.set(
                    &KeyType::INum2Node(old_parent),
                    &ValueType::Node(old_parent_node.into_serial_node()),
                );
                txn.set(
                    &KeyType::INum2Node(new_parent),
                    &ValueType::Node(new_parent_node.into_serial_node()),
                );
            } else {
                let old_entry = old_parent_node.get_dir_data_mut().remove(old_name).unwrap_or_else(||unreachable!("Impossible case when exchange,\
                                                                                                                                    as {old_name} under {old_parent} is checked to be existed."));
                let new_entry = old_parent_node.get_dir_data_mut().remove(new_name).unwrap_or_else(||unreachable!("Impossible case when exchange,\
                                                                                                                                    as {new_name} under {new_parent} is checked to be existed."));

                old_parent_node.insert_entry_for_rename(DirEntry::new(
                    old_name.into(),
                    Arc::clone(new_entry.file_attr_arc_ref()),
                ));
                old_parent_node.insert_entry_for_rename(DirEntry::new(
                    new_name.into(),
                    Arc::clone(old_entry.file_attr_arc_ref()),
                ));

                txn.set(
                    &KeyType::INum2Node(old_parent),
                    &ValueType::Node(old_parent_node.into_serial_node()),
                );
            }

            (txn.commit().await, ())
        })
    }

    #[instrument(skip(self), err, ret)]
    /// Rename helper to move on disk, it may replace destination entry
    async fn rename_may_replace_helper(
        &self,
        context: ReqContext,
        param: RenameParam,
    ) -> DatenLordResult<()> {
        self.rename_may_replace_local(context, &param, false)
            .await?;
        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    /// Helper function of fsync
    async fn fsync_helper(
        &self,
        ino: u64,
        fh: u64,
        _datasync: bool,
        // reply: ReplyEmpty,
    ) -> DatenLordResult<()> {
        let mut inode = self
            .get_node_from_kv_engine(ino)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(ino))?;

        inode.flush(ino, fh).await;

        Ok(())
    }

    #[instrument(skip(self), err, ret)]
    /// Helper function to write data
    async fn write_helper(
        &self,
        ino: u64,
        fh: u64,
        offset: i64,
        data: Vec<u8>,
        flags: u32,
    ) -> DatenLordResult<usize> {
        let data_len = data.len();
        let (result, _) = {
            let mut inode = self
                .get_node_from_kv_engine(ino)
                .await?
                .ok_or_else(|| build_inconsistent_fs!(ino))?;
            let parent_ino = inode.get_parent_ino();

            debug!(
                "write_helper() about to write {} byte data to file of ino={} \
                and name {:?} at offset={}",
                data.len(),
                ino,
                inode.get_name(),
                offset
            );
            let o_flags = fs_util::parse_oflag(flags);
            let write_to_disk = true;
            let res = inode
                .write_file(fh, offset, data, o_flags, write_to_disk)
                .await;
            self.set_node_to_kv_engine(ino, inode).await?;
            (res, parent_ino)
        };
        self.invalidate_remote(ino, offset, data_len).await?;
        result
    }
}

impl<S: S3BackEnd + Send + Sync + 'static> S3MetaData<S> {
    #[allow(clippy::unwrap_used)]
    /// Get a node from kv engine by inum
    pub async fn get_node_from_kv_engine(&self, inum: INum) -> DatenLordResult<Option<S3Node<S>>> {
        let inum_key = KeyType::INum2Node(inum);
        let raw_data = self.kv_engine.get(&inum_key).await.add_context(format!(
            "{}() failed to get node of ino={inum} from kv engine",
            function_name!()
        ))?;

        // deserialize node
        Ok(match raw_data {
            Some(r) => Some(r.into_s3_node(self).await?),
            None => None,
        })
    }

    /// Set node to kv engine use inum
    pub async fn set_node_to_kv_engine(&self, inum: INum, node: S3Node<S>) -> DatenLordResult<()> {
        let inum_key = KeyType::INum2Node(inum);
        let node_value = ValueType::Node(node.into_serial_node());
        self.kv_engine
            .set(&inum_key, &node_value, None)
            .await
            .add_context(format!(
                "{}() failed to set node of ino={inum} to kv engine",
                function_name!()
            ))?;

        Ok(())
    }

    /// Remove node from kv engine use inum
    pub async fn remove_node_from_kv_engine(&self, inum: INum) -> DatenLordResult<()> {
        self.kv_engine
            .delete(&KeyType::INum2Node(inum), None)
            .await
            .add_context(format!(
                "{}() failed to remove node of ino={inum} from kv engine",
                function_name!()
            ))?;

        Ok(())
    }

    /// Helper function to pre-check if node can be deferred deleted.
    fn deferred_delete_pre_check(inode: &S3Node<S>) -> (bool, INum, String) {
        debug_assert!(inode.get_lookup_count() >= 0); // lookup count cannot be negative
        debug_assert!(inode.get_open_count() >= 0);
        // pre-check whether deferred delete or not
        (
            inode.get_lookup_count() > 0 || inode.get_open_count() > 0,
            inode.get_parent_ino(),
            inode.get_name().to_owned(),
        )
    }

    /// Helper function to delete or deferred delete node
    async fn may_deferred_delete_node_helper(
        &self,
        ino: INum,
        from_remote: bool,
    ) -> DatenLordResult<()> {
        // remove entry from parent i-node
        let inode = self.get_node_from_kv_engine(ino).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "{}() failed to \
                         find the i-node of ino={ino} to remove",
                function_name!()
            )
        })?;
        let (deferred_deletion, parent_ino, node_name) = Self::deferred_delete_pre_check(&inode);
        let mut parent_node = self
            .get_node_from_kv_engine(parent_ino)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{}() failed to \
                     find the parent of ino={parent_ino} for i-node of ino={ino}",
                    function_name!()
                )
            })?;
        let deleted_entry = parent_node
            .unlink_entry(&node_name)
            .await
            .add_context(format!(
                "{}() failed to remove entry name={node_name:?} \
                 and ino={ino} from parent directory ino={parent_ino}",
                function_name!()
            ))?;
        debug!(
            "may_deferred_delete_node_helper() successfully remove entry name={:?} \
                 ino={} from parent directory ino={}",
            node_name, ino, parent_ino
        );
        debug_assert_eq!(node_name, deleted_entry.entry_name());
        debug_assert_eq!(deleted_entry.ino(), ino);

        if deferred_deletion {
            // Deferred deletion
            let inode = self.get_node_from_kv_engine(ino).await?.unwrap_or_else(|| {
                unreachable!(
                    "impossible case, may_deferred_delete_node_helper() \
                     i-node of ino={ino} is not in cache",
                );
            });
            debug!(
                "may_deferred_delete_node_helper() deferred removed \
                    the i-node name={:?} of ino={} under parent ino={}, \
                    open count={}, lookup count={}",
                inode.get_name(),
                ino,
                parent_ino,
                inode.get_open_count(),
                inode.get_lookup_count(),
            );
            inode.mark_deferred_deletion();
            // Notify kernel to drop cache
            if from_remote && inode.get_lookup_count() > 0 {
                let fuse_fd = *self.fuse_fd.lock().await;
                // fuse_fd must be set
                assert!(fuse_fd > 0_i32);
                #[cfg(feature = "abi-7-18")]
                {
                    let fuse_delete_notification = FuseDeleteNotification::new(fuse_fd);
                    fuse_delete_notification
                        .notify(parent_ino, ino, inode.get_name().to_owned())
                        .await?;
                }
            }
        } else {
            // immediate deletion
            self.remove_node_from_kv_engine(ino).await?;
        }
        self.set_node_to_kv_engine(parent_ino, parent_node).await?;
        Ok(())
    }

    /// Lookup helper function to pre-check
    async fn lookup_pre_check(
        &self,
        parent: INum,
        name: &str,
        user_id: u32,
        group_id: u32,
    ) -> DatenLordResult<(INum, SFlag, Arc<SyncRwLock<FileAttr>>)> {
        // lookup child ino and type first
        let parent_node = self
            .get_node_from_kv_engine(parent)
            .await?
            .ok_or_else(|| build_inconsistent_fs!(parent))?;
        parent_node.get_attr().check_perm(user_id, group_id, 1)?;
        if let Some(child_entry) = parent_node.get_entry(name) {
            let ino = child_entry.ino();
            let child_type = child_entry.entry_type();
            Ok((ino, child_type, Arc::clone(child_entry.file_attr_arc_ref())))
        } else {
            debug!(
                "lookup_helper() failed to find the file name={:?} \
                    under parent directory of ino={} and name={:?}",
                name,
                parent,
                parent_node.get_name(),
            );
            // lookup() didn't find anything, this is normal
            build_error_result_from_errno(
                Errno::ENOENT,
                format!(
                    "lookup_helper() failed to find the file name={:?} \
                        under parent directory of ino={} and name={:?}",
                    name,
                    parent,
                    parent_node.get_name(),
                ),
            )
        }
    }

    /// Helper function to pre-check for exchange rename.
    ///
    /// This function ensures:
    /// 1. The old parent, old entry, the new parent and the new entry exist.
    /// 2. Both the old parent and the new parent are directories.
    /// 3. The user renaming the file is permitted to do this operation when the
    ///    sticky bit of one of the old entry or the new entry is set.
    ///
    /// When all checks above passed,
    /// this function returns a tuple containing:
    /// - A `S3Node` of old parent
    /// - The ino of old entry
    /// - A `S3Node` of new parent, or `None` if the new parent is same as old
    ///   parent
    /// - The ino of new entry
    ///
    /// Otherwise, it returns an `Err`.
    #[cfg(feature = "abi-7-23")]
    async fn exchange_pre_check<T: MetaTxn + ?Sized>(
        &self,
        txn: &mut T,
        context: &ReqContext,
        old_parent: INum,
        old_name: &str,
        new_parent: INum,
        new_name: &str,
    ) -> DatenLordResult<(S3Node<S>, INum, Option<S3Node<S>>, INum)> {
        let check_node_is_dir = |node: &S3Node<S>| {
            if node.get_type() == SFlag::S_IFDIR {
                Ok(())
            } else {
                build_error_result_from_errno(
                    Errno::ENOTDIR,
                    format!("{} is not a dir.", node.get_ino()),
                )
            }
        };

        let build_enoent = |name: &str, parent: INum, parent_node: &S3Node<S>| {
            build_error_result_from_errno(
                Errno::ENOENT,
                format!(
                    "exchange_pre_check() failed to find child entry of name={:?} \
                        under parent directory ino={} and name={:?}",
                    name,
                    parent,
                    parent_node.get_name(),
                ),
            )
        };

        let old_parent_node = txn
            .get(&KeyType::INum2Node(old_parent))
            .await?
            .ok_or_else(|| {
                anyhow::Error::new(Errno::ENOENT)
                    .context(format!("Old parent {old_parent} does not exist."))
            })?
            .into_s3_node(self)
            .await?;
        check_node_is_dir(&old_parent_node)?;

        let old_entry_ino = match old_parent_node.get_entry(old_name) {
            None => {
                debug!(
                    "exchange() failed to find child entry of name={:?} under parent directory ino={} and name={:?}",
                    old_name, old_parent, old_parent_node.get_name(),
                );
                return build_enoent(old_name, old_parent, &old_parent_node);
            }
            Some(old_entry) => {
                Self::check_sticky_bit(context, &old_parent_node, old_entry)?;
                debug_assert_eq!(&old_name, &old_entry.entry_name());
                old_entry.ino()
            }
        };

        let new_parent_node = if old_parent == new_parent {
            None
        } else {
            let new_parent_node = txn
                .get(&KeyType::INum2Node(new_parent))
                .await?
                .ok_or_else(|| {
                    anyhow::Error::new(Errno::ENOENT)
                        .context(format!("New parent {new_parent} does not exist."))
                })?
                .into_s3_node(self)
                .await?;

            check_node_is_dir(&new_parent_node)?;

            Some(new_parent_node)
        };

        let new_parent_ref = new_parent_node.as_ref().unwrap_or(&old_parent_node);

        let new_entry_ino = match new_parent_ref.get_entry(new_name) {
            None => {
                debug!(
                    "exchange() failed to find child entry of name={:?} under parent directory ino={} and name={:?}",
                    new_name, new_parent, new_parent_ref.get_name(),
                );
                return build_enoent(new_name, new_parent, new_parent_ref);
            }
            Some(new_entry) => {
                Self::check_sticky_bit(context, new_parent_ref, new_entry)?;
                debug_assert_eq!(&new_name, &new_entry.entry_name());
                new_entry.ino()
            }
        };
        Ok((
            old_parent_node,
            old_entry_ino,
            new_parent_node,
            new_entry_ino,
        ))
    }

    /// Rename helper function to pre-check
    ///
    /// This function ensures:
    /// 1. The old parent, old entry (`old_name`) and the new parent exists.
    /// 2. The user renaming the file is permitted to do this operation when the
    ///    sticky bit of one of the old entry or the new entry (if exists) is
    ///    set.
    /// 3. The new entry does not exists, or the `no_replace` is false.
    ///
    /// When all checks above passed,
    /// this function returns a tuple containing the fd of old parent,
    /// the ino of old node,
    /// the fd of new parent and the ino of new node (if exists).
    ///
    /// Otherwise, it returns an `Err`.
    async fn rename_pre_check(
        &self,
        context: ReqContext,
        old_parent: INum,
        old_name: &str,
        new_parent: INum,
        new_name: &str,
        no_replace: bool,
    ) -> DatenLordResult<(RawFd, INum, RawFd, Option<INum>)> {
        let old_parent_node = self
            .get_node_from_kv_engine(old_parent)
            .await?
            .ok_or_else(|| {
                error::build_inconsistent_fs_with_context(
                    function_name!(),
                    format!("the parent i-node of ino={old_parent} should be in cache"),
                )
            })?;
        let old_parent_fd = old_parent_node.get_fd();
        let old_entry_ino = match old_parent_node.get_entry(old_name) {
            None => {
                debug!(
                    "rename() failed to find child entry of name={:?} under parent directory ino={} and name={:?}",
                    old_name, old_parent, old_parent_node.get_name(),
                );
                return build_error_result_from_errno(
                    Errno::ENOENT,
                    format!(
                        "rename_pre_check() failed to find child entry of name={:?} \
                            under parent directory ino={} and name={:?}",
                        old_name,
                        old_parent,
                        old_parent_node.get_name(),
                    ),
                );
            }
            Some(old_entry) => {
                Self::check_sticky_bit(&context, &old_parent_node, old_entry)?;
                debug_assert_eq!(&old_name, &old_entry.entry_name());
                old_entry.ino()
            }
        };

        let new_parent_node = self
            .get_node_from_kv_engine(new_parent)
            .await?
            .ok_or_else(|| {
                error::build_inconsistent_fs_with_context(
                    function_name!(),
                    format!("the new parent i-node of ino={new_parent} should be in cache"),
                )
            })?;
        let new_parent_fd = new_parent_node.get_fd();
        let new_entry_ino = if let Some(new_entry) = new_parent_node.get_entry(new_name) {
            Self::check_sticky_bit(&context, &new_parent_node, new_entry)?;
            debug_assert_eq!(&new_name, &new_entry.entry_name());
            let new_ino = new_entry.ino();
            if no_replace {
                debug!(
                    "rename() found i-node of ino={} and name={:?} under new parent ino={} and name={:?}, \
                        but RENAME_NOREPLACE is specified",
                    new_ino, new_name, new_parent, new_parent_node.get_name(),
                );
                return build_error_result_from_errno(
                    Errno::EEXIST, // RENAME_NOREPLACE
                    format!(
                        "rename() found i-node of ino={} and name={:?} under new parent ino={} and name={:?}, \
                            but RENAME_NOREPLACE is specified",
                        new_ino, new_name, new_parent, new_parent_node.get_name(),
                    ),
                );
            }
            debug!(
                "rename() found the new parent directory of ino={} and name={:?} already has a child with name={:?}",
                new_parent, new_parent_node.get_name(), new_name,
            );
            Some(new_ino)
        } else {
            None
        };
        debug!(
            "rename() pre-check passed, old parent ino={}, old name={:?}, new parent ino={}, new name={:?}, \
                old entry ino={}, new entry ino={:?}",
            old_parent, old_name, new_parent, new_name, old_entry_ino, new_entry_ino,
        );
        Ok((old_parent_fd, old_entry_ino, new_parent_fd, new_entry_ino))
    }

    /// Rename in cache helper
    async fn rename_in_cache_helper(
        &self,
        old_parent: INum,
        old_name: &str,
        new_parent: INum,
        new_name: &str,
    ) -> DatenLordResult<Option<DirEntry>> {
        let mut old_parent_node = self.get_node_from_kv_engine(old_parent).await?.unwrap_or_else(|| {
            unreachable!(
                "impossible case when rename, the from parent i-node of ino={old_parent} should be in cache",
            )
        });
        let entry_to_move = match old_parent_node.remove_entry_for_rename(old_name) {
            None => unreachable!(
                "impossible case when rename, the from entry of name={:?} \
                        should be under from directory ino={} and name={:?}",
                old_name,
                old_parent,
                old_parent_node.get_name(),
            ),
            Some(old_entry) => DirEntry::new(
                new_name.to_owned(),
                Arc::clone(old_entry.file_attr_arc_ref()),
            ),
        };
        self.set_node_to_kv_engine(old_parent, old_parent_node)
            .await?;

        // TODO : the error is: {}

        let mut new_parent_node = self.get_node_from_kv_engine(new_parent).await?.unwrap_or_else(|| {
            unreachable!(
                "impossible case when rename, the to parent i-node of ino={new_parent} should be in cache"
            )
        });
        let result = new_parent_node.insert_entry_for_rename(entry_to_move);
        self.set_node_to_kv_engine(new_parent, new_parent_node)
            .await?;
        Ok(result)
    }

    /// Rename to move on disk locally, it may replace destination entry
    async fn rename_may_replace_local(
        &self,
        context: ReqContext,
        param: &RenameParam,
        from_remote: bool,
    ) -> DatenLordResult<()> {
        let old_parent = param.old_parent;
        let old_name = &param.old_name;
        let new_parent = param.new_parent;
        let new_name = &param.new_name;
        let flags = param.flags;
        debug!(
            "rename(old parent={}, old name={:?}, new parent={}, new name={:?})",
            old_parent, old_name, new_parent, new_name,
        );
        let no_replace = flags == 1; // RENAME_NOREPLACE

        let pre_check_res = self
            .rename_pre_check(
                context, old_parent, old_name, new_parent, new_name, no_replace,
            )
            .await;
        let (_, old_entry_ino, _, new_entry_ino) = match pre_check_res {
            Ok((old_parent_fd, old_entry_ino, new_parent_fd, new_entry_ino)) => {
                (old_parent_fd, old_entry_ino, new_parent_fd, new_entry_ino)
            }
            Err(e) => {
                debug!("rename() pre-check failed, the error is: {:?}", e);
                return Err(e);
            }
        };

        // Just replace new entry, may deferred delete
        if let Some(new_ino) = new_entry_ino {
            self.may_deferred_delete_node_helper(new_ino, from_remote)
                .await
                .add_context(format!(
                    "{}() failed to \
                        maybe deferred delete the replaced i-node ino={new_ino}",
                    function_name!()
                ))?;
        }

        {
            let mut moved_node = self.get_node_from_kv_engine(old_entry_ino).await?.unwrap_or_else(|| {
                unreachable!(
                    "impossible case when rename, the from entry i-node of ino={old_entry_ino} should be in cache",
                )
            });
            moved_node.set_parent_ino(new_parent);
            moved_node.set_name(new_name);
            debug!(
                "rename_may_replace_local() successfully moved the from i-node \
                of ino={} and name={:?} under from parent ino={} to \
                the to i-node of ino={} and name={:?} under to parent ino={}",
                old_entry_ino, old_name, old_parent, old_entry_ino, new_name, new_parent,
            );
            self.set_node_to_kv_engine(old_entry_ino, moved_node)
                .await?;
        };

        let rename_replace_res = self
            .rename_in_cache_helper(old_parent, old_name, new_parent, new_name)
            .await?;
        debug_assert!(
            rename_replace_res.is_none(),
            "rename_may_replace_local() should already have \
                deleted the target i-node to be replaced",
        );
        Ok(())
    }

    /// Helper function to remove node locally
    pub(crate) async fn remove_node_local(
        &self,
        context: ReqContext,
        parent: INum,
        node_name: &str,
        node_type: SFlag,
        from_remote: bool,
    ) -> DatenLordResult<()> {
        let node_ino: INum;
        {
            // pre-checks
            let parent_node = self.get_node_from_kv_engine(parent).await?.ok_or_else(|| {
                error::build_inconsistent_fs_with_context(
                    function_name!(),
                    format!("parent of ino={parent} should be in cache before remove its child"),
                )
            })?;
            match parent_node.get_entry(node_name) {
                None => {
                    debug!(
                        "remove_node_local() failed to find i-node name={:?} \
                            under parent of ino={}",
                        node_name, parent,
                    );
                    return build_error_result_from_errno(
                        Errno::ENOENT,
                        format!(
                            "remove_node_local() failed to find i-node name={node_name:?} \
                                under parent of ino={parent}",
                        ),
                    );
                }
                Some(child_entry) => {
                    Self::check_sticky_bit(&context, &parent_node, child_entry)?;
                    node_ino = child_entry.ino();
                    if let SFlag::S_IFDIR = node_type {
                        // check the directory to delete is empty
                        let dir_node =
                            self.get_node_from_kv_engine(node_ino)
                                .await?
                                .ok_or_else(|| {
                                    error::build_inconsistent_fs_with_context(
                                        function_name!(),
                                        format!(
                                            "directory name={node_name:?} of ino={node_ino} \
                                found under the parent of ino={parent}, \
                                but no i-node found for this directory"
                                        ),
                                    )
                                })?;
                        if !dir_node.is_node_data_empty() {
                            debug!(
                                "remove_node_local() cannot remove \
                                    the non-empty directory name={:?} of ino={} \
                                    under the parent directory of ino={}",
                                node_name, node_ino, parent,
                            );
                            return build_error_result_from_errno(
                                Errno::ENOTEMPTY,
                                format!(
                                    "remove_node_local() cannot remove \
                                        the non-empty directory name={node_name:?} of ino={node_ino} \
                                        under the parent directory of ino={parent}",
                                ),
                            );
                        }
                    }

                    let child_node = self.get_node_from_kv_engine(node_ino)
                        .await?
                        .ok_or_else(|| error::build_inconsistent_fs_with_context(
                            function_name!(),
                            format!("i-node name={node_name:?} of ino={node_ino} found under the parent of ino={parent}, but no i-node found for this node"
                            )))?;

                    debug_assert_eq!(node_ino, child_node.get_ino());
                    debug_assert_eq!(node_name, child_node.get_name());
                    debug_assert_eq!(parent, child_node.get_parent_ino());
                    debug_assert_eq!(node_type, child_node.get_type());
                    debug_assert_eq!(node_type, child_node.get_attr().kind);
                }
            }
        }
        {
            // all checks passed, ready to remove,
            // when deferred deletion, remove entry from directory first
            self.may_deferred_delete_node_helper(node_ino, from_remote)
                .await
                .add_context(format!(
                    "{}() failed to maybe deferred delete child i-node of ino={node_ino}, \
                        name={node_name:?} and type={node_type:?} under parent ino={parent}",
                    function_name!()
                ))?;
            // reply.ok().await?;
            debug!(
                "remove_node_local() successfully removed child i-node of ino={}, \
                    name={:?} and type={:?} under parent ino={}",
                node_ino, node_name, node_type, parent,
            );
        };
        Ok(())
    }

    /// Allocate a new uinque inum for new node
    async fn alloc_inum(&self) -> DatenLordResult<INum> {
        let result = self.inum_allocator.alloc_inum_for_fnode().await;
        debug!("alloc_inum_for_fnode() result={result:?}");
        result
    }

    /// Invalidate cache from other nodes
    async fn invalidate_remote(
        &self,
        full_ino: INum,
        offset: i64,
        len: usize,
    ) -> DatenLordResult<()> {
        let volume_info = serde_json::to_string(self.storage_config.as_ref())?;

        dist_client::invalidate(
            &self.kv_engine,
            &self.node_id,
            &volume_info,
            full_ino,
            offset
                .overflow_div(self.data_cache.get_align().cast())
                .cast(),
            offset
                .overflow_add(len.cast())
                .overflow_sub(1)
                .overflow_div(self.data_cache.get_align().cast())
                .cast(),
        )
        .await
        .map_err(DatenLordError::from)
        .add_context("failed to invlidate others' cache")
    }

    /// If sticky bit is set, only the owner of the directory, the owner of the
    /// file, or the superuser can rename or delete files.
    fn check_sticky_bit(
        context: &ReqContext,
        parent_node: &S3Node<S>,
        child_entry: &DirEntry,
    ) -> DatenLordResult<()> {
        let parent_attr = parent_node.get_attr();
        if NEED_CHECK_PERM
            && context.uid!= 0
            && (parent_attr.perm & 0o1000 != 0)
            && context.uid!= parent_attr.uid
            && context.uid!= child_entry.file_attr_arc_ref().read().uid
        {
            build_error_result_from_errno(Errno::EACCES, "Sticky bit set".to_owned())
        } else {
            Ok(())
        }
    }

    /// Helper function to get inode from `MetaTxn`
    async fn try_get_inode_from_txn<T: MetaTxn + ?Sized>(
        &self,
        txn: &mut T,
        ino: INum,
    ) -> DatenLordResult<Option<S3Node<S>>> {
        let inode = txn
            .get(&KeyType::INum2Node(ino))
            .await
            .add_context(format!(
                "{}() failed to get i-node of ino={ino} from kv engine",
                function_name!()
            ))?;
        match inode {
            Some(inode) => Ok(Some(inode.into_s3_node(self).await?)),
            None => Ok(None),
        }
    }

    /// Helper function to get inode that must exist from `MetaTxn`
    async fn get_inode_from_txn<T: MetaTxn + ?Sized>(
        &self,
        txn: &mut T,
        ino: INum,
    ) -> DatenLordResult<S3Node<S>> {
        txn.get(&KeyType::INum2Node(ino))
            .await
            .add_context(format!(
                "{}() failed to get i-node of ino={ino} from kv engine",
                function_name!()
            ))?
            .ok_or_else(|| build_inconsistent_fs!(ino))? // inode must exist
            .into_s3_node(self)
            .await
    }
}
