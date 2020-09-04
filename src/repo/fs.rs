//! Persistent fs backed repo
use crate::error::Error;
use crate::repo::{BlockPut, BlockStore};
use async_trait::async_trait;
use bitswap::Block;
use cid::Cid;
use core::convert::TryFrom;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tracing_futures::Instrument;

use tokio::fs;
use tokio::sync::broadcast;

use super::{BlockRm, BlockRmError, Column, DataStore, RepoCid};
use std::hash::Hash;

use std::collections::HashMap;

/// The PinStore implementation for FsDataStore
mod pinstore;

// mod flatfs;
type ArcMutexMap<A, B> = Arc<Mutex<HashMap<A, B>>>;

/// FsDataStore which uses the filesystem as a lockable key-value store. Maintains a similar to
/// blockstore sharded two level storage. Direct have empty files, recursive pins record all of
/// their indirect descendants. Pin files are separated by their file extensions.
///
/// When modifying, single write lock is used.
///
/// For the PinStore implementation, please see `fs/pinstore.rs`.
#[derive(Debug)]
pub struct FsDataStore {
    /// The base directory under which we have a sharded directory structure, and the individual
    /// blocks are stored under the shard. See unixfs/examples/cat.rs for read example.
    path: PathBuf,

    /// Start with simple, conservative solution, allows concurrent queries but single writer.
    /// FIXME: this needs to become semaphore for owned permits; do "dirty reads" as there should
    /// not be any chance of seeing partial writes, that is, under modern linux filesystems.
    lock: tokio::sync::RwLock<()>,

    /// Not really needed
    written_bytes: AtomicU64,
}

// FIXME: need to do
//  - bring back the canonicalized version, or just force base32? should work with that as well
//  - block rm test failure
//  - refs local test failure

#[async_trait]
impl DataStore for FsDataStore {
    fn new(mut root: PathBuf) -> Self {
        root.push("pins");
        FsDataStore {
            path: root,
            lock: Default::default(),
            written_bytes: Default::default(),
        }
    }

    async fn init(&self) -> Result<(), Error> {
        tokio::fs::create_dir_all(&self.path).await?;
        Ok(())
    }

