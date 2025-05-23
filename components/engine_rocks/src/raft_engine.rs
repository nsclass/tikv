// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
use engine_traits::{
    CF_DEFAULT, Error, Iterable, KvEngine, MiscExt, Mutable, Peekable, RAFT_LOG_MULTI_GET_CNT,
    RaftEngine, RaftEngineDebug, RaftEngineReadOnly, RaftLogBatch, Result, WriteBatch,
    WriteBatchExt, WriteOptions,
};
use kvproto::{
    metapb::Region,
    raft_serverpb::{
        RaftApplyState, RaftLocalState, RegionLocalState, StoreIdent, StoreRecoverState,
    },
};
use protobuf::Message;
use raft::eraftpb::Entry;
use tikv_util::{box_err, box_try};

use crate::{RocksEngine, RocksWriteBatchVec, util};

impl RaftEngineReadOnly for RocksEngine {
    fn get_raft_state(&self, raft_group_id: u64) -> Result<Option<RaftLocalState>> {
        let key = keys::raft_state_key(raft_group_id);
        self.get_msg_cf(CF_DEFAULT, &key)
    }

    fn get_entry(&self, raft_group_id: u64, index: u64) -> Result<Option<Entry>> {
        let key = keys::raft_log_key(raft_group_id, index);
        self.get_msg_cf(CF_DEFAULT, &key)
    }

    fn fetch_entries_to(
        &self,
        region_id: u64,
        low: u64,
        high: u64,
        max_size: Option<usize>,
        buf: &mut Vec<Entry>,
    ) -> Result<usize> {
        let (max_size, mut total_size, mut count) = (max_size.unwrap_or(usize::MAX), 0, 0);

        if high - low <= RAFT_LOG_MULTI_GET_CNT {
            // If election happens in inactive regions, they will just try to fetch one
            // empty log.
            for i in low..high {
                if total_size > 0 && total_size >= max_size {
                    break;
                }
                let key = keys::raft_log_key(region_id, i);
                match self.get_value(&key) {
                    Ok(None) => return Err(Error::EntriesCompacted),
                    Ok(Some(v)) => {
                        let mut entry = Entry::default();
                        entry.merge_from_bytes(&v)?;
                        assert_eq!(entry.get_index(), i);
                        buf.push(entry);
                        total_size += v.len();
                        count += 1;
                    }
                    Err(e) => return Err(box_err!(e)),
                }
            }
            return Ok(count);
        }

        let (mut check_compacted, mut compacted, mut next_index) = (true, false, low);
        let start_key = keys::raft_log_key(region_id, low);
        let end_key = keys::raft_log_key(region_id, high);
        self.scan(
            CF_DEFAULT,
            &start_key,
            &end_key,
            true, // fill_cache
            |_, value| {
                let mut entry = Entry::default();
                entry.merge_from_bytes(value)?;

                if check_compacted {
                    if entry.get_index() != low {
                        compacted = true;
                        // May meet gap or has been compacted.
                        return Ok(false);
                    }
                    check_compacted = false;
                } else {
                    assert_eq!(entry.get_index(), next_index);
                }
                next_index += 1;

                buf.push(entry);
                total_size += value.len();
                count += 1;
                Ok(total_size < max_size)
            },
        )?;

        // If we get the correct number of entries, returns.
        // Or the total size almost exceeds max_size, returns.
        if count == (high - low) as usize || total_size >= max_size {
            return Ok(count);
        }

        if compacted {
            return Err(Error::EntriesCompacted);
        }

        // Here means we don't fetch enough entries.
        Err(Error::EntriesUnavailable)
    }

    fn is_empty(&self) -> Result<bool> {
        let mut is_empty = true;
        self.scan(CF_DEFAULT, b"", b"", false, |_, _| {
            is_empty = false;
            Ok(false)
        })?;

        Ok(is_empty)
    }

    fn get_store_ident(&self) -> Result<Option<StoreIdent>> {
        self.get_msg_cf(CF_DEFAULT, keys::STORE_IDENT_KEY)
    }

    fn get_prepare_bootstrap_region(&self) -> Result<Option<Region>> {
        self.get_msg_cf(CF_DEFAULT, keys::PREPARE_BOOTSTRAP_KEY)
    }

    // Following methods are used by raftstore v2 only, which always use raft log
    // engine.
    fn get_region_state(
        &self,
        _raft_group_id: u64,
        _apply_index: u64,
    ) -> Result<Option<RegionLocalState>> {
        panic!()
    }

    fn get_apply_state(
        &self,
        _raft_group_id: u64,
        _apply_index: u64,
    ) -> Result<Option<RaftApplyState>> {
        panic!()
    }

    fn get_flushed_index(&self, _raft_group_id: u64, _cf: &str) -> Result<Option<u64>> {
        panic!()
    }

