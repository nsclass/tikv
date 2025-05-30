// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::hash_map::Entry as MapEntry,
    error::Error as StdError,
    result,
    sync::{Arc, Mutex, RwLock, mpsc},
    thread,
    time::Duration,
};

use collections::{HashMap, HashSet};
use crossbeam::channel::TrySendError;
use encryption_export::DataKeyManager;
use engine_rocks::{RocksEngine, RocksSnapshot, RocksStatistics};
use engine_test::raft::RaftTestEngine;
use engine_traits::{
    CF_DEFAULT, CF_RAFT, CompactExt, Engines, Iterable, ManualCompactionOptions, MiscExt, Mutable,
    Peekable, RaftEngineReadOnly, SyncMutable, WriteBatch, WriteBatchExt,
};
use file_system::IoRateLimiter;
use futures::{self, StreamExt, channel::oneshot, executor::block_on, future::BoxFuture};
use kvproto::{
    errorpb::Error as PbError,
    kvrpcpb::{ApiVersion, Context, DiskFullOpt},
    metapb::{self, Buckets, PeerRole, RegionEpoch, StoreLabel},
    pdpb::{self, CheckPolicy, StoreReport},
    raft_cmdpb::*,
    raft_serverpb::{
        PeerState, RaftApplyState, RaftLocalState, RaftMessage, RaftTruncatedState,
        RegionLocalState,
    },
};
use pd_client::{BucketStat, PdClient};
use raft::eraftpb::ConfChangeType;
use raftstore::{
    Error, Result,
    router::RaftStoreRouter,
    store::{
        fsm::{
            ApplyRouter, RaftBatchSystem, RaftRouter, create_raft_batch_system,
            store::{PENDING_MSG_CAP, StoreMeta},
        },
        transport::CasualRouter,
        *,
    },
};
use resource_control::ResourceGroupManager;
use tempfile::TempDir;
use test_pd_client::TestPdClient;
use tikv::{config::TikvConfig, server::Result as ServerResult};
use tikv_util::{
    HandyRwLock,
    thread_group::GroupProperties,
    time::{Instant, ThreadReadId},
    worker::LazyWorker,
};
use txn_types::WriteBatchFlags;

use super::*;
use crate::Config;
// We simulate 3 or 5 nodes, each has a store.
// Sometimes, we use fixed id to test, which means the id
// isn't allocated by pd, and node id, store id are same.
// E,g, for node 1, the node id and store id are both 1.

pub trait Simulator {
    // Pass 0 to let pd allocate a node id if db is empty.
    // If node id > 0, the node must be created in db already,
    // and the node id must be the same as given argument.
    // Return the node id.
    // TODO: we will rename node name here because now we use store only.
    fn run_node(
        &mut self,
        node_id: u64,
        cfg: Config,
        engines: Engines<RocksEngine, RaftTestEngine>,
        store_meta: Arc<Mutex<StoreMeta>>,
        key_manager: Option<Arc<DataKeyManager>>,
        router: RaftRouter<RocksEngine, RaftTestEngine>,
        system: RaftBatchSystem<RocksEngine, RaftTestEngine>,
        resource_manager: &Option<Arc<ResourceGroupManager>>,
    ) -> ServerResult<u64>;
    fn stop_node(&mut self, node_id: u64);
    fn get_node_ids(&self) -> HashSet<u64>;
    fn async_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        cb: Callback<RocksSnapshot>,
    ) -> Result<()> {
        self.async_command_on_node_with_opts(node_id, request, cb, Default::default())
    }
    fn async_command_on_node_with_opts(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        cb: Callback<RocksSnapshot>,
        opts: RaftCmdExtraOpts,
    ) -> Result<()>;
    fn send_raft_msg(&mut self, msg: RaftMessage) -> Result<()>;
    fn get_snap_dir(&self, node_id: u64) -> String;
    fn get_snap_mgr(&self, node_id: u64) -> &SnapManager;
    fn get_router(&self, node_id: u64) -> Option<RaftRouter<RocksEngine, RaftTestEngine>>;
    fn get_apply_router(&self, node_id: u64) -> Option<ApplyRouter<RocksEngine>>;
    fn add_send_filter(&mut self, node_id: u64, filter: Box<dyn Filter>);
    fn clear_send_filters(&mut self, node_id: u64);
    fn add_recv_filter(&mut self, node_id: u64, filter: Box<dyn Filter>);
    fn clear_recv_filters(&mut self, node_id: u64);

    fn call_command(&self, request: RaftCmdRequest, timeout: Duration) -> Result<RaftCmdResponse> {
        let node_id = request.get_header().get_peer().get_store_id();
        self.call_command_on_node(node_id, request, timeout)
    }

    fn read(
        &mut self,
        batch_id: Option<ThreadReadId>,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let node_id = request.get_header().get_peer().get_store_id();
        let (cb, mut rx) = make_cb(&request);
        self.async_read(node_id, batch_id, request, cb);
        rx.recv_timeout(timeout)
            .map_err(|_| Error::Timeout(format!("request timeout for {:?}", timeout)))
    }

    fn async_read(
        &mut self,
        node_id: u64,
        batch_id: Option<ThreadReadId>,
        request: RaftCmdRequest,
        cb: Callback<RocksSnapshot>,
    );

    fn call_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let (cb, mut rx) = make_cb(&request);

        match self.async_command_on_node(node_id, request, cb) {
            Ok(()) => {}
            Err(e) => {
                let mut resp = RaftCmdResponse::default();
                resp.mut_header().set_error(e.into());
                return Ok(resp);
            }
        }
        rx.recv_timeout(timeout)
            .map_err(|e| Error::Timeout(format!("request timeout for {:?}: {:?}", timeout, e)))
    }
}

pub struct Cluster<T: Simulator> {
    pub cfg: Config,
    leaders: HashMap<u64, metapb::Peer>,
    pub count: usize,

    pub paths: Vec<TempDir>,
    pub dbs: Vec<Engines<RocksEngine, RaftTestEngine>>,
    pub store_metas: HashMap<u64, Arc<Mutex<StoreMeta>>>,
    key_managers: Vec<Option<Arc<DataKeyManager>>>,
    pub io_rate_limiter: Option<Arc<IoRateLimiter>>,
    pub engines: HashMap<u64, Engines<RocksEngine, RaftTestEngine>>,
    key_managers_map: HashMap<u64, Option<Arc<DataKeyManager>>>,
    pub labels: HashMap<u64, HashMap<String, String>>,
    group_props: HashMap<u64, GroupProperties>,
    pub sst_workers: Vec<LazyWorker<String>>,
    pub sst_workers_map: HashMap<u64, usize>,
    pub kv_statistics: Vec<Arc<RocksStatistics>>,
    pub raft_statistics: Vec<Option<Arc<RocksStatistics>>>,
    pub sim: Arc<RwLock<T>>,
    pub pd_client: Arc<TestPdClient>,
    resource_manager: Option<Arc<ResourceGroupManager>>,
}

impl<T: Simulator> Cluster<T> {
    // Create the default Store cluster.
    pub fn new(
        id: u64,
        count: usize,
        sim: Arc<RwLock<T>>,
        pd_client: Arc<TestPdClient>,
        api_version: ApiVersion,
    ) -> Cluster<T> {
        // TODO: In the future, maybe it's better to test both case where
        // `use_delete_range` is true and false
        Cluster {
            cfg: Config::new(new_tikv_config_with_api_ver(id, api_version), true),
            leaders: HashMap::default(),
            count,
            paths: vec![],
            dbs: vec![],
            store_metas: HashMap::default(),
            key_managers: vec![],
            io_rate_limiter: None,
            engines: HashMap::default(),
            key_managers_map: HashMap::default(),
            labels: HashMap::default(),
            group_props: HashMap::default(),
            sim,
            pd_client,
            sst_workers: vec![],
            sst_workers_map: HashMap::default(),
            resource_manager: Some(Arc::new(ResourceGroupManager::default())),
            kv_statistics: vec![],
            raft_statistics: vec![],
        }
    }

    pub fn set_cfg(&mut self, mut cfg: TikvConfig) {
        cfg.cfg_path = self.cfg.tikv.cfg_path.clone();
        self.cfg.tikv = cfg;
    }