    async fn open(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn contains(&self, _col: Column, _key: &[u8]) -> Result<bool, Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    async fn get(&self, _col: Column, _key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    async fn put(&self, _col: Column, _key: &[u8], _value: &[u8]) -> Result<(), Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    async fn remove(&self, _col: Column, _key: &[u8]) -> Result<(), Error> {
        Err(anyhow::anyhow!("not implemented"))
    }

    async fn wipe(&self) {
        todo!()
    }
}

#[derive(Debug)]
pub struct FsBlockStore {
    /// The base directory under which we have a sharded directory structure, and the individual
    /// blocks are stored under the shard. See unixfs/examples/cat.rs for read example.
    path: PathBuf,

    /// Synchronize concurrent reads and writes to the same Cid.
    /// If the write ever happens, the message sent will be Ok(()), on failure it'll be an Err(()).
    /// Since this is a broadcast channel, the late arriving receiver might not get any messages.
    writes: ArcMutexMap<RepoCid, broadcast::Sender<Result<(), ()>>>,

    /// Initially used to demonstrate a bug, not really needed anymore. Could be used as a basis
    /// for periodic synching to disk to know much space we have used.
    written_bytes: AtomicU64,
}

struct RemoveOnDrop<K: Eq + Hash, V>(ArcMutexMap<K, V>, Option<K>);

impl<K: Eq + Hash, V> Drop for RemoveOnDrop<K, V> {
    fn drop(&mut self) {
        if let Some(key) = self.1.take() {
            let mut g = self.0.lock().unwrap();
            g.remove(&key);
        }
    }
}

/// When synchronizing to a possible ongoing write through `FsBlockStore::writes` these are the
/// possible outcomes.
#[derive(Debug)]
enum WriteCompletion {
    KnownGood,
    NotObserved,
    KnownBad,
    NotOngoing,
}

impl FsBlockStore {
    /// Returns the same Cid in either case. Ok variant is returned in case it is suspected the
    /// write completed successfully or there was never any write ongoing. Err variant is returned
    /// if it's known that the write failed.
    async fn write_completion(&self, cid: &Cid) -> WriteCompletion {
        use std::collections::hash_map::Entry;

        let mut rx = match self
            .writes
            .lock()
            .expect("cannot support poisoned")
            .entry(RepoCid(cid.clone()))
        {
            Entry::Occupied(oe) => oe.get().subscribe(),
            Entry::Vacant(_) => return WriteCompletion::NotOngoing,
        };

        trace!("awaiting concurrent write to completion");

        match rx.recv().await {
            Ok(Ok(())) => WriteCompletion::KnownGood,
            Err(broadcast::RecvError::Closed) => WriteCompletion::NotObserved,
            Ok(Err(_)) => WriteCompletion::KnownBad,
            Err(broadcast::RecvError::Lagged(_)) => {
                unreachable!("sending at most one message to the channel with capacity of one")
            }
        }
    }
}

#[async_trait]
impl BlockStore for FsBlockStore {
    fn new(path: PathBuf) -> Self {
        FsBlockStore {
            path,
            //cids: Default::default(),
            writes: Arc::new(Mutex::new(HashMap::with_capacity(8))),
            written_bytes: Default::default(),
        }
    }

    async fn init(&self) -> Result<(), Error> {
        fs::create_dir_all(self.path.clone()).await?;
        Ok(())
    }

    async fn open(&self) -> Result<(), Error> {
        // TODO: we probably want to cache the space usage?
        Ok(())
    }

    async fn contains(&self, cid: &Cid) -> Result<bool, Error> {
        let path = block_path(self.path.clone(), cid);

        // why doesn't this synchronize with the rest? Not sure if there is any use for this method
        // actually. When does it matter if a block exists, except for testing.

        let metadata = match fs::metadata(path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };

        Ok(metadata.is_file())
    }

    async fn get(&self, cid: &Cid) -> Result<Option<Block>, Error> {
        let span = tracing::trace_span!("get block", cid = %cid);

        async move {
            if let WriteCompletion::KnownBad = self.write_completion(cid).await {
                return Ok(None);
            }

            let path = block_path(self.path.clone(), cid);

            let cid = cid.to_owned();

            // probably best to do everything in the blocking thread if we are to issue multiple
            // syscalls
            tokio::task::spawn_blocking(move || {
                let mut file = match std::fs::File::open(path) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => {
                        return Err(e.into());
                    }
                };

                let len = file.metadata()?.len();

                let mut data = Vec::with_capacity(len as usize);
                file.read_to_end(&mut data)?;
                let block = Block::new(data.into_boxed_slice(), cid);
                Ok(Some(block))
            })
            .await?
        }
        .instrument(span)
        .await
    }

    async fn put(&self, block: Block) -> Result<(Cid, BlockPut), Error> {
        use std::collections::hash_map::Entry;

        let span = tracing::trace_span!("put block", cid = %block.cid());

        let target_path = block_path(self.path.clone(), &block.cid());
        let cid = block.cid;
        let data = block.data;

        let inner_span = span.clone();

        async move {
            // why synchronize here? because when we lose the race we cant know if there was someone
            // else interested in writing this block or not

            // FIXME: allowing only the creator to cleanup means this is not forget safe. the forget
            // doesn't result in memory unsafety but it will deadlock any other access to the cid.
            let (tx, mut rx) = {
                let mut g = self.writes.lock().expect("cant support poisoned");

                match g.entry(RepoCid(cid.to_owned())) {
                    Entry::Occupied(oe) => {
                        // someone is already writing this, nice
                        trace!("joining in on another already writing the block");
                        (oe.get().clone(), oe.get().subscribe())
                    }
                    Entry::Vacant(ve) => {
                        // we might be the first, or then the block exists already
                        let (tx, rx) = broadcast::channel(1);
                        ve.insert(tx.clone());
                        (tx, rx)
                    }
                }
            };

            // create this in case the winner is dropped while awaiting
            let cleanup = RemoveOnDrop(self.writes.clone(), Some(RepoCid(cid.to_owned())));

            let span = tracing::Span::current();

            // launch a blocking task for the filesystem mutation.
            // `tx` is moved into the task but `rx` stays in the async context.
            let je = tokio::task::spawn_blocking(move || {
                let _entered = span.enter();
                // pick winning writer with filesystem and create_new; this error will be the 1st
                // nested level

                // this is blocking context, use this instead of instrument
                let _entered = inner_span.enter();

                let sharded = target_path
                    .parent()
                    .expect("we already have at least the shard parent");

                // FIXME: missing fsync on directories; for example after winning the race we could
                // fsync the parent and parent.parent
                std::fs::create_dir_all(sharded)?;

                let target = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target_path)?;

                let temp_path = target_path.with_extension("tmp");

                match write_through_tempfile(target, &target_path, temp_path, &data) {
                    Ok(()) => {
                        let _ = tx
                            .send(Ok(()))
                            .expect("this cannot fail as we have at least one receiver on stack");

                        trace!("successfully wrote the block");
                        Ok::<_, std::io::Error>(Ok(data.len()))
                    }
                    Err(e) => {
                        match std::fs::remove_file(&target_path) {
                            Ok(_) => debug!("removed partially written {:?}", target_path),
                            Err(removal) => warn!(
                                "failed to remove partially written {:?}: {}",
                                target_path, removal
                            ),
                        }
                        let _ = tx
                            .send(Err(()))
                            .expect("this cannot fail as we have at least one receiver on stack");
                        Ok(Err(e))
                    }
                }
            })
            .await;

            // this is quite unfortunate but can't think of a way which would handle cleanup in drop
            // and not waste much effort.
            drop(cleanup);

            match je {
                Ok(Ok(Ok(written))) => {
                    drop(rx);

                    self.written_bytes
                        .fetch_add(written as u64, Ordering::SeqCst);

                    Ok((cid, BlockPut::NewBlock))
                }
                Ok(Ok(Err(e))) => {
                    // write failed but hopefully the target was removed
                    // no point in trying to remove it now
                    // ignore if no one is listening
                    Err(Error::new(e))
                }
                Ok(Err(_)) => {
                    // At least the following cases:
                    // - the block existed already
                    // - the block is being written to and we should await for this to complete
                    // - readonly or full filesystem prevents file creation

                    let message = match rx.recv().await {
                        Ok(message) => {
                            trace!("synchronized with writer, write outcome: {:?}", message);
                            message
                        }
                        Err(broadcast::RecvError::Closed) => {
                            // there was never any write intention by any party, and we may have just
                            // closed the last sender above, or we were late for the one message.
                            Ok(())
                        }
                        Err(broadcast::RecvError::Lagged(_)) => {
                            unreachable!("broadcast channel should only be messaged once here")
                        }
                    };

                    drop(rx);

                    if message.is_err() {
                        // could loop, however if one write failed, the next should probably
                        // fail as well (e.g. out of disk space)
                        Err(anyhow::anyhow!("other concurrent write failed"))
                    } else {
                        Ok((cid.to_owned(), BlockPut::Existed))
                    }
                }
                Err(e) => {
                    // blocking task panicked or the runtime is going down, but we don't know
                    // if the thread has stopped or not (like not)
                    Err(e.into())
                }
            }
        }
        .instrument(span)
        .await
    }