    fn get_dirty_mark(&self, _raft_group_id: u64, _tablet_index: u64) -> Result<bool> {
        panic!()
    }

    fn get_recover_state(&self) -> Result<Option<StoreRecoverState>> {
        self.get_msg_cf(CF_DEFAULT, keys::RECOVER_STATE_KEY)
    }
}

impl RaftEngineDebug for RocksEngine {
    fn scan_entries<F>(&self, raft_group_id: u64, mut f: F) -> Result<()>
    where
        F: FnMut(Entry) -> Result<bool>,
    {
        let start_key = keys::raft_log_key(raft_group_id, 0);
        let end_key = keys::raft_log_key(raft_group_id, u64::MAX);
        self.scan(
            CF_DEFAULT,
            &start_key,
            &end_key,
            false, // fill_cache
            |_, value| {
                let mut entry = Entry::default();
                entry.merge_from_bytes(value)?;
                f(entry)
            },
        )
    }
}

impl RocksEngine {
    fn gc_impl(
        &self,
        raft_group_id: u64,
        mut from: u64,
        to: u64,
        raft_wb: &mut RocksWriteBatchVec,
    ) -> Result<usize> {
        if from == 0 {
            let start_key = keys::raft_log_key(raft_group_id, 0);
            let prefix = keys::raft_log_prefix(raft_group_id);
            match self.seek(CF_DEFAULT, &start_key)? {
                Some((k, _)) if k.starts_with(&prefix) => from = box_try!(keys::raft_log_index(&k)),
                // No need to gc.
                _ => return Ok(0),
            }
        }
        if from >= to {
            return Ok(0);
        }

        for idx in from..to {
            let key = keys::raft_log_key(raft_group_id, idx);
            raft_wb.delete(&key)?;
            if raft_wb.count() >= Self::WRITE_BATCH_MAX_KEYS * 2 {
                raft_wb.write()?;
                raft_wb.clear();
            }
        }
        Ok((to - from) as usize)
    }
}

// FIXME: RaftEngine should probably be implemented generically
// for all KvEngines, but is currently implemented separately for
// every engine.
impl RaftEngine for RocksEngine {
    type LogBatch = RocksWriteBatchVec;

    fn log_batch(&self, capacity: usize) -> Self::LogBatch {
        RocksWriteBatchVec::with_unit_capacity(self, capacity)
    }

    fn sync(&self) -> Result<()> {
        self.sync_wal()
    }

    fn consume(&self, batch: &mut Self::LogBatch, sync_log: bool) -> Result<usize> {
        let bytes = batch.data_size();
        let mut opts = WriteOptions::default();
        opts.set_sync(sync_log);
        batch.write_opt(&opts)?;
        batch.clear();
        Ok(bytes)
    }

    fn consume_and_shrink(
        &self,
        batch: &mut Self::LogBatch,
        sync_log: bool,
        max_capacity: usize,
        shrink_to: usize,
    ) -> Result<usize> {
        let data_size = self.consume(batch, sync_log)?;
        if data_size > max_capacity {
            *batch = self.write_batch_with_cap(shrink_to);
        }
        Ok(data_size)
    }

    fn clean(
        &self,
        raft_group_id: u64,
        mut first_index: u64,
        state: &RaftLocalState,
        batch: &mut Self::LogBatch,
    ) -> Result<()> {
        batch.delete(&keys::raft_state_key(raft_group_id))?;
        batch.delete(&keys::region_state_key(raft_group_id))?;
        batch.delete(&keys::apply_state_key(raft_group_id))?;
        if first_index == 0 {
            let seek_key = keys::raft_log_key(raft_group_id, 0);
            let prefix = keys::raft_log_prefix(raft_group_id);
            fail::fail_point!("engine_rocks_raft_engine_clean_seek", |_| Ok(()));
            if let Some((key, _)) = self.seek(CF_DEFAULT, &seek_key)? {
                if !key.starts_with(&prefix) {
                    // No raft logs for the raft group.
                    return Ok(());
                }
                first_index = match keys::raft_log_index(&key) {
                    Ok(index) => index,
                    Err(_) => return Ok(()),
                };
            } else {
                return Ok(());
            }
        }
        if first_index <= state.last_index {
            for index in first_index..=state.last_index {
                let key = keys::raft_log_key(raft_group_id, index);
                batch.delete(&key)?;
            }
        }
        Ok(())
    }

    fn gc(&self, raft_group_id: u64, from: u64, to: u64, batch: &mut Self::LogBatch) -> Result<()> {
        self.gc_impl(raft_group_id, from, to, batch)?;
        Ok(())
    }