    // To destroy temp dir later.
    pub fn take_path(&mut self) -> Vec<TempDir> {
        std::mem::take(&mut self.paths)
    }

    pub fn id(&self) -> u64 {
        self.cfg.server.cluster_id
    }

    pub fn pre_start_check(&mut self) -> result::Result<(), Box<dyn StdError>> {
        for path in &self.paths {
            self.cfg.storage.data_dir = path.path().to_str().unwrap().to_owned();
            self.cfg.validate()?
        }
        Ok(())
    }

    /// Engines in a just created cluster are not bootstrapped, which means they
    /// are not associated with a `node_id`. Call `Cluster::start` can bootstrap
    /// all nodes in the cluster.
    ///
    /// However sometimes a node can be bootstrapped externally. This function
    /// can be called to mark them as bootstrapped in `Cluster`.
    pub fn set_bootstrapped(&mut self, node_id: u64, offset: usize) {
        let engines = self.dbs[offset].clone();
        let key_mgr = self.key_managers[offset].clone();
        assert!(self.engines.insert(node_id, engines).is_none());
        assert!(self.key_managers_map.insert(node_id, key_mgr).is_none());
        assert!(self.sst_workers_map.insert(node_id, offset).is_none());
    }

    fn create_engine(&mut self, router: Option<RaftRouter<RocksEngine, RaftTestEngine>>) {
        let (engines, key_manager, dir, sst_worker, kv_statistics, raft_statistics) =
            create_test_engine(router, self.io_rate_limiter.clone(), &self.cfg);
        self.dbs.push(engines);
        self.key_managers.push(key_manager);
        self.paths.push(dir);
        self.sst_workers.push(sst_worker);
        self.kv_statistics.push(kv_statistics);
        self.raft_statistics.push(raft_statistics);
    }

    pub fn restart_engine(&mut self, node_id: u64) {
        let idx = node_id as usize - 1;
        let path = self.paths.remove(idx);
        {
            self.dbs.remove(idx);
            self.key_managers.remove(idx);
            self.sst_workers.remove(idx);
            self.kv_statistics.remove(idx);
            self.raft_statistics.remove(idx);
            self.engines.remove(&node_id);
        }
        let (engines, key_manager, dir, sst_worker, kv_statistics, raft_statistics) =
            start_test_engine(None, self.io_rate_limiter.clone(), &self.cfg, path);
        self.dbs.insert(idx, engines);
        self.key_managers.insert(idx, key_manager);
        self.paths.insert(idx, dir);
        self.sst_workers.insert(idx, sst_worker);
        self.kv_statistics.insert(idx, kv_statistics);
        self.raft_statistics.insert(idx, raft_statistics);
        self.engines
            .insert(node_id, self.dbs.last().unwrap().clone());
    }

    pub fn create_engines(&mut self) {
        self.io_rate_limiter = Some(Arc::new(
            self.cfg
                .storage
                .io_rate_limit
                .build(true /* enable_statistics */),
        ));
        for _ in 0..self.count {
            self.create_engine(None);
        }
    }

    pub fn start(&mut self) -> ServerResult<()> {
        // Try recover from last shutdown.
        let node_ids: Vec<u64> = self.engines.iter().map(|(&id, _)| id).collect();
        for node_id in node_ids {
            self.run_node(node_id)?;
        }

        // Try start new nodes.
        for _ in 0..self.count - self.engines.len() {
            let (router, system) =
                create_raft_batch_system(&self.cfg.raft_store, &self.resource_manager);
            self.create_engine(Some(router.clone()));

            let engines = self.dbs.last().unwrap().clone();
            let key_mgr = self.key_managers.last().unwrap().clone();
            let store_meta = Arc::new(Mutex::new(StoreMeta::new(PENDING_MSG_CAP)));

            let props = GroupProperties::default();
            tikv_util::thread_group::set_properties(Some(props.clone()));

            let mut sim = self.sim.wl();
            let node_id = sim.run_node(
                0,
                self.cfg.clone(),
                engines.clone(),
                store_meta.clone(),
                key_mgr.clone(),
                router,
                system,
                &self.resource_manager,
            )?;
            self.group_props.insert(node_id, props);
            self.engines.insert(node_id, engines);
            self.store_metas.insert(node_id, store_meta);
            self.key_managers_map.insert(node_id, key_mgr);
            self.sst_workers_map
                .insert(node_id, self.sst_workers.len() - 1);
        }
        Ok(())
    }

    pub fn compact_data(&self) {
        for engine in self.engines.values() {
            let db = &engine.kv;
            db.compact_range_cf(
                CF_DEFAULT,
                None,
                None,
                ManualCompactionOptions::new(false, 1, false),
            )
            .unwrap();
        }
    }

    pub fn flush_data(&self) {
        for engine in self.engines.values() {
            let db = &engine.kv;
            db.flush_cf(CF_DEFAULT, true /* sync */).unwrap();
        }
    }

    // Bootstrap the store with fixed ID (like 1, 2, .. 5) and
    // initialize first region in all stores, then start the cluster.
    pub fn run(&mut self) {
        self.create_engines();
        self.bootstrap_region().unwrap();
        self.start().unwrap();
    }

    // Bootstrap the store with fixed ID (like 1, 2, .. 5) and
    // initialize first region in store 1, then start the cluster.
    pub fn run_conf_change(&mut self) -> u64 {
        self.create_engines();
        let region_id = self.bootstrap_conf_change();
        self.start().unwrap();
        region_id
    }

    pub fn get_node_ids(&self) -> HashSet<u64> {
        self.sim.rl().get_node_ids()
    }

    pub fn run_node(&mut self, node_id: u64) -> ServerResult<()> {
        debug!("starting node {}", node_id);
        let engines = self.engines[&node_id].clone();
        let key_mgr = self.key_managers_map[&node_id].clone();
        let (router, system) =
            create_raft_batch_system(&self.cfg.raft_store, &self.resource_manager);
        let mut cfg = self.cfg.clone();
        if let Some(labels) = self.labels.get(&node_id) {
            cfg.server.labels = labels.to_owned();
        }
        let store_meta = match self.store_metas.entry(node_id) {
            MapEntry::Occupied(o) => {
                let mut meta = o.get().lock().unwrap();
                *meta = StoreMeta::new(PENDING_MSG_CAP);
                o.get().clone()
            }
            MapEntry::Vacant(v) => v
                .insert(Arc::new(Mutex::new(StoreMeta::new(PENDING_MSG_CAP))))
                .clone(),
        };
        let props = GroupProperties::default();
        self.group_props.insert(node_id, props.clone());
        tikv_util::thread_group::set_properties(Some(props));
        debug!("calling run node"; "node_id" => node_id);
        // FIXME: rocksdb event listeners may not work, because we change the router.
        self.sim.wl().run_node(
            node_id,
            cfg,
            engines,
            store_meta,
            key_mgr,
            router,
            system,
            &self.resource_manager,
        )?;
        debug!("node {} started", node_id);
        Ok(())
    }

    pub fn stop_node(&mut self, node_id: u64) {
        debug!("stopping node {}", node_id);
        self.group_props[&node_id].mark_shutdown();
        // Simulate shutdown behavior of server shutdown. It's not enough to just set
        // the map above as current thread may also query properties during shutdown.
        let previous_prop = tikv_util::thread_group::current_properties();
        tikv_util::thread_group::set_properties(Some(self.group_props[&node_id].clone()));
        match self.sim.write() {
            Ok(mut sim) => sim.stop_node(node_id),
            Err(_) => safe_panic!("failed to acquire write lock."),
        }
        self.pd_client.shutdown_store(node_id);
        debug!("node {} stopped", node_id);
        tikv_util::thread_group::set_properties(previous_prop);
    }

    pub fn get_engine(&self, node_id: u64) -> RocksEngine {
        self.engines[&node_id].kv.clone()
    }

    pub fn get_raft_engine(&self, node_id: u64) -> RaftTestEngine {
        self.engines[&node_id].raft.clone()
    }

    pub fn get_all_engines(&self, node_id: u64) -> Engines<RocksEngine, RaftTestEngine> {
        self.engines[&node_id].clone()
    }