    async fn remove(&self, cid: &Cid) -> Result<Result<BlockRm, BlockRmError>, Error> {
        let path = block_path(self.path.clone(), cid);

        let span = trace_span!("remove block", cid = %cid);

        match self.write_completion(cid).instrument(span).await {
            WriteCompletion::KnownBad => Ok(Err(BlockRmError::NotFound(cid.to_owned()))),
            completion => {
                trace!(cid = %cid, completion = ?completion, "removing block after synchronizing");
                match fs::remove_file(path).await {
                    // FIXME: not sure if theres any point in taking cid ownership here?
                    Ok(()) => Ok(Ok(BlockRm::Removed(cid.to_owned()))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Ok(Err(BlockRmError::NotFound(cid.to_owned())))
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    async fn list(&self) -> Result<Vec<Cid>, Error> {
        use futures::future::{ready, Either};
        use futures::stream::{empty, TryStreamExt};

        let span = tracing::trace_span!("listing blocks");

        async move {
            let stream = fs::read_dir(self.path.clone()).await?;

            // FIXME: written as a stream to make the Vec be BoxStream<'static, Cid>
            let vec = stream
                .and_then(|d| async move {
                    // map over the shard directories
                    Ok(if d.file_type().await?.is_dir() {
                        Either::Left(fs::read_dir(d.path()).await?)
                    } else {
                        Either::Right(empty())
                    })
                })
                // flatten each
                .try_flatten()
                // convert the paths ending in ".data" into cid
                .try_filter_map(|d| {
                    let name = d.file_name();
                    let path: &std::path::Path = name.as_ref();

                    ready(if path.extension() != Some("data".as_ref()) {
                        Ok(None)
                    } else {
                        let maybe_cid = filestem_to_block_cid(path.file_stem());
                        Ok(maybe_cid)
                    })
                })
                .try_collect::<Vec<_>>()
                .await?;

            Ok(vec)
        }
        .instrument(span)
        .await
    }

    async fn wipe(&self) {
        unimplemented!("wipe")
    }
}

fn filestem_to_block_cid(file_stem: Option<&std::ffi::OsStr>) -> Option<Cid> {
    file_stem.and_then(|stem| stem.to_str()).and_then(|s| {
        // this isn't an interchangeable way to store esp. cidv0
        let cid = Cid::try_from(s);

        // it's very unlikely that we'd hit a valid file with "data" extension
        // which we did write so I'd say wrapping the Cid parsing error as
        // std::io::Error is highly unnecessary. if someone wants to
        // *keep* ".data" ending files in the block store we shouldn't
        // die over it.
        //
        // if we could, we would do a log_once here, if we could easily
        // do such thing. like a inode based global probabilistic
        // hashset.
        //
        // FIXME: add test

        cid.ok()
    })
}

fn filestem_to_pin_cid(file_stem: Option<&std::ffi::OsStr>) -> Option<Cid> {
    file_stem.and_then(|stem| stem.to_str()).and_then(|s| {
        // this isn't an interchangeable way to store esp. cidv0
        let bytes = multibase::Base::Base32Lower.decode(s).ok()?;
        let cid = Cid::try_from(bytes);

        // it's very unlikely that we'd hit a valid file with "data" extension
        // which we did write so I'd say wrapping the Cid parsing error as
        // std::io::Error is highly unnecessary. if someone wants to
        // *keep* ".data" ending files in the block store we shouldn't
        // die over it.
        //
        // if we could, we would do a log_once here, if we could easily
        // do such thing. like a inode based global probabilistic
        // hashset.
        //
        // FIXME: add test

        cid.ok()
    })
}

fn block_path(mut base: PathBuf, cid: &Cid) -> PathBuf {
    // this is ascii always, and wasteful until we can drop the cid for multihash ... which is
    // probably soon, we just need turn /refs/local to use /pin/list.
    let mut file = if cid.version() == cid::Version::V1 {
        cid.to_string()
    } else {
        Cid::new_v1(cid.codec(), cid.hash().to_owned()).to_string()
    };

    // second-to-last/2
    let start = file.len() - 3;

    let shard = &file[start..start + 2];
    assert_eq!(file[start + 2..].len(), 1);
    base.push(shard);

    // not sure why set extension instead
    file.push_str(".data");
    base.push(file);
    base
}

/// Same as `block_path` except it doesn't canonicalize the cid to later version.
fn pin_path(mut base: PathBuf, cid: &Cid) -> PathBuf {
    // it might be illegal to to render cidv0 as base32
    let mut file: String = multibase::Base::Base32Lower.encode(cid.to_bytes());

    // second-to-last/2
    let start = file.len() - 3;

    let shard = &file[start..start + 2];
    assert_eq!(file[start + 2..].len(), 1);
    base.push(shard);

    // not sure why set extension instead
    file.push_str(".data");
    base.push(file);
    base
}

fn write_through_tempfile(
    target: std::fs::File,
    target_path: impl AsRef<std::path::Path>,
    temp_path: impl AsRef<std::path::Path>,
    data: &[u8],
) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut temp = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)?;

    temp.write_all(&*data)?;
    temp.flush()?;

    // safe default
    temp.sync_all()?;

    drop(temp);
    drop(target);

    std::fs::rename(temp_path, target_path)?;

    // FIXME: there should be a directory fsync here as well

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitswap::Block;
    use cid::{Cid, Codec};
    use hex_literal::hex;
    use multihash::Sha2_256;
    use std::env::temp_dir;
    use std::sync::Arc;

    #[tokio::test(max_threads = 1)]
    async fn test_fs_blockstore() {
        let mut tmp = temp_dir();
        tmp.push("blockstore1");
        std::fs::remove_dir_all(tmp.clone()).ok();
        let store = FsBlockStore::new(tmp.clone());

        let data = b"1".to_vec().into_boxed_slice();
        let cid = Cid::new_v1(Codec::Raw, Sha2_256::digest(&data));
        let block = Block::new(data, cid.clone());

        store.init().await.unwrap();
        store.open().await.unwrap();

        let contains = store.contains(&cid).await.unwrap();
        assert_eq!(contains, false);
        let get = store.get(&cid).await.unwrap();
        assert_eq!(get, None);
        if store.remove(&cid).await.unwrap().is_ok() {
            panic!("block should not be found")
        }

        let put = store.put(block.clone()).await.unwrap();
        assert_eq!(put.0, cid.to_owned());
        let contains = store.contains(&cid);
        assert_eq!(contains.await.unwrap(), true);
        let get = store.get(&cid);
        assert_eq!(get.await.unwrap(), Some(block.clone()));

        store.remove(&cid).await.unwrap().unwrap();
        let contains = store.contains(&cid);
        assert_eq!(contains.await.unwrap(), false);
        let get = store.get(&cid);
        assert_eq!(get.await.unwrap(), None);

        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test(max_threads = 1)]
    async fn test_fs_blockstore_open() {
        let mut tmp = temp_dir();
        tmp.push("blockstore2");
        std::fs::remove_dir_all(&tmp).ok();

        let data = b"1".to_vec().into_boxed_slice();
        let cid = Cid::new_v1(Codec::Raw, Sha2_256::digest(&data));
        let block = Block::new(data, cid);

        let block_store = FsBlockStore::new(tmp.clone());
        block_store.init().await.unwrap();
        block_store.open().await.unwrap();

        assert!(!block_store.contains(block.cid()).await.unwrap());
        block_store.put(block.clone()).await.unwrap();

        let block_store = FsBlockStore::new(tmp.clone());
        block_store.open().await.unwrap();
        assert!(block_store.contains(block.cid()).await.unwrap());
        assert_eq!(block_store.get(block.cid()).await.unwrap().unwrap(), block);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test(max_threads = 1)]
    async fn test_fs_blockstore_list() {
        let mut tmp = temp_dir();
        tmp.push("blockstore_list");
        std::fs::remove_dir_all(&tmp).ok();

        let block_store = FsBlockStore::new(tmp.clone());
        block_store.init().await.unwrap();
        block_store.open().await.unwrap();

        for data in &[b"1", b"2", b"3"] {
            let data_slice = data.to_vec().into_boxed_slice();
            let cid = Cid::new_v1(Codec::Raw, Sha2_256::digest(&data_slice));
            let block = Block::new(data_slice, cid);
            block_store.put(block.clone()).await.unwrap();
        }

        let cids = block_store.list().await.unwrap();
        assert_eq!(cids.len(), 3);
        for cid in cids.iter() {
            assert!(block_store.contains(cid).await.unwrap());
        }
    }

    #[tokio::test(max_threads = 1)]
    async fn race_to_insert_new() {
        // FIXME: why not tempdir?
        let mut tmp = temp_dir();
        tmp.push("race_to_insert_new");
        std::fs::remove_dir_all(&tmp).ok();

        let single = FsBlockStore::new(tmp.clone());
        single.init().await.unwrap();

        let single = Arc::new(single);

        let cid = Cid::try_from("QmRgutAxd8t7oGkSm4wmeuByG6M51wcTso6cubDdQtuEfL").unwrap();
        let data = hex!("0a0d08021207666f6f6261720a1807");

        let block = Block {
            cid,
            data: data.into(),
        };

        let count = 10;

        let (writes, existing) = race_to_insert_scenario(count, block, &single).await;

        let single = Arc::try_unwrap(single).unwrap();
        assert_eq!(single.written_bytes.into_inner(), 15);

        assert_eq!(writes, 1);
        assert_eq!(existing, count - 1);
    }

    #[tokio::test(max_threads = 1)]
    async fn race_to_insert_with_existing() {
        // FIXME: why not tempdir?
        let mut tmp = temp_dir();
        tmp.push("race_to_insert_existing");
        std::fs::remove_dir_all(&tmp).ok();

        let single = FsBlockStore::new(tmp.clone());
        single.init().await.unwrap();

        let single = Arc::new(single);

        let cid = Cid::try_from("QmRgutAxd8t7oGkSm4wmeuByG6M51wcTso6cubDdQtuEfL").unwrap();
        let data = hex!("0a0d08021207666f6f6261720a1807");

        let block = Block {
            cid,
            data: data.into(),
        };

        single.put(block.clone()).await.unwrap();

        assert_eq!(single.written_bytes.load(Ordering::SeqCst), 15);

        let count = 10;

        let (writes, existing) = race_to_insert_scenario(count, block, &single).await;

        let single = Arc::try_unwrap(single).unwrap();
        assert_eq!(single.written_bytes.into_inner(), 15);

        assert_eq!(writes, 0);
        assert_eq!(existing, count);
    }

    async fn race_to_insert_scenario(
        count: usize,
        block: Block,
        blockstore: &Arc<FsBlockStore>,
    ) -> (usize, usize) {
        let barrier = Arc::new(tokio::sync::Barrier::new(count));

        let join_handles = (0..count)
            .map(|_| {
                tokio::spawn({
                    let bs = Arc::clone(&blockstore);
                    let barrier = Arc::clone(&barrier);
                    let block = block.clone();
                    async move {
                        barrier.wait().await;
                        bs.put(block).await
                    }
                })
            })
            .collect::<Vec<_>>();

        let mut writes = 0usize;
        let mut existing = 0usize;

        for jh in join_handles {
            let res = jh.await;

            match res {
                Ok(Ok((_, BlockPut::NewBlock))) => writes += 1,
                Ok(Ok((_, BlockPut::Existed))) => existing += 1,
                Ok(Err(e)) => println!("joinhandle err: {}", e),
                _ => unreachable!("join error"),
            }
        }

        (writes, existing)
    }

    #[tokio::test(max_threads = 1)]
    async fn remove() {
        // FIXME: why not tempdir?
        let mut tmp = temp_dir();
        tmp.push("remove");
        std::fs::remove_dir_all(&tmp).ok();

        let single = FsBlockStore::new(tmp.clone());

        single.init().await.unwrap();

        let cid = Cid::try_from("QmRgutAxd8t7oGkSm4wmeuByG6M51wcTso6cubDdQtuEfL").unwrap();
        let data = hex!("0a0d08021207666f6f6261720a1807");

        let block = Block {
            cid: cid.clone(),
            data: data.into(),
        };

        assert_eq!(single.list().await.unwrap().len(), 0);

        single.put(block).await.unwrap();

        // compare the multihash since we store the block named as cidv1
        assert_eq!(single.list().await.unwrap()[0].hash(), cid.hash());

        single.remove(&cid).await.unwrap().unwrap();
        assert_eq!(single.list().await.unwrap().len(), 0);
    }
}
