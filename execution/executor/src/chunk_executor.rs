// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use crate::{
    components::{
        apply_chunk_output::{ensure_no_discard, ensure_no_retry, ApplyChunkOutput},
        chunk_commit_queue::{ChunkCommitQueue, ChunkToUpdateLedger},
        chunk_output::ChunkOutput,
    },
    logging::{LogEntry, LogSchema},
    metrics::{
        APTOS_EXECUTOR_APPLY_CHUNK_SECONDS, APTOS_EXECUTOR_COMMIT_CHUNK_SECONDS,
        APTOS_EXECUTOR_EXECUTE_CHUNK_SECONDS, APTOS_EXECUTOR_LEDGER_UPDATE_OTHER_SECONDS,
        APTOS_EXECUTOR_VM_EXECUTE_CHUNK_SECONDS,
    },
};
use anyhow::{anyhow, ensure, Result};
use aptos_executor_types::{
    ChunkCommitNotification, ChunkExecutorTrait, ExecutedChunk, ParsedTransactionOutput,
    TransactionReplayer, VerifyExecutionMode,
};
use aptos_experimental_runtimes::thread_manager::optimal_min_len;
use aptos_infallible::{Mutex, RwLock};
use aptos_logger::prelude::*;
use aptos_metrics_core::TimerHelper;
use aptos_state_view::StateViewId;
use aptos_storage_interface::{
    async_proof_fetcher::AsyncProofFetcher, cached_state_view::CachedStateView,
    state_delta::StateDelta, DbReaderWriter, ExecutedTrees,
};
use aptos_types::{
    contract_event::ContractEvent,
    ledger_info::LedgerInfoWithSignatures,
    transaction::{
        signature_verified_transaction::SignatureVerifiedTransaction, Transaction, TransactionInfo,
        TransactionListWithProof, TransactionOutput, TransactionOutputListWithProof,
        TransactionStatus, Version,
    },
    write_set::WriteSet,
};
use aptos_vm::VMExecutor;
use fail::fail_point;
use itertools::multizip;
use once_cell::sync::Lazy;
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use std::{iter::once, marker::PhantomData, sync::Arc};

pub static SIG_VERIFY_POOL: Lazy<Arc<rayon::ThreadPool>> = Lazy::new(|| {
    Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(8) // More than 8 threads doesn't seem to help much
            .thread_name(|index| format!("signature-checker-{}", index))
            .build()
            .unwrap(),
    )
});

pub struct ChunkExecutor<V> {
    db: DbReaderWriter,
    inner: RwLock<Option<ChunkExecutorInner<V>>>,
}

impl<V: VMExecutor> ChunkExecutor<V> {
    pub fn new(db: DbReaderWriter) -> Self {
        Self {
            db,
            inner: RwLock::new(None),
        }
    }

    fn maybe_initialize(&self) -> Result<()> {
        if self.inner.read().is_none() {
            self.reset()?;
        }
        Ok(())
    }
}

