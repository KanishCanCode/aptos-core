// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_state_view::StateView;
use aptos_types::{
    block_executor::partitioner::PartitionedTransactions, transaction::TransactionOutput,
};
use move_core_types::vm_status::VMStatus;
use std::sync::Arc;

pub struct ShardedExecutionOutput {
    pub sharded_output: Vec<Vec<TransactionOutput>>,
    pub global_output: Vec<TransactionOutput>,
}

impl ShardedExecutionOutput {
    pub fn new(
        sharded_output: Vec<Vec<TransactionOutput>>,
        global_output: Vec<TransactionOutput>,
    ) -> Self {
        Self {
            sharded_output,
            global_output,
        }
    }

    pub fn into_inner(self) -> (Vec<Vec<TransactionOutput>>, Vec<TransactionOutput>) {
        (self.sharded_output, self.global_output)
    }
}

// Interface to communicate from the block executor coordinator to the executor shards.
pub trait ExecutorClient<S: StateView + Sync + Send + 'static>: Send + Sync {
    fn num_shards(&self) -> usize;

    // A blocking call that executes the transactions in the block. It returns the execution results from each shard
    // and in the round order and also the global output.
    fn execute_block(
        &self,
        state_view: Arc<S>,
        transactions: PartitionedTransactions,
        concurrency_level_per_shard: usize,
        maybe_block_gas_limit: Option<u64>,
    ) -> Result<ShardedExecutionOutput, VMStatus>;
}
