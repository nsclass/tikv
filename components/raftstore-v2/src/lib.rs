// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

//! Raftstore is the place where we implement multi-raft.
//!
//! The thread module of raftstore is batch-system, more check
//! components/batch-system. All state machines are defined in [`fsm`] module.
//! Everything that wrapping raft is implemented in [`raft`] module. And the
//! commands, including split/merge/confchange/read/write, are implemented in
//! [`operation`] module. All state machines are expected to communicate with
//! messages. They are defined in [`router`] module.

// You may get confused about the peer, or other structs like apply, in fsm and
// peer in raft module. The guideline is that if any field doesn't depend on
// the details of batch system, then it should be defined for peer in raft
// module.
//
// If we change to other concurrent programming solution, we can easily just
// change the peer in fsm.
//
// Any accessors should be defined in the file where the struct is defined.
// Functionalities like read, write, etc should be implemented in [`operation`]
// using a standalone modules.

#![feature(let_chains)]
#![feature(array_windows)]
#![feature(box_into_inner)]
#![feature(assert_matches)]

mod batch;
mod bootstrap;
mod fsm;
mod operation;
mod raft;
pub mod router;
mod worker;

pub(crate) use batch::StoreContext;
pub use batch::{StoreRouter, StoreSystem, create_store_batch_system};
pub use bootstrap::Bootstrap;
pub use fsm::StoreMeta;
pub use operation::{SimpleWriteBinary, SimpleWriteEncoder, StateStorage, write_initial_states};
pub use raftstore::{Error, Result, store::Config};
pub use worker::{
    pd::{PdReporter, Task as PdTask},
    tablet::Task as TabletTask,
};

pub use crate::raft::Storage;