    fn delete_all_but_one_states_before(
        &self,
        _raft_group_id: u64,
        _apply_index: u64,
        _batch: &mut Self::LogBatch,
    ) -> Result<()> {
        panic!()
    }

    fn flush_metrics(&self, instance: &str) {
        KvEngine::flush_metrics(self, instance)
    }

    fn dump_stats(&self) -> Result<String> {
        MiscExt::dump_stats(self)
    }

    fn get_engine_size(&self) -> Result<u64> {
        let handle = util::get_cf_handle(self.as_inner(), CF_DEFAULT)?;
        let used_size = util::get_engine_cf_used_size(self.as_inner(), handle);

        Ok(used_size)
    }

    fn get_engine_path(&self) -> &str {
        self.as_inner().path()
    }

    fn for_each_raft_group<E, F>(&self, f: &mut F) -> std::result::Result<(), E>
    where
        F: FnMut(u64) -> std::result::Result<(), E>,
        E: From<Error>,
    {
        let start_key = keys::REGION_META_MIN_KEY;
        let end_key = keys::REGION_META_MAX_KEY;
        let mut err = None;
        self.scan(CF_DEFAULT, start_key, end_key, false, |key, _| {
            let (region_id, suffix) = box_try!(keys::decode_region_meta_key(key));
            if suffix != keys::REGION_STATE_SUFFIX {
                return Ok(true);
            }

            match f(region_id) {
                Ok(()) => Ok(true),
                Err(e) => {
                    err = Some(e);
                    Ok(false)
                }
            }
        })?;
        match err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }
}

impl RaftLogBatch for RocksWriteBatchVec {
    fn append(
        &mut self,
        raft_group_id: u64,
        overwrite_to: Option<u64>,
        entries: Vec<Entry>,
    ) -> Result<()> {
        let overwrite_to = overwrite_to.unwrap_or(0);
        if let Some(last) = entries.last()
            && last.get_index() + 1 < overwrite_to
        {
            for index in last.get_index() + 1..overwrite_to {
                let key = keys::raft_log_key(raft_group_id, index);
                self.delete(&key).unwrap();
            }
        }
        if let Some(max_size) = entries.iter().map(|e| e.compute_size()).max() {
            let ser_buf = Vec::with_capacity(max_size as usize);
            return self.append_impl(raft_group_id, &entries, ser_buf);
        }
        Ok(())
    }

    fn put_raft_state(&mut self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.put_msg(&keys::raft_state_key(raft_group_id), state)
    }

    fn persist_size(&self) -> usize {
        self.data_size()
    }

    fn is_empty(&self) -> bool {
        WriteBatch::is_empty(self)
    }

    fn merge(&mut self, src: Self) -> Result<()> {
        WriteBatch::merge(self, src)
    }

    fn put_store_ident(&mut self, ident: &StoreIdent) -> Result<()> {
        self.put_msg(keys::STORE_IDENT_KEY, ident)
    }

    fn put_prepare_bootstrap_region(&mut self, region: &Region) -> Result<()> {
        self.put_msg(keys::PREPARE_BOOTSTRAP_KEY, region)
    }

    fn remove_prepare_bootstrap_region(&mut self) -> Result<()> {
        self.delete(keys::PREPARE_BOOTSTRAP_KEY)
    }

    // Following methods are used by raftstore v2 only, which always use raft log
    // engine.
    fn put_region_state(
        &mut self,
        _raft_group_id: u64,
        _apply_index: u64,
        _state: &RegionLocalState,
    ) -> Result<()> {
        panic!()
    }

    fn put_apply_state(
        &mut self,
        _raft_group_id: u64,
        _apply_index: u64,
        _state: &RaftApplyState,
    ) -> Result<()> {
        panic!()
    }

    fn put_flushed_index(
        &mut self,
        _raft_group_id: u64,
        _cf: &str,
        _tablet_index: u64,
        _apply_index: u64,
    ) -> Result<()> {
        panic!()
    }

    fn put_dirty_mark(
        &mut self,
        _raft_group_id: u64,
        _tablet_index: u64,
        _dirty: bool,
    ) -> Result<()> {
        panic!()
    }

    fn put_recover_state(&mut self, state: &StoreRecoverState) -> Result<()> {
        self.put_msg(keys::RECOVER_STATE_KEY, state)
    }
}

impl RocksWriteBatchVec {
    fn append_impl(
        &mut self,
        raft_group_id: u64,
        entries: &[Entry],
        mut ser_buf: Vec<u8>,
    ) -> Result<()> {
        for entry in entries {
            let key = keys::raft_log_key(raft_group_id, entry.get_index());
            ser_buf.clear();
            entry.write_to_vec(&mut ser_buf).unwrap();
            self.put(&key, &ser_buf)?;
        }
        Ok(())
    }
}