    pub fn send_raft_msg(&mut self, msg: RaftMessage) -> Result<()> {
        self.sim.wl().send_raft_msg(msg)
    }

    pub fn call_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        match self
            .sim
            .rl()
            .call_command_on_node(node_id, request.clone(), timeout)
        {
            Err(e) => {
                warn!("failed to call command {:?}: {:?}", request, e);
                Err(e)
            }
            a => a,
        }
    }

    pub fn read(
        &self,
        batch_id: Option<ThreadReadId>,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        match self.sim.wl().read(batch_id, request.clone(), timeout) {
            Err(e) => {
                warn!("failed to read {:?}: {:?}", request, e);
                Err(e)
            }
            a => a,
        }
    }

    pub fn call_command(
        &self,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let mut is_read = false;
        for req in request.get_requests() {
            match req.get_cmd_type() {
                CmdType::Get | CmdType::Snap | CmdType::ReadIndex => {
                    is_read = true;
                }
                _ => (),
            }
        }
        let ret = if is_read {
            self.sim.wl().read(None, request.clone(), timeout)
        } else {
            self.sim.rl().call_command(request.clone(), timeout)
        };
        match ret {
            Err(e) => {
                warn!("failed to call command {:?}: {:?}", request, e);
                Err(e)
            }
            a => a,
        }
    }

    pub fn call_command_on_leader(
        &mut self,
        mut request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let timer = Instant::now();
        let region_id = request.get_header().get_region_id();
        loop {
            let leader = match self.leader_of_region(region_id) {
                None => return Err(Error::NotLeader(region_id, None)),
                Some(l) => l,
            };
            request.mut_header().set_peer(leader);
            let resp = match self.call_command(request.clone(), timeout) {
                e @ Err(_) => return e,
                Ok(resp) => resp,
            };
            if self.refresh_leader_if_needed(&resp, region_id)
                && timer.saturating_elapsed() < timeout
            {
                warn!(
                    "{:?} is no longer leader, let's retry",
                    request.get_header().get_peer()
                );
                continue;
            }
            return Ok(resp);
        }
    }

    fn valid_leader_id(&self, region_id: u64, leader_id: u64) -> bool {
        let store_ids = match self.voter_store_ids_of_region(region_id) {
            None => return false,
            Some(ids) => ids,
        };
        let node_ids = self.sim.rl().get_node_ids();
        store_ids.contains(&leader_id) && node_ids.contains(&leader_id)
    }

    fn voter_store_ids_of_region(&self, region_id: u64) -> Option<Vec<u64>> {
        block_on(self.pd_client.get_region_by_id(region_id))
            .unwrap()
            .map(|region| {
                region
                    .get_peers()
                    .iter()
                    .flat_map(|p| {
                        if p.get_role() != PeerRole::Learner {
                            Some(p.get_store_id())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
    }

    pub fn query_leader(
        &self,
        store_id: u64,
        region_id: u64,
        timeout: Duration,
    ) -> Option<metapb::Peer> {
        // To get region leader, we don't care real peer id, so use 0 instead.
        let peer = new_peer(store_id, 0);
        let find_leader = new_status_request(region_id, peer, new_region_leader_cmd());
        let mut resp = match self.call_command(find_leader, timeout) {
            Ok(resp) => resp,
            Err(err) => {
                error!(
                    "fail to get leader of region {} on store {}, error: {:?}",
                    region_id, store_id, err
                );
                return None;
            }
        };
        let mut region_leader = resp.take_status_response().take_region_leader();
        // NOTE: node id can't be 0.
        if self.valid_leader_id(region_id, region_leader.get_leader().get_store_id()) {
            Some(region_leader.take_leader())
        } else {
            None
        }
    }

    pub fn leader_of_region(&mut self, region_id: u64) -> Option<metapb::Peer> {
        let timer = Instant::now_coarse();
        let timeout = Duration::from_secs(5);
        let mut store_ids = None;
        while timer.saturating_elapsed() < timeout {
            match self.voter_store_ids_of_region(region_id) {
                None => thread::sleep(Duration::from_millis(10)),
                Some(ids) => {
                    store_ids = Some(ids);
                    break;
                }
            };
        }
        let store_ids = store_ids?;
        if let Some(l) = self.leaders.get(&region_id) {
            // leader may be stopped in some tests.
            if self.valid_leader_id(region_id, l.get_store_id()) {
                return Some(l.clone());
            }
        }
        self.reset_leader_of_region(region_id);
        let mut leader = None;
        let mut leaders = HashMap::default();

        let node_ids = self.sim.rl().get_node_ids();
        // For some tests, we stop the node but pd still has this information,
        // and we must skip this.
        let alive_store_ids: Vec<_> = store_ids
            .iter()
            .filter(|id| node_ids.contains(id))
            .cloned()
            .collect();
        while timer.saturating_elapsed() < timeout {
            for store_id in &alive_store_ids {
                let l = match self.query_leader(*store_id, region_id, Duration::from_secs(1)) {
                    None => continue,
                    Some(l) => l,
                };
                leaders
                    .entry(l.get_id())
                    .or_insert((l, vec![]))
                    .1
                    .push(*store_id);
            }
            if let Some((_, (l, c))) = leaders.iter().max_by_key(|(_, (_, c))| c.len()) {
                // It may be a step down leader.
                if c.contains(&l.get_store_id()) {
                    leader = Some(l.clone());
                    // Technically, correct calculation should use two quorum when in joint
                    // state. Here just for simplicity.
                    if c.len() > store_ids.len() / 2 {
                        break;
                    }
                }
            }
            debug!("failed to detect leaders"; "leaders" => ?leaders, "store_ids" => ?store_ids);
            sleep_ms(10);
            leaders.clear();
        }

        if let Some(l) = leader {
            self.leaders.insert(region_id, l);
        }

        self.leaders.get(&region_id).cloned()
    }

    pub fn check_regions_number(&self, len: u32) {
        assert_eq!(self.pd_client.get_regions_number() as u32, len)
    }

    // For test when a node is already bootstrapped the cluster with the first
    // region But another node may request bootstrap at same time and get
    // is_bootstrap false Add Region but not set bootstrap to true
    pub fn add_first_region(&self) -> Result<()> {
        let mut region = metapb::Region::default();
        let region_id = self.pd_client.alloc_id().unwrap();
        let peer_id = self.pd_client.alloc_id().unwrap();
        region.set_id(region_id);
        region.set_start_key(keys::EMPTY_KEY.to_vec());
        region.set_end_key(keys::EMPTY_KEY.to_vec());
        region.mut_region_epoch().set_version(INIT_EPOCH_VER);
        region.mut_region_epoch().set_conf_ver(INIT_EPOCH_CONF_VER);
        let peer = new_peer(peer_id, peer_id);
        region.mut_peers().push(peer);
        self.pd_client.add_region(&region);
        Ok(())
    }

    /// Multiple nodes with fixed node id, like node 1, 2, .. 5,
    /// First region 1 is in all stores with peer 1, 2, .. 5.
    /// Peer 1 is in node 1, store 1, etc.
    ///
    /// Must be called after `create_engines`.
    pub fn bootstrap_region(&mut self) -> Result<()> {
        for (i, engines) in self.dbs.iter().enumerate() {
            let id = i as u64 + 1;
            self.engines.insert(id, engines.clone());
            let store_meta = Arc::new(Mutex::new(StoreMeta::new(PENDING_MSG_CAP)));
            self.store_metas.insert(id, store_meta);
            self.key_managers_map
                .insert(id, self.key_managers[i].clone());
            self.sst_workers_map.insert(id, i);
        }

        let mut region = metapb::Region::default();
        region.set_id(1);
        region.set_start_key(keys::EMPTY_KEY.to_vec());
        region.set_end_key(keys::EMPTY_KEY.to_vec());
        region.mut_region_epoch().set_version(INIT_EPOCH_VER);
        region.mut_region_epoch().set_conf_ver(INIT_EPOCH_CONF_VER);

        for (&id, engines) in &self.engines {
            let peer = new_peer(id, id);
            region.mut_peers().push(peer.clone());
            bootstrap_store(engines, self.id(), id).unwrap();
        }

        for engines in self.engines.values() {
            prepare_bootstrap_cluster(engines, &region)?;
        }

        self.bootstrap_cluster(region);

        Ok(())
    }

    // Return first region id.
    pub fn bootstrap_conf_change(&mut self) -> u64 {
        for (i, engines) in self.dbs.iter().enumerate() {
            let id = i as u64 + 1;
            self.engines.insert(id, engines.clone());
            let store_meta = Arc::new(Mutex::new(StoreMeta::new(PENDING_MSG_CAP)));
            self.store_metas.insert(id, store_meta);
            self.key_managers_map
                .insert(id, self.key_managers[i].clone());
            self.sst_workers_map.insert(id, i);
        }

        for (&id, engines) in &self.engines {
            bootstrap_store(engines, self.id(), id).unwrap();
        }

        let node_id = 1;
        let region_id = 1;
        let peer_id = 1;

        let region = initial_region(node_id, region_id, peer_id);
        prepare_bootstrap_cluster(&self.engines[&node_id], &region).unwrap();
        self.bootstrap_cluster(region);
        region_id
    }

    // This is only for fixed id test.
    fn bootstrap_cluster(&mut self, region: metapb::Region) {
        self.pd_client
            .bootstrap_cluster(new_store(1, "".to_owned()), region)
            .unwrap();
        for id in self.engines.keys() {
            let mut store = new_store(*id, "".to_owned());
            if let Some(labels) = self.labels.get(id) {
                for (key, value) in labels.iter() {
                    store.labels.push(StoreLabel {
                        key: key.clone(),
                        value: value.clone(),
                        ..Default::default()
                    });
                }
            }
            self.pd_client.put_store(store).unwrap();
        }
    }

    pub fn add_label(&mut self, node_id: u64, key: &str, value: &str) {
        self.labels
            .entry(node_id)
            .or_default()
            .insert(key.to_owned(), value.to_owned());
    }

    pub fn add_new_engine(&mut self) -> u64 {
        self.create_engine(None);
        self.count += 1;
        let node_id = self.count as u64;

        let engines = self.dbs.last().unwrap().clone();
        bootstrap_store(&engines, self.id(), node_id).unwrap();
        self.engines.insert(node_id, engines);

        let key_mgr = self.key_managers.last().unwrap().clone();
        self.key_managers_map.insert(node_id, key_mgr);
        self.sst_workers_map
            .insert(node_id, self.sst_workers.len() - 1);

        self.run_node(node_id).unwrap();
        node_id
    }

    pub fn reset_leader_of_region(&mut self, region_id: u64) {
        self.leaders.remove(&region_id);
    }

    pub fn assert_quorum<F: FnMut(&RocksEngine) -> bool>(&self, mut condition: F) {
        if self.engines.is_empty() {
            return;
        }
        let half = self.engines.len() / 2;
        let mut qualified_cnt = 0;
        for (id, engines) in &self.engines {
            if !condition(&engines.kv) {
                debug!("store {} is not qualified yet.", id);
                continue;
            }
            debug!("store {} is qualified", id);
            qualified_cnt += 1;
            if half < qualified_cnt {
                return;
            }
        }

        panic!(
            "need at lease {} qualified stores, but only got {}",
            half + 1,
            qualified_cnt
        );
    }

    pub fn shutdown(&mut self) {
        debug!("about to shutdown cluster");
        let keys = match self.sim.read() {
            Ok(s) => s.get_node_ids(),
            Err(_) => {
                safe_panic!("failed to acquire read lock");
                // Leave the resource to avoid double panic.
                return;
            }
        };
        for id in keys {
            self.stop_node(id);
        }
        self.leaders.clear();
        self.store_metas.clear();
        for sst_worker in self.sst_workers.drain(..) {
            sst_worker.stop_worker();
        }
        debug!("all nodes are shut down.");
    }

    // If the resp is "not leader error", get the real leader.
    // Otherwise reset or refresh leader if needed.
    // Returns if the request should retry.
    fn refresh_leader_if_needed(&mut self, resp: &RaftCmdResponse, region_id: u64) -> bool {
        if !is_error_response(resp) {
            return false;
        }

        let err = resp.get_header().get_error();
        if err
            .get_message()
            .contains("peer has not applied to current term")
        {
            // leader peer has not applied to current term
            return true;
        }

        // If command is stale, leadership may have changed.
        // EpochNotMatch is not checked as leadership is checked first in raftstore.
        if err.has_stale_command() {
            self.reset_leader_of_region(region_id);
            return true;
        }

        if !err.has_not_leader() {
            return false;
        }
        let err = err.get_not_leader();
        if !err.has_leader() {
            self.reset_leader_of_region(region_id);
            return true;
        }
        self.leaders.insert(region_id, err.get_leader().clone());
        true
    }

    pub fn request(
        &mut self,
        key: &[u8],
        reqs: Vec<Request>,
        read_quorum: bool,
        timeout: Duration,
    ) -> RaftCmdResponse {
        let timer = Instant::now();
        let mut tried_times = 0;
        // At least retry once.
        while tried_times < 2 || timer.saturating_elapsed() < timeout {
            tried_times += 1;
            let mut region = self.get_region(key);
            let region_id = region.get_id();
            let req = new_request(
                region_id,
                region.take_region_epoch(),
                reqs.clone(),
                read_quorum,
            );
            let result = self.call_command_on_leader(req, timeout);

            let resp = match result {
                e @ Err(Error::Timeout(_))
                | e @ Err(Error::NotLeader(..))
                | e @ Err(Error::StaleCommand) => {
                    warn!("call command failed, retry it"; "err" => ?e);
                    sleep_ms(100);
                    continue;
                }
                Err(e) => panic!("call command failed {:?}", e),
                Ok(resp) => resp,
            };

            if resp.get_header().get_error().has_epoch_not_match() {
                warn!("seems split, let's retry");
                sleep_ms(100);
                continue;
            }
            if resp
                .get_header()
                .get_error()
                .get_message()
                .contains("merging mode")
            {
                warn!("seems waiting for merge, let's retry");
                sleep_ms(100);
                continue;
            }
            return resp;
        }
        panic!("request timeout");
    }

    // Get region when the `filter` returns true.
    pub fn get_region_with<F>(&self, key: &[u8], filter: F) -> metapb::Region
    where
        F: Fn(&metapb::Region) -> bool,
    {
        for _ in 0..100 {
            if let Ok(region) = self.pd_client.get_region(key) {
                if filter(&region) {
                    return region;
                }
            }
            // We may meet range gap after split, so here we will
            // retry to get the region again.
            sleep_ms(20);
        }

        panic!("find no region for {}", log_wrappers::hex_encode_upper(key));
    }

    pub fn get_region(&self, key: &[u8]) -> metapb::Region {
        self.get_region_with(key, |_| true)
    }

    pub fn get_region_id(&self, key: &[u8]) -> u64 {
        self.get_region(key).get_id()
    }

    pub fn get_down_peers(&self) -> HashMap<u64, pdpb::PeerStats> {
        self.pd_client.get_down_peers()
    }

    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl(CF_DEFAULT, key, false)
    }

    pub fn get_cf(&mut self, cf: &str, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl(cf, key, false)
    }

    pub fn must_get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl(CF_DEFAULT, key, true)
    }

    fn get_impl(&mut self, cf: &str, key: &[u8], read_quorum: bool) -> Option<Vec<u8>> {
        let mut resp = self.request(
            key,
            vec![new_get_cf_cmd(cf, key)],
            read_quorum,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::Get);
        if resp.get_responses()[0].has_get() {
            Some(resp.mut_responses()[0].mut_get().take_value())
        } else {
            None
        }
    }

    pub fn async_request(
        &mut self,
        req: RaftCmdRequest,
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        self.async_request_with_opts(req, Default::default())
    }

    pub fn async_request_with_opts(
        &mut self,
        mut req: RaftCmdRequest,
        opts: RaftCmdExtraOpts,
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        let region_id = req.get_header().get_region_id();
        let leader = self.leader_of_region(region_id).unwrap();
        req.mut_header().set_peer(leader.clone());
        let (cb, mut rx) = make_cb(&req);
        self.sim
            .rl()
            .async_command_on_node_with_opts(leader.get_store_id(), req, cb, opts)?;
        Ok(Box::pin(async move {
            let fut = rx.next();
            fut.await.unwrap()
        }))
    }

    pub fn async_exit_joint(
        &mut self,
        region_id: u64,
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        let region = block_on(self.pd_client.get_region_by_id(region_id))
            .unwrap()
            .unwrap();
        let exit_joint = new_admin_request(
            region_id,
            region.get_region_epoch(),
            new_change_peer_v2_request(vec![]),
        );
        self.async_request(exit_joint)
    }

    pub fn async_put(
        &mut self,
        key: &[u8],
        value: &[u8],
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        let mut region = self.get_region(key);
        let reqs = vec![new_put_cmd(key, value)];
        let put = new_request(region.get_id(), region.take_region_epoch(), reqs, false);
        self.async_request(put)
    }

    pub fn async_remove_peer(
        &mut self,
        region_id: u64,
        peer: metapb::Peer,
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        let region = block_on(self.pd_client.get_region_by_id(region_id))
            .unwrap()
            .unwrap();
        let remove_peer = new_change_peer_request(ConfChangeType::RemoveNode, peer);
        let req = new_admin_request(region_id, region.get_region_epoch(), remove_peer);
        self.async_request(req)
    }

    pub fn async_add_peer(
        &mut self,
        region_id: u64,
        peer: metapb::Peer,
    ) -> Result<BoxFuture<'static, RaftCmdResponse>> {
        let region = block_on(self.pd_client.get_region_by_id(region_id))
            .unwrap()
            .unwrap();
        let add_peer = new_change_peer_request(ConfChangeType::AddNode, peer);
        let req = new_admin_request(region_id, region.get_region_epoch(), add_peer);
        self.async_request(req)
    }

    pub fn must_put(&mut self, key: &[u8], value: &[u8]) {
        self.must_put_cf(CF_DEFAULT, key, value);
    }

    pub fn must_put_cf(&mut self, cf: &str, key: &[u8], value: &[u8]) {
        if let Err(e) = self.batch_put(key, vec![new_put_cf_cmd(cf, key, value)]) {
            panic!("has error: {:?}", e);
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> result::Result<(), PbError> {
        self.batch_put(key, vec![new_put_cf_cmd(CF_DEFAULT, key, value)])
            .map(|_| ())
    }

    pub fn batch_put(
        &mut self,
        region_key: &[u8],
        reqs: Vec<Request>,
    ) -> result::Result<RaftCmdResponse, PbError> {
        let resp = self.request(region_key, reqs, false, Duration::from_secs(5));
        if resp.get_header().has_error() {
            Err(resp.get_header().get_error().clone())
        } else {
            Ok(resp)
        }
    }

    pub fn must_delete(&mut self, key: &[u8]) {
        self.must_delete_cf(CF_DEFAULT, key)
    }

    pub fn must_delete_cf(&mut self, cf: &str, key: &[u8]) {
        let resp = self.request(
            key,
            vec![new_delete_cmd(cf, key)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
    }

    pub fn must_delete_range_cf(&mut self, cf: &str, start: &[u8], end: &[u8]) {
        let resp = self.request(
            start,
            vec![new_delete_range_cmd(cf, start, end)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
    }

    pub fn must_notify_delete_range_cf(&mut self, cf: &str, start: &[u8], end: &[u8]) {
        let mut req = new_delete_range_cmd(cf, start, end);
        req.mut_delete_range().set_notify_only(true);
        let resp = self.request(start, vec![req], false, Duration::from_secs(5));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
    }

    pub fn must_flush_cf(&mut self, cf: &str, sync: bool) {
        for engines in &self.dbs {
            engines.kv.flush_cf(cf, sync).unwrap();
        }
    }

    pub fn must_get_buckets(&mut self, region_id: u64) -> BucketStat {
        let timer = Instant::now();
        let timeout = Duration::from_secs(5);
        let mut tried_times = 0;
        // At least retry once.
        while tried_times < 2 || timer.saturating_elapsed() < timeout {
            tried_times += 1;
            match self.pd_client.get_buckets(region_id) {
                Some(buckets) => return buckets,
                None => sleep_ms(100),
            }
        }
        panic!("failed to get buckets for region {}", region_id);
    }

    pub fn get_region_epoch(&self, region_id: u64) -> RegionEpoch {
        block_on(self.pd_client.get_region_by_id(region_id))
            .unwrap()
            .unwrap()
            .take_region_epoch()
    }

    pub fn region_detail(&self, region_id: u64, store_id: u64) -> RegionDetailResponse {
        let status_cmd = new_region_detail_cmd();
        let peer = new_peer(store_id, 0);
        let req = new_status_request(region_id, peer, status_cmd);
        let resp = self.call_command(req, Duration::from_secs(5));
        assert!(resp.is_ok(), "{:?}", resp);

        let mut resp = resp.unwrap();
        assert!(resp.has_status_response());
        let mut status_resp = resp.take_status_response();
        assert_eq!(status_resp.get_cmd_type(), StatusCmdType::RegionDetail);
        assert!(status_resp.has_region_detail());
        status_resp.take_region_detail()
    }

    pub fn truncated_state(&self, region_id: u64, store_id: u64) -> RaftTruncatedState {
        self.apply_state(region_id, store_id).take_truncated_state()
    }

    pub fn wait_log_truncated(&self, region_id: u64, store_id: u64, index: u64) {
        let timer = Instant::now();
        loop {
            let truncated_state = self.truncated_state(region_id, store_id);
            if truncated_state.get_index() >= index {
                return;
            }
            if timer.saturating_elapsed() >= Duration::from_secs(5) {
                panic!(
                    "[region {}] log is still not truncated to {}: {:?} on store {}",
                    region_id, index, truncated_state, store_id,
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait_applied_index(&mut self, region_id: u64, store_id: u64, index: u64) {
        let timer = Instant::now();
        loop {
            let applied_index = self.apply_state(region_id, store_id).applied_index;
            if applied_index >= index {
                return;
            }
            if timer.saturating_elapsed() >= Duration::from_secs(5) {
                panic!(
                    "[region {}] log is still not applied to {}: {} on store {}",
                    region_id, index, applied_index, store_id,
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait_tombstone(&self, region_id: u64, peer: metapb::Peer, check_exist: bool) {
        let timer = Instant::now();
        let mut state;
        loop {
            state = self.region_local_state(region_id, peer.get_store_id());
            if state.get_state() == PeerState::Tombstone
                && (!check_exist || state.get_region().get_peers().contains(&peer))
            {
                return;
            }
            if timer.saturating_elapsed() > Duration::from_secs(5) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "{:?} is still not gc in region {} {:?}",
            peer, region_id, state
        );
    }

    pub fn wait_destroy_and_clean(&self, region_id: u64, peer: metapb::Peer) {
        let timer = Instant::now();
        self.wait_tombstone(region_id, peer.clone(), false);
        let mut state;
        loop {
            state = self.get_raft_local_state(region_id, peer.get_store_id());
            if state.is_none() {
                return;
            }
            if timer.saturating_elapsed() > Duration::from_secs(5) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "{:?} is still not cleaned in region {} {:?}",
            peer, region_id, state
        );
    }

    pub fn apply_state(&self, region_id: u64, store_id: u64) -> RaftApplyState {
        let key = keys::apply_state_key(region_id);
        self.get_engine(store_id)
            .get_msg_cf::<RaftApplyState>(engine_traits::CF_RAFT, &key)
            .unwrap()
            .unwrap_or_default()
    }

    pub fn get_raft_local_state(&self, region_id: u64, store_id: u64) -> Option<RaftLocalState> {
        self.engines[&store_id]
            .raft
            .get_raft_state(region_id)
            .unwrap()
    }

    pub fn raft_local_state(&self, region_id: u64, store_id: u64) -> RaftLocalState {
        self.get_raft_local_state(region_id, store_id).unwrap()
    }

    pub fn region_local_state(&self, region_id: u64, store_id: u64) -> RegionLocalState {
        self.get_engine(store_id)
            .get_msg_cf::<RegionLocalState>(
                engine_traits::CF_RAFT,
                &keys::region_state_key(region_id),
            )
            .unwrap()
            .unwrap()
    }

    pub fn must_peer_state(&self, region_id: u64, store_id: u64, peer_state: PeerState) {
        for _ in 0..100 {
            let state = self
                .get_engine(store_id)
                .get_msg_cf::<RegionLocalState>(
                    engine_traits::CF_RAFT,
                    &keys::region_state_key(region_id),
                )
                .unwrap()
                .unwrap();
            if state.get_state() == peer_state {
                return;
            }
            sleep_ms(10);
        }
        panic!(
            "[region {}] peer state still not reach {:?}",
            region_id, peer_state
        );
    }

    pub fn wait_peer_state(&self, region_id: u64, store_id: u64, peer_state: PeerState) {
        for _ in 0..100 {
            if let Some(state) = self
                .get_engine(store_id)
                .get_msg_cf::<RegionLocalState>(
                    engine_traits::CF_RAFT,
                    &keys::region_state_key(region_id),
                )
                .unwrap()
                && state.get_state() == peer_state
            {
                return;
            }
            sleep_ms(10);
        }
        panic!(
            "[region {}] peer state still not reach {:?}",
            region_id, peer_state
        );
    }

    pub fn wait_peer_role(&self, region_id: u64, store_id: u64, peer_id: u64, role: PeerRole) {
        for _ in 0..100 {
            if let Some(state) = self
                .get_engine(store_id)
                .get_msg_cf::<RegionLocalState>(
                    engine_traits::CF_RAFT,
                    &keys::region_state_key(region_id),
                )
                .unwrap()
            {
                let peer = state
                    .get_region()
                    .get_peers()
                    .iter()
                    .find(|p| p.get_id() == peer_id)
                    .unwrap();
                if peer.role == role {
                    return;
                }
            }
            sleep_ms(10);
        }
        panic!(
            "[region {}] peer state still not reach {:?}",
            region_id, role
        );
    }

    pub fn wait_last_index(
        &mut self,
        region_id: u64,
        store_id: u64,
        expected: u64,
        timeout: Duration,
    ) {
        let timer = Instant::now();
        loop {
            let raft_state = self.raft_local_state(region_id, store_id);
            let cur_index = raft_state.get_last_index();
            if cur_index >= expected {
                return;
            }
            if timer.saturating_elapsed() >= timeout {
                panic!(
                    "[region {}] last index still not reach {}: {:?}",
                    region_id, expected, raft_state
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn restore_kv_meta(&self, region_id: u64, store_id: u64, snap: &RocksSnapshot) {
        let (meta_start, meta_end) = (
            keys::region_meta_prefix(region_id),
            keys::region_meta_prefix(region_id + 1),
        );
        let mut kv_wb = self.engines[&store_id].kv.write_batch();
        self.engines[&store_id]
            .kv
            .scan(CF_RAFT, &meta_start, &meta_end, false, |k, _| {
                kv_wb.delete(k).unwrap();
                Ok(true)
            })
            .unwrap();
        snap.scan(CF_RAFT, &meta_start, &meta_end, false, |k, v| {
            kv_wb.put(k, v).unwrap();
            Ok(true)
        })
        .unwrap();

        let (raft_start, raft_end) = (
            keys::region_raft_prefix(region_id),
            keys::region_raft_prefix(region_id + 1),
        );
        self.engines[&store_id]
            .kv
            .scan(CF_RAFT, &raft_start, &raft_end, false, |k, _| {
                kv_wb.delete(k).unwrap();
                Ok(true)
            })
            .unwrap();
        snap.scan(CF_RAFT, &raft_start, &raft_end, false, |k, v| {
            kv_wb.put(k, v).unwrap();
            Ok(true)
        })
        .unwrap();
        kv_wb.write().unwrap();
    }

    pub fn add_send_filter_on_node(&mut self, node_id: u64, filter: Box<dyn Filter>) {
        self.sim.wl().add_send_filter(node_id, filter);
    }

    pub fn clear_send_filter_on_node(&mut self, node_id: u64) {
        self.sim.wl().clear_send_filters(node_id);
    }

    pub fn add_recv_filter_on_node(&mut self, node_id: u64, filter: Box<dyn Filter>) {
        self.sim.wl().add_recv_filter(node_id, filter);
    }

    pub fn add_send_filter<F: FilterFactory>(&self, factory: F) {
        let mut sim = self.sim.wl();
        for node_id in sim.get_node_ids() {
            for filter in factory.generate(node_id) {
                sim.add_send_filter(node_id, filter);
            }
        }
    }

    pub fn clear_recv_filter_on_node(&mut self, node_id: u64) {
        self.sim.wl().clear_recv_filters(node_id);
    }

    pub fn transfer_leader(&mut self, region_id: u64, leader: metapb::Peer) {
        let epoch = self.get_region_epoch(region_id);
        let transfer_leader = new_admin_request(region_id, &epoch, new_transfer_leader_cmd(leader));
        let resp = self
            .call_command_on_leader(transfer_leader, Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            resp.get_admin_response().get_cmd_type(),
            AdminCmdType::TransferLeader,
            "{:?}",
            resp
        );
    }

    pub fn must_transfer_leader(&mut self, region_id: u64, leader: metapb::Peer) {
        let timer = Instant::now();
        loop {
            self.reset_leader_of_region(region_id);
            let cur_leader = self.leader_of_region(region_id);
            if let Some(ref cur_leader) = cur_leader {
                if cur_leader.get_id() == leader.get_id()
                    && cur_leader.get_store_id() == leader.get_store_id()
                {
                    return;
                }
            }
            if timer.saturating_elapsed() > Duration::from_secs(5) {
                panic!(
                    "failed to transfer leader to [{}] {:?}, current leader: {:?}",
                    region_id, leader, cur_leader
                );
            }
            self.transfer_leader(region_id, leader.clone());
        }
    }

    pub fn try_transfer_leader(&mut self, region_id: u64, leader: metapb::Peer) -> RaftCmdResponse {
        let epoch = self.get_region_epoch(region_id);
        let transfer_leader = new_admin_request(region_id, &epoch, new_transfer_leader_cmd(leader));
        self.call_command_on_leader(transfer_leader, Duration::from_secs(5))
            .unwrap()
    }

    pub fn try_transfer_leader_with_timeout(
        &mut self,
        region_id: u64,
        leader: metapb::Peer,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let epoch = self.get_region_epoch(region_id);
        let transfer_leader = new_admin_request(region_id, &epoch, new_transfer_leader_cmd(leader));
        self.call_command_on_leader(transfer_leader, timeout)
    }

    pub fn get_snap_dir(&self, node_id: u64) -> String {
        self.sim.rl().get_snap_dir(node_id)
    }

    pub fn get_snap_mgr(&self, node_id: u64) -> SnapManager {
        self.sim.rl().get_snap_mgr(node_id).clone()
    }

    pub fn clear_send_filters(&mut self) {
        let mut sim = self.sim.wl();
        for node_id in sim.get_node_ids() {
            sim.clear_send_filters(node_id);
        }
    }

    // It's similar to `ask_split`, the difference is the msg, it sends, is
    // `Msg::SplitRegion`, and `region` will not be embedded to that msg.
    // Caller must ensure that the `split_key` is in the `region`.
    pub fn split_region(
        &mut self,
        region: &metapb::Region,
        split_key: &[u8],
        cb: Callback<RocksSnapshot>,
    ) {
        let leader = self.leader_of_region(region.get_id()).unwrap();
        let router = self.sim.rl().get_router(leader.get_store_id()).unwrap();
        let split_key = split_key.to_vec();
        CasualRouter::send(
            &router,
            region.get_id(),
            CasualMessage::SplitRegion {
                region_epoch: region.get_region_epoch().clone(),
                split_keys: vec![split_key],
                callback: cb,
                source: "test".into(),
                share_source_region_size: false,
            },
        )
        .unwrap();
    }

    pub fn enter_force_leader(&mut self, region_id: u64, store_id: u64, failed_stores: Vec<u64>) {
        let mut plan = pdpb::RecoveryPlan::default();
        let mut force_leader = pdpb::ForceLeader::default();
        force_leader.set_enter_force_leaders([region_id].to_vec());
        force_leader.set_failed_stores(failed_stores.to_vec());
        plan.set_force_leader(force_leader);
        // Triggers the unsafe recovery plan execution.
        self.pd_client.must_set_unsafe_recovery_plan(store_id, plan);
        self.must_send_store_heartbeat(store_id);
    }

    pub fn must_enter_force_leader(
        &mut self,
        region_id: u64,
        store_id: u64,
        failed_stores: Vec<u64>,
    ) -> StoreReport {
        self.enter_force_leader(region_id, store_id, failed_stores);
        let mut store_report = None;
        for _ in 0..20 {
            store_report = self.pd_client.must_get_store_report(store_id);
            if store_report.is_some() {
                break;
            }
            sleep_ms(100);
        }
        assert_ne!(store_report, None);
        store_report.unwrap()
    }

    pub fn exit_force_leader(&mut self, region_id: u64, store_id: u64) {
        let router = self.sim.rl().get_router(store_id).unwrap();
        router
            .significant_send(region_id, SignificantMsg::ExitForceLeaderState)
            .unwrap();
    }

    pub fn must_send_flashback_msg(
        &mut self,
        region_id: u64,
        cmd_type: AdminCmdType,
    ) -> BoxFuture<'static, RaftCmdResponse> {
        let leader = self.leader_of_region(region_id).unwrap();
        let store_id = leader.get_store_id();
        let region_epoch = self.get_region_epoch(region_id);
        let mut admin = AdminRequest::default();
        admin.set_cmd_type(cmd_type);
        let mut req = RaftCmdRequest::default();
        req.mut_header().set_region_id(region_id);
        req.mut_header().set_region_epoch(region_epoch);
        req.mut_header().set_peer(leader);
        req.set_admin_request(admin);
        req.mut_header()
            .set_flags(WriteBatchFlags::FLASHBACK.bits());
        let (result_tx, result_rx) = oneshot::channel();
        let router = self.sim.rl().get_router(store_id).unwrap();
        if let Err(e) = router.send_command(
            req,
            Callback::write(Box::new(move |resp| {
                result_tx.send(resp.response).unwrap();
            })),
            RaftCmdExtraOpts {
                deadline: None,
                disk_full_opt: DiskFullOpt::AllowedOnAlmostFull,
            },
        ) {
            panic!(
                "router send flashback msg {:?} failed, error: {}",
                cmd_type, e
            );
        }
        Box::pin(async move { result_rx.await.unwrap() })
    }

    pub fn must_send_wait_flashback_msg(&mut self, region_id: u64, cmd_type: AdminCmdType) {
        self.wait_applied_to_current_term(region_id, Duration::from_secs(3));
        let resp = self.must_send_flashback_msg(region_id, cmd_type);
        block_on(async {
            let resp = resp.await;
            if resp.get_header().has_error() {
                panic!(
                    "call flashback msg {:?} failed, error: {:?}",
                    cmd_type,
                    resp.get_header().get_error()
                );
            }
        });
    }

    pub fn wait_applied_to_current_term(&mut self, region_id: u64, timeout: Duration) {
        let mut now = Instant::now();
        let deadline = now + timeout;
        while now < deadline {
            if let Some(leader) = self.leader_of_region(region_id) {
                let raft_apply_state = self.apply_state(region_id, leader.get_store_id());
                let raft_local_state = self.raft_local_state(region_id, leader.get_store_id());
                // If term matches and apply to commit index, then it must apply to current
                // term.
                if raft_apply_state.applied_index == raft_apply_state.commit_index
                    && raft_apply_state.commit_term == raft_local_state.get_hard_state().get_term()
                {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(10));
            now = Instant::now();
        }
        panic!("region {} is not applied to current term", region_id,);
    }

    pub fn must_split(&mut self, region: &metapb::Region, split_key: &[u8]) {
        let mut try_cnt = 0;
        let split_count = self.pd_client.get_split_count();
        loop {
            debug!("asking split"; "region" => ?region, "key" => ?split_key);
            // In case ask split message is ignored, we should retry.
            if try_cnt % 50 == 0 {
                self.reset_leader_of_region(region.get_id());
                let key = split_key.to_vec();
                let check = Box::new(move |write_resp: WriteResponse| {
                    let mut resp = write_resp.response;
                    if resp.get_header().has_error() {
                        let error = resp.get_header().get_error();
                        if error.has_epoch_not_match()
                            || error.has_not_leader()
                            || error.has_stale_command()
                            || error
                                .get_message()
                                .contains("peer has not applied to current term")
                        {
                            warn!("fail to split: {:?}, ignore.", error);
                            return;
                        }
                        panic!("failed to split: {:?}", resp);
                    }
                    let admin_resp = resp.mut_admin_response();
                    let split_resp = admin_resp.mut_splits();
                    let regions = split_resp.get_regions();
                    assert_eq!(regions.len(), 2);
                    assert_eq!(regions[0].get_end_key(), key.as_slice());
                    assert_eq!(regions[0].get_end_key(), regions[1].get_start_key());
                });
                if self.leader_of_region(region.get_id()).is_some() {
                    self.split_region(region, split_key, Callback::write(check));
                }
            }

            if self.pd_client.check_split(region, split_key)
                && self.pd_client.get_split_count() > split_count
            {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {:?} has not been split by {}",
                    region,
                    log_wrappers::hex_encode_upper(split_key)
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    pub fn wait_region_split(&mut self, region: &metapb::Region) {
        self.wait_region_split_max_cnt(region, 20, 250, true);
    }

    pub fn wait_region_split_max_cnt(
        &mut self,
        region: &metapb::Region,
        itvl_ms: u64,
        max_try_cnt: u64,
        is_panic: bool,
    ) {
        let mut try_cnt = 0;
        let split_count = self.pd_client.get_split_count();
        loop {
            if self.pd_client.get_split_count() > split_count {
                match self.pd_client.get_region(region.get_start_key()) {
                    Err(_) => {}
                    Ok(left) => {
                        if left.get_end_key() != region.get_end_key() {
                            return;
                        }
                    }
                };
            }

            if try_cnt > max_try_cnt {
                if is_panic {
                    panic!(
                        "region {:?} has not been split after {}ms",
                        region,
                        max_try_cnt * itvl_ms
                    );
                } else {
                    return;
                }
            }
            try_cnt += 1;
            sleep_ms(itvl_ms);
        }
    }

    pub fn new_prepare_merge(&self, source: u64, target: u64) -> RaftCmdRequest {
        let region = block_on(self.pd_client.get_region_by_id(target))
            .unwrap()
            .unwrap();
        let prepare_merge = new_prepare_merge(region);
        let source_region = block_on(self.pd_client.get_region_by_id(source))
            .unwrap()
            .unwrap();
        new_admin_request(
            source_region.get_id(),
            source_region.get_region_epoch(),
            prepare_merge,
        )
    }

    pub fn merge_region(&mut self, source: u64, target: u64, cb: Callback<RocksSnapshot>) {
        let mut req = self.new_prepare_merge(source, target);
        let leader = self.leader_of_region(source).unwrap();
        req.mut_header().set_peer(leader.clone());
        self.sim
            .rl()
            .async_command_on_node(leader.get_store_id(), req, cb)
            .unwrap();
    }

    pub fn try_merge(&mut self, source: u64, target: u64) -> RaftCmdResponse {
        self.call_command_on_leader(
            self.new_prepare_merge(source, target),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    pub fn must_try_merge(&mut self, source: u64, target: u64) {
        let resp = self.try_merge(source, target);
        if is_error_response(&resp) {
            panic!(
                "{} failed to try merge to {}, resp {:?}",
                source, target, resp
            );
        }
    }

    /// Make sure region exists on that store.
    pub fn must_region_exist(&mut self, region_id: u64, store_id: u64) {
        let mut try_cnt = 0;
        loop {
            let find_leader =
                new_status_request(region_id, new_peer(store_id, 0), new_region_leader_cmd());
            let resp = self
                .call_command(find_leader, Duration::from_secs(5))
                .unwrap();

            if !is_error_response(&resp) {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {} doesn't exist on store {} after {} tries",
                    region_id, store_id, try_cnt
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    /// Make sure region not exists on that store.
    pub fn must_region_not_exist(&mut self, region_id: u64, store_id: u64) {
        let mut try_cnt = 0;
        loop {
            let status_cmd = new_region_detail_cmd();
            let peer = new_peer(store_id, 0);
            let req = new_status_request(region_id, peer, status_cmd);
            let resp = self.call_command(req, Duration::from_secs(5)).unwrap();
            if resp.get_header().has_error() && resp.get_header().get_error().has_region_not_found()
            {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {} still exists on store {} after {} tries: {:?}",
                    region_id, store_id, try_cnt, resp
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    pub fn must_remove_region(&mut self, store_id: u64, region_id: u64) {
        let timer = Instant::now();
        loop {
            let peer = new_peer(store_id, 0);
            let find_leader = new_status_request(region_id, peer, new_region_leader_cmd());
            let resp = self
                .call_command(find_leader, Duration::from_secs(5))
                .unwrap();

            if is_error_response(&resp) {
                assert!(
                    resp.get_header().get_error().has_region_not_found(),
                    "unexpected error resp: {:?}",
                    resp
                );
                break;
            }
            if timer.saturating_elapsed() > Duration::from_secs(60) {
                panic!("region {} is not removed after 60s.", region_id);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    // it's so common that we provide an API for it
    pub fn partition(&self, s1: Vec<u64>, s2: Vec<u64>) {
        self.add_send_filter(PartitionFilterFactory::new(s1, s2));
    }

    pub fn must_wait_for_leader_expire(&self, node_id: u64, region_id: u64) {
        let timer = Instant::now_coarse();
        while timer.saturating_elapsed() < Duration::from_secs(5) {
            if self
                .query_leader(node_id, region_id, Duration::from_secs(1))
                .is_none()
            {
                return;
            }
            sleep_ms(100);
        }
        panic!(
            "region {}'s replica in store {} still has a valid leader after 5 secs",
            region_id, node_id
        );
    }

    pub fn must_send_store_heartbeat(&self, node_id: u64) {
        let router = self.sim.rl().get_router(node_id).unwrap();
        StoreRouter::send(&router, StoreMsg::Tick(StoreTick::PdStoreHeartbeat)).unwrap();
    }

    pub fn gc_peer(
        &mut self,
        region_id: u64,
        node_id: u64,
        peer: metapb::Peer,
    ) -> std::result::Result<(), TrySendError<RaftMessage>> {
        let router = self.sim.rl().get_router(node_id).unwrap();

        let mut message = RaftMessage::default();
        message.set_region_id(region_id);
        message.set_from_peer(peer.clone());
        message.set_to_peer(peer);
        message.set_region_epoch(self.get_region_epoch(region_id));
        message.set_is_tombstone(true);
        router.send_raft_message(message)
    }

    pub fn must_gc_peer(&mut self, region_id: u64, node_id: u64, peer: metapb::Peer) {
        for _ in 0..250 {
            self.gc_peer(region_id, node_id, peer.clone()).unwrap();
            if self.region_local_state(region_id, node_id).get_state() == PeerState::Tombstone {
                return;
            }
            sleep_ms(20);
        }

        panic!(
            "gc peer timeout: region id {}, node id {}, peer {:?}",
            region_id, node_id, peer
        );
    }

    pub fn get_ctx(&mut self, key: &[u8]) -> Context {
        let region = self.get_region(key);
        let leader = self.leader_of_region(region.id).unwrap();
        let epoch = self.get_region_epoch(region.id);
        let mut ctx = Context::default();
        ctx.set_region_id(region.id);
        ctx.set_peer(leader);
        ctx.set_region_epoch(epoch);
        ctx
    }

    pub fn get_router(&self, node_id: u64) -> Option<RaftRouter<RocksEngine, RaftTestEngine>> {
        self.sim.rl().get_router(node_id)
    }

    pub fn get_apply_router(&self, node_id: u64) -> Option<ApplyRouter<RocksEngine>> {
        self.sim.rl().get_apply_router(node_id)
    }

    pub fn refresh_region_bucket_keys(
        &mut self,
        region: &metapb::Region,
        buckets: Vec<Bucket>,
        bucket_ranges: Option<Vec<BucketRange>>,
        expect_buckets: Option<Buckets>,
    ) -> u64 {
        let leader = self.leader_of_region(region.get_id()).unwrap();
        let router = self.sim.rl().get_router(leader.get_store_id()).unwrap();
        let (tx, rx) = mpsc::channel();
        let cb = Callback::Test {
            cb: Box::new(move |stat: PeerInternalStat| {
                if let Some(expect_buckets) = expect_buckets {
                    assert_eq!(expect_buckets.get_keys(), stat.buckets.keys);
                    assert_eq!(
                        expect_buckets.get_keys().len() - 1,
                        stat.buckets.sizes.len()
                    );
                }
                tx.send(stat.buckets.version).unwrap();
            }),
        };
        CasualRouter::send(
            &router,
            region.get_id(),
            CasualMessage::RefreshRegionBuckets {
                region_epoch: region.get_region_epoch().clone(),
                buckets,
                bucket_ranges,
                cb,
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    pub fn send_half_split_region_message(
        &mut self,
        region: &metapb::Region,
        expected_bucket_ranges: Option<Vec<BucketRange>>,
    ) {
        let leader = self.leader_of_region(region.get_id()).unwrap();
        let router = self.sim.rl().get_router(leader.get_store_id()).unwrap();
        let (tx, rx) = mpsc::channel();
        let cb = Callback::Test {
            cb: Box::new(move |stat: PeerInternalStat| {
                assert_eq!(
                    expected_bucket_ranges.is_none(),
                    stat.bucket_ranges.is_none()
                );
                if let Some(expected_bucket_ranges) = expected_bucket_ranges {
                    let actual_bucket_ranges = stat.bucket_ranges.unwrap();
                    assert_eq!(expected_bucket_ranges.len(), actual_bucket_ranges.len());
                    for i in 0..actual_bucket_ranges.len() {
                        assert_eq!(expected_bucket_ranges[i].0, actual_bucket_ranges[i].0);
                        assert_eq!(expected_bucket_ranges[i].1, actual_bucket_ranges[i].1);
                    }
                }
                tx.send(1).unwrap();
            }),
        };

        CasualRouter::send(
            &router,
            region.get_id(),
            CasualMessage::HalfSplitRegion {
                region_epoch: region.get_region_epoch().clone(),
                start_key: None,
                end_key: None,
                policy: CheckPolicy::Scan,
                source: "bucket",
                cb,
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    pub fn scan<F>(
        &self,
        store_id: u64,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        fill_cache: bool,
        f: F,
    ) -> engine_traits::Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> engine_traits::Result<bool>,
    {
        self.engines[&store_id]
            .kv
            .scan(cf, start_key, end_key, fill_cache, f)?;

        Ok(())
    }
}

impl<T: Simulator> Drop for Cluster<T> {
    fn drop(&mut self) {
        test_util::clear_failpoints();
        self.shutdown();
    }
}

pub trait RawEngine<EK: engine_traits::KvEngine>:
    Peekable<DbVector = EK::DbVector> + SyncMutable
{
    fn region_cache_engine(&self) -> bool {
        false
    }

    fn region_local_state(&self, region_id: u64)
    -> engine_traits::Result<Option<RegionLocalState>>;

    fn raft_apply_state(&self, _region_id: u64) -> engine_traits::Result<Option<RaftApplyState>>;

    fn raft_local_state(&self, _region_id: u64) -> engine_traits::Result<Option<RaftLocalState>>;
}

impl RawEngine<RocksEngine> for RocksEngine {
    fn region_local_state(
        &self,
        region_id: u64,
    ) -> engine_traits::Result<Option<RegionLocalState>> {
        self.get_msg_cf(CF_RAFT, &keys::region_state_key(region_id))
    }

    fn raft_apply_state(&self, region_id: u64) -> engine_traits::Result<Option<RaftApplyState>> {
        self.get_msg_cf(CF_RAFT, &keys::apply_state_key(region_id))
    }

    fn raft_local_state(&self, region_id: u64) -> engine_traits::Result<Option<RaftLocalState>> {
        self.get_msg_cf(CF_RAFT, &keys::raft_state_key(region_id))
    }
}