impl<V: VMExecutor> ChunkExecutorTrait for ChunkExecutor<V> {
    fn enqueue_chunk_by_execution(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()> {
        self.maybe_initialize()?;
        self.inner
            .read()
            .as_ref()
            .expect("not reset")
            .enqueue_chunk_by_execution(txn_list_with_proof, verified_target_li, epoch_change_li)
    }

    fn enqueue_chunk_by_transaction_outputs(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()> {
        self.inner
            .read()
            .as_ref()
            .expect("not reset")
            .enqueue_chunk_by_transaction_outputs(
                txn_output_list_with_proof,
                verified_target_li,
                epoch_change_li,
            )
    }

    fn update_ledger(&self) -> Result<()> {
        self.inner
            .read()
            .as_ref()
            .expect("not reset")
            .update_ledger()
    }

    fn commit_chunk(&self) -> Result<ChunkCommitNotification> {
        self.inner
            .read()
            .as_ref()
            .expect("not reset")
            .commit_chunk()
    }

    fn reset(&self) -> Result<()> {
        *self.inner.write() = Some(ChunkExecutorInner::new(self.db.clone())?);
        Ok(())
    }

    fn finish(&self) {
        *self.inner.write() = None;
    }
}

struct ChunkExecutorInner<V> {
    db: DbReaderWriter,
    commit_queue: Mutex<ChunkCommitQueue>,
    _phantom: PhantomData<V>,
}

impl<V: VMExecutor> ChunkExecutorInner<V> {
    pub fn new(db: DbReaderWriter) -> Result<Self> {
        let commit_queue = Mutex::new(ChunkCommitQueue::new_from_db(&db.reader)?);
        Ok(Self {
            db,
            commit_queue,
            _phantom: PhantomData,
        })
    }

    fn latest_state_view(&self, latest_state: &StateDelta) -> Result<CachedStateView> {
        let first_version = latest_state.next_version();
        CachedStateView::new(
            StateViewId::ChunkExecution { first_version },
            self.db.reader.clone(),
            first_version,
            latest_state.current.clone(),
            Arc::new(AsyncProofFetcher::new(self.db.reader.clone())),
        )
    }

    fn commit_chunk_impl(&self) -> Result<ExecutedChunk> {
        let (persisted_state, chunk) = self.commit_queue.lock().next_chunk_to_commit()?;

        if chunk.ledger_info.is_some() || !chunk.transactions_to_commit().is_empty() {
            fail_point!("executor::commit_chunk", |_| {
                Err(anyhow::anyhow!("Injected error in commit_chunk"))
            });
            self.db.writer.save_transactions(
                chunk.transactions_to_commit(),
                persisted_state.next_version(),
                persisted_state.base_version,
                chunk.ledger_info.as_ref(),
                false, // sync_commit
                chunk.result_state.clone(),
                // TODO(aldenhu): avoid cloning
                chunk
                    .ledger_update_output
                    .state_updates_until_last_checkpoint
                    .clone(),
                Some(&chunk.ledger_update_output.sharded_state_cache),
            )?;
        }
        self.commit_queue
            .lock()
            .dequeue_committed(chunk.result_state.clone())?;

        Ok(chunk)
    }

    // ************************* Chunk Executor Implementation *************************
    fn enqueue_chunk_by_execution(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()> {
        let _timer = APTOS_EXECUTOR_EXECUTE_CHUNK_SECONDS.start_timer();

        let num_txns = txn_list_with_proof.transactions.len();
        ensure!(num_txns != 0, "Empty transaction list!");
        let first_version_in_request = txn_list_with_proof
            .first_transaction_version
            .ok_or_else(|| anyhow!("Non-empty chunk with first_version == None."))?;
        let parent_state = self.commit_queue.lock().latest_state();
        ensure!(
            first_version_in_request == parent_state.next_version(),
            "Unexpected chunk. version in request: {}, current_version: {:?}",
            first_version_in_request,
            parent_state.current_version,
        );

        verify_chunk(
            &txn_list_with_proof,
            verified_target_li,
            Some(first_version_in_request),
        )?;
        let TransactionListWithProof {
            transactions,
            events: _,
            first_transaction_version: _,
            proof: txn_infos_with_proof,
        } = txn_list_with_proof;
        let verified_target_li = verified_target_li.clone();
        let epoch_change_li = epoch_change_li.cloned();
        let known_state_checkpoints: Vec<_> = txn_infos_with_proof
            .transaction_infos
            .iter()
            .map(|t| t.state_checkpoint_hash())
            .collect();

        // TODO(skedia) In the chunk executor path, we ideally don't need to verify the signature
        // as only transactions with verified signatures are committed to the storage.
        let num_txns = transactions.len();
        let sig_verified_txns = SIG_VERIFY_POOL.install(|| {
            transactions
                .into_par_iter()
                .with_min_len(optimal_min_len(num_txns, 32))
                .map(|t| t.into())
                .collect::<Vec<_>>()
        });

        // Execute transactions.
        let state_view = self.latest_state_view(&parent_state)?;
        let chunk_output = {
            let _timer = APTOS_EXECUTOR_VM_EXECUTE_CHUNK_SECONDS.start_timer();
            // State sync executor shouldn't have block gas limit.
            ChunkOutput::by_transaction_execution::<V>(sig_verified_txns.into(), state_view, None)?
        };

        // Calcualte state snapshot
        let (result_state, next_epoch_state, state_checkpoint_output) =
            ApplyChunkOutput::calculate_state_checkpoint(
                chunk_output,
                &self.commit_queue.lock().latest_state(),
                None, // append_state_checkpoint_to_block
                Some(known_state_checkpoints),
                false, // is_block
            )?;

        // Enqueue for next stage.
        self.commit_queue
            .lock()
            .enqueue_for_ledger_update(ChunkToUpdateLedger {
                result_state,
                state_checkpoint_output,
                next_epoch_state,
                verified_target_li,
                epoch_change_li,
                txn_infos_with_proof,
            })?;

        info!(
            LogSchema::new(LogEntry::ChunkExecutor)
                .first_version_in_request(Some(first_version_in_request))
                .num_txns_in_request(num_txns),
            "Executed transaction chunk!",
        );

        Ok(())
    }

    fn enqueue_chunk_by_transaction_outputs(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()> {
        let _timer = APTOS_EXECUTOR_APPLY_CHUNK_SECONDS.start_timer();

        let num_txns = txn_output_list_with_proof.transactions_and_outputs.len();
        ensure!(num_txns != 0, "Empty transaction list!");
        let first_version_in_request = txn_output_list_with_proof
            .first_transaction_output_version
            .ok_or_else(|| anyhow!("Non-empty chunk with first_version == None."))?;
        let parent_state = self.commit_queue.lock().latest_state();
        ensure!(
            first_version_in_request == parent_state.next_version(),
            "Unexpected chunk. version in request: {}, current_version: {:?}",
            first_version_in_request,
            parent_state.current_version,
        );

        // Verify input transaction list.
        txn_output_list_with_proof.verify(
            verified_target_li.ledger_info(),
            Some(first_version_in_request),
        )?;
        let TransactionOutputListWithProof {
            transactions_and_outputs,
            first_transaction_output_version: _,
            proof: txn_infos_with_proof,
        } = txn_output_list_with_proof;
        let verified_target_li = verified_target_li.clone();
        let epoch_change_li = epoch_change_li.cloned();
        let known_state_checkpoints: Vec<_> = txn_infos_with_proof
            .transaction_infos
            .iter()
            .map(|t| t.state_checkpoint_hash())
            .collect();

        // Apply transaction outputs.
        let state_view = self.latest_state_view(&parent_state)?;
        let chunk_output =
            ChunkOutput::by_transaction_output(transactions_and_outputs, state_view)?;

        // Calculate state snapshot
        let (result_state, next_epoch_state, state_checkpoint_output) =
            ApplyChunkOutput::calculate_state_checkpoint(
                chunk_output,
                &self.commit_queue.lock().latest_state(),
                None, // append_state_checkpoint_to_block
                Some(known_state_checkpoints),
                false, // is_block
            )?;

        // Enqueue for next stage.
        self.commit_queue
            .lock()
            .enqueue_for_ledger_update(ChunkToUpdateLedger {
                result_state,
                state_checkpoint_output,
                next_epoch_state,
                verified_target_li,
                epoch_change_li,
                txn_infos_with_proof,
            })?;

        info!(
            LogSchema::new(LogEntry::ChunkExecutor)
                .first_version_in_request(Some(first_version_in_request))
                .num_txns_in_request(num_txns),
            "Applied transaction output chunk!",
        );

        Ok(())
    }

    pub fn update_ledger(&self) -> Result<()> {
        let _timer =
            APTOS_EXECUTOR_LEDGER_UPDATE_OTHER_SECONDS.timer_with(&["chunk_update_ledger_total"]);

        let (parent_accumulator, chunk) = self.commit_queue.lock().next_chunk_to_update_ledger()?;
        let ChunkToUpdateLedger {
            result_state,
            state_checkpoint_output,
            next_epoch_state,
            verified_target_li,
            epoch_change_li,
            txn_infos_with_proof,
        } = chunk;

        let first_version = parent_accumulator.num_leaves();
        let num_overlap = txn_infos_with_proof.verify_extends_ledger(
            first_version,
            parent_accumulator.root_hash(),
            Some(first_version),
        )?;
        assert_eq!(num_overlap, 0, "overlapped chunks");

        let (ledger_update_output, to_discard, to_retry) =
            ApplyChunkOutput::calculate_ledger_update(state_checkpoint_output, parent_accumulator)?;
        ensure!(to_discard.is_empty(), "Unexpected discard.");
        ensure!(to_retry.is_empty(), "Unexpected retry.");
        ledger_update_output
            .ensure_transaction_infos_match(&txn_infos_with_proof.transaction_infos)?;
        let ledger_info_opt = ledger_update_output.maybe_select_chunk_ending_ledger_info(
            &verified_target_li,
            epoch_change_li.as_ref(),
            next_epoch_state.as_ref(),
        )?;

        let executed_chunk = ExecutedChunk {
            result_state,
            ledger_info: ledger_info_opt,
            next_epoch_state,
            ledger_update_output,
        };
        let num_txns = executed_chunk.transactions_to_commit().len();

        self.commit_queue
            .lock()
            .save_ledger_update_output(executed_chunk)?;
        info!(
            LogSchema::new(LogEntry::ChunkExecutor)
                .first_version_in_request(Some(first_version))
                .num_txns_in_request(num_txns),
            "Calculated ledger update!",
        );
        Ok(())
    }

    fn commit_chunk(&self) -> Result<ChunkCommitNotification> {
        let _timer = APTOS_EXECUTOR_COMMIT_CHUNK_SECONDS.start_timer();
        let executed_chunk = self.commit_chunk_impl()?;

        Ok(executed_chunk.into_chunk_commit_notification())
    }
}

/// Verifies the transaction list proof against the ledger info and returns transactions
/// that are not already applied in the ledger.
#[cfg(not(feature = "consensus-only-perf-test"))]
fn verify_chunk(
    txn_list_with_proof: &TransactionListWithProof,
    verified_target_li: &LedgerInfoWithSignatures,
    first_version_in_request: Option<u64>,
) -> Result<()> {
    txn_list_with_proof.verify(verified_target_li.ledger_info(), first_version_in_request)
}

/// In consensus-only mode, the [TransactionListWithProof](transaction list) is *not*
/// verified against the proof and the [LedgerInfoWithSignatures](ledger info).
/// This is because the [FakeAptosDB] from where these transactions come from
/// returns an empty proof and not an actual proof, so proof verification will
/// fail regardless. This function does not skip any transactions that may be
/// already in the ledger, because it is not necessary as execution is disabled.
#[cfg(feature = "consensus-only-perf-test")]
fn verify_chunk(
    _txn_list_with_proof: &TransactionListWithProof,
    _verified_target_li: &LedgerInfoWithSignatures,
    _first_version_in_request: Option<u64>,
) -> Result<()> {
    // no-op: we do not verify the proof in consensus-only mode
    Ok(())
}

impl<V: VMExecutor> TransactionReplayer for ChunkExecutor<V> {
    fn replay(
        &self,
        transactions: Vec<Transaction>,
        transaction_infos: Vec<TransactionInfo>,
        write_sets: Vec<WriteSet>,
        event_vecs: Vec<Vec<ContractEvent>>,
        verify_execution_mode: &VerifyExecutionMode,
    ) -> Result<()> {
        self.maybe_initialize()?;
        self.inner.read().as_ref().expect("not reset").replay(
            transactions,
            transaction_infos,
            write_sets,
            event_vecs,
            verify_execution_mode,
        )
    }

    fn commit(&self) -> Result<ExecutedChunk> {
        self.inner.read().as_ref().expect("not reset").commit()
    }
}

impl<V: VMExecutor> TransactionReplayer for ChunkExecutorInner<V> {
    fn replay(
        &self,
        mut transactions: Vec<Transaction>,
        mut transaction_infos: Vec<TransactionInfo>,
        mut write_sets: Vec<WriteSet>,
        mut event_vecs: Vec<Vec<ContractEvent>>,
        verify_execution_mode: &VerifyExecutionMode,
    ) -> Result<()> {
        let mut latest_view = self.commit_queue.lock().expect_latest_view()?;
        let chunk_begin = latest_view.num_transactions() as Version;
        let chunk_end = chunk_begin + transactions.len() as Version; // right-exclusive

        // Find epoch boundaries.
        let mut epochs = Vec::new();
        let mut epoch_begin = chunk_begin; // epoch begin version
        for (version, events) in multizip((chunk_begin..chunk_end, event_vecs.iter())) {
            let is_epoch_ending = ParsedTransactionOutput::parse_reconfig_events(events)
                .next()
                .is_some();
            if is_epoch_ending {
                epochs.push((epoch_begin, version + 1));
                epoch_begin = version + 1;
            }
        }
        if epoch_begin < chunk_end {
            epochs.push((epoch_begin, chunk_end));
        }

        let mut executed_chunk = None;
        // Replay epoch by epoch.
        for (begin, end) in epochs {
            self.remove_and_replay_epoch(
                &mut executed_chunk,
                &mut latest_view,
                &mut transactions,
                &mut transaction_infos,
                &mut write_sets,
                &mut event_vecs,
                begin,
                end,
                verify_execution_mode,
            )?;
        }

        self.commit_queue
            .lock()
            .enqueue_chunk_to_commit_directly(executed_chunk.expect("Nothing to commit."))
    }

    fn commit(&self) -> Result<ExecutedChunk> {
        self.commit_chunk_impl()
    }
}

impl<V: VMExecutor> ChunkExecutorInner<V> {
    /// Remove `end_version - begin_version` transactions from the mutable input arguments and replay.
    /// The input range indicated by `[begin_version, end_version]` is guaranteed not to cross epoch boundaries.
    /// Notice there can be known broken versions inside the range.
    fn remove_and_replay_epoch(
        &self,
        executed_chunk: &mut Option<ExecutedChunk>,
        latest_view: &mut ExecutedTrees,
        transactions: &mut Vec<Transaction>,
        transaction_infos: &mut Vec<TransactionInfo>,
        write_sets: &mut Vec<WriteSet>,
        event_vecs: &mut Vec<Vec<ContractEvent>>,
        begin_version: Version,
        end_version: Version,
        verify_execution_mode: &VerifyExecutionMode,
    ) -> Result<()> {
        // we try to apply the txns in sub-batches split by known txns to skip and the end of the batch
        let txns_to_skip = verify_execution_mode.txns_to_skip();
        let mut batch_ends = txns_to_skip
            .range(begin_version..end_version)
            .chain(once(&end_version));

        let mut batch_begin = begin_version;
        let mut batch_end = *batch_ends.next().unwrap();
        while batch_begin < end_version {
            if batch_begin == batch_end {
                // batch_end is a known broken version that won't pass execution verification
                self.remove_and_apply(
                    executed_chunk,
                    latest_view,
                    transactions,
                    transaction_infos,
                    write_sets,
                    event_vecs,
                    batch_begin,
                    batch_begin + 1,
                )?;
                info!(
                    version_skipped = batch_begin,
                    "Skipped known broken transaction, applied transaction output directly."
                );
                batch_begin += 1;
                batch_end = *batch_ends.next().unwrap();
                continue;
            }

            // Try to run the transactions with the VM
            let next_begin = if verify_execution_mode.should_verify() {
                self.verify_execution(
                    latest_view,
                    transactions,
                    transaction_infos,
                    write_sets,
                    event_vecs,
                    batch_begin,
                    batch_end,
                    verify_execution_mode,
                )?
            } else {
                batch_end
            };
            self.remove_and_apply(
                executed_chunk,
                latest_view,
                transactions,
                transaction_infos,
                write_sets,
                event_vecs,
                batch_begin,
                next_begin,
            )?;
            batch_begin = next_begin;
        }

        Ok(())
    }

    fn verify_execution(
        &self,
        latest_view: &mut ExecutedTrees,
        transactions: &[Transaction],
        transaction_infos: &[TransactionInfo],
        write_sets: &[WriteSet],
        event_vecs: &[Vec<ContractEvent>],
        begin_version: Version,
        end_version: Version,
        verify_execution_mode: &VerifyExecutionMode,
    ) -> Result<Version> {
        // Execute transactions.
        let state_view = self.latest_state_view(latest_view.state())?;
        let txns = transactions
            .iter()
            .take((end_version - begin_version) as usize)
            .cloned()
            .map(|t| t.into())
            .collect::<Vec<SignatureVerifiedTransaction>>();

        // State sync executor shouldn't have block gas limit.
        let chunk_output =
            ChunkOutput::by_transaction_execution::<V>(txns.into(), state_view, None)?;
        // not `zip_eq`, deliberately
        for (version, txn_out, txn_info, write_set, events) in multizip((
            begin_version..end_version,
            chunk_output.transaction_outputs.iter(),
            transaction_infos.iter(),
            write_sets.iter(),
            event_vecs.iter(),
        )) {
            if let Err(err) = txn_out.ensure_match_transaction_info(
                version,
                txn_info,
                Some(write_set),
                Some(events),
            ) {
                if verify_execution_mode.is_lazy_quit() {
                    error!("(Not quitting right away.) {}", err);
                    verify_execution_mode.mark_seen_error();
                    return Ok(version + 1);
                } else {
                    return Err(err);
                }
            }
        }
        Ok(end_version)
    }

    /// Consume `end_version - begin_version` txns from the mutable input arguments
    /// It's guaranteed that there's no known broken versions or epoch endings in the range.
    fn remove_and_apply(
        &self,
        executed_chunk: &mut Option<ExecutedChunk>,
        latest_view: &mut ExecutedTrees,
        transactions: &mut Vec<Transaction>,
        transaction_infos: &mut Vec<TransactionInfo>,
        write_sets: &mut Vec<WriteSet>,
        event_vecs: &mut Vec<Vec<ContractEvent>>,
        begin_version: Version,
        end_version: Version,
    ) -> Result<()> {
        let num_txns = (end_version - begin_version) as usize;
        let txn_infos: Vec<_> = transaction_infos.drain(..num_txns).collect();
        let txns_and_outputs = multizip((
            transactions.drain(..num_txns),
            txn_infos.iter(),
            write_sets.drain(..num_txns),
            event_vecs.drain(..num_txns),
        ))
        .map(|(txn, txn_info, write_set, events)| {
            (
                txn,
                TransactionOutput::new(
                    write_set,
                    events,
                    txn_info.gas_used(),
                    TransactionStatus::Keep(txn_info.status().clone()),
                ),
            )
        })
        .collect();

        let state_view = self.latest_state_view(latest_view.state())?;
        let chunk_output = ChunkOutput::by_transaction_output(txns_and_outputs, state_view)?;
        let (executed_batch, to_discard, to_retry) = chunk_output.apply_to_ledger(
            latest_view,
            Some(
                txn_infos
                    .iter()
                    .map(|txn_info| txn_info.state_checkpoint_hash())
                    .collect(),
            ),
            None,
        )?;
        ensure_no_discard(to_discard)?;
        ensure_no_retry(to_retry)?;
        executed_batch
            .ledger_update_output
            .ensure_transaction_infos_match(&txn_infos)?;

        match executed_chunk {
            Some(chunk) => chunk.combine(executed_batch),
            None => *executed_chunk = Some(executed_batch),
        }
        *latest_view = executed_chunk.as_ref().unwrap().result_view();
        Ok(())
    }
}
