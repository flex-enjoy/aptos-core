// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::log::{
    CallFrame, EventStorage, ExecutionAndIOCosts, ExecutionGasEvent, FrameName, StorageFees,
    TransactionGasLog, WriteOpType, WriteStorage, WriteTransient,
};
use aptos_gas_algebra::{Fee, FeePerGasUnit, InternalGas, NumArgs, NumBytes};
use aptos_gas_meter::AptosGasMeter;
use aptos_types::{
    contract_event::ContractEvent, state_store::state_key::StateKey, write_set::WriteOp,
};
use aptos_vm_types::change_set::{GroupWrite, VMChangeSet};
use move_binary_format::{
    errors::{Location, PartialVMResult, VMResult},
    file_format::CodeOffset,
    file_format_common::Opcodes,
};
use move_core_types::{
    account_address::AccountAddress,
    identifier::Identifier,
    language_storage::{ModuleId, TypeTag},
};
use move_vm_types::{
    gas::{GasMeter, SimpleInstruction},
    views::{TypeView, ValueView},
};

/// A special gas meter adapter that records all gas-related events, along with the associated costs
/// assessed by the underlying gas meter.
pub struct GasProfiler<G> {
    base: G,

    intrinsic_cost: Option<InternalGas>,
    total_exec_io: InternalGas,
    frames: Vec<CallFrame>,
    write_set_transient: Vec<WriteTransient>,
    storage_fees: Option<StorageFees>,
}

// TODO: consider switching to a library like https://docs.rs/delegate/latest/delegate/.
macro_rules! delegate {
    ($(
        fn $fn: ident $(<$($lt: lifetime),*>)? (&self $(, $arg: ident : $ty: ty)* $(,)?) -> $ret_ty: ty;
    )*) => {
        $(fn $fn $(<$($lt)*>)? (&self, $($arg: $ty),*) -> $ret_ty {
            self.base.$fn($($arg),*)
        })*
    };
}

macro_rules! delegate_mut {
    ($(
        fn $fn: ident $(<$($lt: lifetime),*>)? (&mut self $(, $arg: ident : $ty: ty)* $(,)?) -> $ret_ty: ty;
    )*) => {
        $(fn $fn $(<$($lt)*>)? (&mut self, $($arg: $ty),*) -> $ret_ty {
            self.base.$fn($($arg),*)
        })*
    };
}

macro_rules! record_bytecode {
    ($(
        $([$op: expr])?
        fn $fn: ident $(<$($lt: lifetime),*>)? (&mut self $(, $arg: ident : $ty: ty)* $(,)?) -> PartialVMResult<()>;
    )*) => {
        $(fn $fn $(<$($lt)*>)? (&mut self, $($arg: $ty),*) -> PartialVMResult<()> {
            #[allow(unused)]
            use Opcodes::*;

            #[allow(unused)]
            let (cost, res) = self.delegate_charge(|base| base.$fn($($arg),*));

            $(
                self.record_bytecode($op, cost);
            )?

            res
        })*
    };
}

impl<G> GasProfiler<G> {
    pub fn new_script(base: G) -> Self {
        Self {
            base,

            intrinsic_cost: None,
            total_exec_io: 0.into(),
            frames: vec![CallFrame::new_script()],
            write_set_transient: vec![],
            storage_fees: None,
        }
    }

    pub fn new_function(
        base: G,
        module_id: ModuleId,
        func_name: Identifier,
        ty_args: Vec<TypeTag>,
    ) -> Self {
        Self {
            base,

            intrinsic_cost: None,
            total_exec_io: 0.into(),
            frames: vec![CallFrame::new_function(module_id, func_name, ty_args)],
            write_set_transient: vec![],
            storage_fees: None,
        }
    }
}

impl<G> GasProfiler<G>
where
    G: AptosGasMeter,
{
    fn active_event_stream(&mut self) -> &mut Vec<ExecutionGasEvent> {
        &mut self.frames.last_mut().unwrap().events
    }

    fn record_gas_event(&mut self, event: ExecutionGasEvent) {
        use ExecutionGasEvent::*;

        match &event {
            Loc(..) => (),
            Call(..) => unreachable!("call frames are handled separately"),
            Bytecode { cost, .. } | CallNative { cost, .. } | LoadResource { cost, .. } => {
                self.total_exec_io += *cost;
            },
        }

        self.active_event_stream().push(event);
    }

    fn record_bytecode(&mut self, op: Opcodes, cost: InternalGas) {
        self.record_gas_event(ExecutionGasEvent::Bytecode { op, cost })
    }

    fn record_offset(&mut self, offset: CodeOffset) {
        self.record_gas_event(ExecutionGasEvent::Loc(offset))
    }

    /// Delegate the charging call to the base gas meter and measure variation in balance.
    fn delegate_charge<F, R>(&mut self, charge: F) -> (InternalGas, R)
    where
        F: FnOnce(&mut G) -> R,
    {
        let old = self.base.balance_internal();
        let res = charge(&mut self.base);
        let new = self.base.balance_internal();
        let cost = old.checked_sub(new).expect("gas cost must be non-negative");

        (cost, res)
    }
}

impl<G> GasMeter for GasProfiler<G>
where
    G: AptosGasMeter,
{
    delegate_mut! {
        // Note: we only use this callback for memory tracking, not for charging gas.
        fn charge_ld_const_after_deserialization(&mut self, val: impl ValueView)
            -> PartialVMResult<()>;

        // Note: we don't use this to charge gas so no need to record anything.
        fn charge_native_function_before_execution(
            &mut self,
            ty_args: impl ExactSizeIterator<Item = impl TypeView> + Clone,
            args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;

        // Note: we don't use this to charge gas so no need to record anything.
        fn charge_drop_frame(
            &mut self,
            locals: impl Iterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;
    }

    record_bytecode! {
        [POP]
        fn charge_pop(&mut self, popped_val: impl ValueView) -> PartialVMResult<()>;

        [LD_CONST]
        fn charge_ld_const(&mut self, size: NumBytes) -> PartialVMResult<()>;

        [COPY_LOC]
        fn charge_copy_loc(&mut self, val: impl ValueView) -> PartialVMResult<()>;

        [MOVE_LOC]
        fn charge_move_loc(&mut self, val: impl ValueView) -> PartialVMResult<()>;

        [ST_LOC]
        fn charge_store_loc(&mut self, val: impl ValueView) -> PartialVMResult<()>;

        [PACK]
        fn charge_pack(
            &mut self,
            is_generic: bool,
            args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;

        [UNPACK]
        fn charge_unpack(
            &mut self,
            is_generic: bool,
            args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;

        [READ_REF]
        fn charge_read_ref(&mut self, val: impl ValueView) -> PartialVMResult<()>;

        [WRITE_REF]
        fn charge_write_ref(
            &mut self,
            new_val: impl ValueView,
            old_val: impl ValueView,
        ) -> PartialVMResult<()>;

        [EQ]
        fn charge_eq(&mut self, lhs: impl ValueView, rhs: impl ValueView) -> PartialVMResult<()>;

        [NEQ]
        fn charge_neq(&mut self, lhs: impl ValueView, rhs: impl ValueView) -> PartialVMResult<()>;

        [
            match (is_mut, is_generic) {
                (false, false) => IMM_BORROW_GLOBAL,
                (false, true) => IMM_BORROW_GLOBAL_GENERIC,
                (true, false) => MUT_BORROW_GLOBAL,
                (true, true) => MUT_BORROW_GLOBAL_GENERIC
            }
        ]
        fn charge_borrow_global(
            &mut self,
            is_mut: bool,
            is_generic: bool,
            ty: impl TypeView,
            is_success: bool,
        ) -> PartialVMResult<()>;

        [if is_generic { EXISTS } else { EXISTS_GENERIC }]
        fn charge_exists(
            &mut self,
            is_generic: bool,
            ty: impl TypeView,
            exists: bool,
        ) -> PartialVMResult<()>;

        [if is_generic { MOVE_FROM } else { MOVE_FROM_GENERIC }]
        fn charge_move_from(
            &mut self,
            is_generic: bool,
            ty: impl TypeView,
            val: Option<impl ValueView>,
        ) -> PartialVMResult<()>;

        [if is_generic { MOVE_TO } else { MOVE_TO_GENERIC }]
        fn charge_move_to(
            &mut self,
            is_generic: bool,
            ty: impl TypeView,
            val: impl ValueView,
            is_success: bool,
        ) -> PartialVMResult<()>;

        [VEC_PACK]
        fn charge_vec_pack<'a>(
            &mut self,
            ty: impl TypeView + 'a,
            args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;

        [VEC_LEN]
        fn charge_vec_len(&mut self, ty: impl TypeView) -> PartialVMResult<()>;

        [VEC_IMM_BORROW]
        fn charge_vec_borrow(
            &mut self,
            is_mut: bool,
            ty: impl TypeView,
            is_success: bool,
        ) -> PartialVMResult<()>;

        [VEC_PUSH_BACK]
        fn charge_vec_push_back(
            &mut self,
            ty: impl TypeView,
            val: impl ValueView,
        ) -> PartialVMResult<()>;

        [VEC_POP_BACK]
        fn charge_vec_pop_back(
            &mut self,
            ty: impl TypeView,
            val: Option<impl ValueView>,
        ) -> PartialVMResult<()>;

        [VEC_UNPACK]
        fn charge_vec_unpack(
            &mut self,
            ty: impl TypeView,
            expect_num_elements: NumArgs,
            elems: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        ) -> PartialVMResult<()>;

        [VEC_SWAP]
        fn charge_vec_swap(&mut self, ty: impl TypeView) -> PartialVMResult<()>;
    }

    fn balance_internal(&self) -> InternalGas {
        self.base.balance_internal()
    }

    fn charge_native_function(
        &mut self,
        amount: InternalGas,
        ret_vals: Option<impl ExactSizeIterator<Item = impl ValueView> + Clone>,
    ) -> PartialVMResult<()> {
        let (cost, res) =
            self.delegate_charge(|base| base.charge_native_function(amount, ret_vals));

        let cur = self.frames.pop().expect("frame must exist");
        let (module_id, name, ty_args) = match cur.name {
            FrameName::Function {
                module_id,
                name,
                ty_args,
            } => (module_id, name, ty_args),
            FrameName::Script => unreachable!(),
        };

        self.record_gas_event(ExecutionGasEvent::CallNative {
            module_id,
            fn_name: name,
            ty_args,
            cost,
        });

        res
    }

    fn charge_br_false(&mut self, target_offset: Option<CodeOffset>) -> PartialVMResult<()> {
        let (cost, res) = self.delegate_charge(|base| base.charge_br_false(target_offset));

        self.record_bytecode(Opcodes::BR_FALSE, cost);
        if let Some(offset) = target_offset {
            self.record_offset(offset);
        }

        res
    }

    fn charge_br_true(&mut self, target_offset: Option<CodeOffset>) -> PartialVMResult<()> {
        let (cost, res) = self.delegate_charge(|base| base.charge_br_true(target_offset));

        self.record_bytecode(Opcodes::BR_TRUE, cost);
        if let Some(offset) = target_offset {
            self.record_offset(offset);
        }

        res
    }

    fn charge_branch(&mut self, target_offset: CodeOffset) -> PartialVMResult<()> {
        let (cost, res) = self.delegate_charge(|base| base.charge_branch(target_offset));

        self.record_bytecode(Opcodes::BRANCH, cost);
        self.record_offset(target_offset);

        res
    }

    fn charge_simple_instr(&mut self, instr: SimpleInstruction) -> PartialVMResult<()> {
        let (cost, res) = self.delegate_charge(|base| base.charge_simple_instr(instr));

        self.record_bytecode(instr.to_opcode(), cost);

        // TODO: Right now we keep the last frame on the stack even after hitting the ret instruction,
        //       so that it can be picked up by finishing procedure.
        //       This is a bit hacky and can lead to weird behaviors if the profiler is used
        //       over multiple transactions, but again, guarding against that case is a broader
        //       problem we can deal with in the future.
        if matches!(instr, SimpleInstruction::Ret) && self.frames.len() > 1 {
            let cur_frame = self.frames.pop().expect("frame must exist");
            let last_frame = self.frames.last_mut().expect("frame must exist");
            last_frame.events.push(ExecutionGasEvent::Call(cur_frame));
        }

        res
    }

    fn charge_call(
        &mut self,
        module_id: &ModuleId,
        func_name: &str,
        args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        num_locals: NumArgs,
    ) -> PartialVMResult<()> {
        let (cost, res) =
            self.delegate_charge(|base| base.charge_call(module_id, func_name, args, num_locals));

        self.record_bytecode(Opcodes::CALL, cost);
        self.frames.push(CallFrame::new_function(
            module_id.clone(),
            Identifier::new(func_name).unwrap(),
            vec![],
        ));

        res
    }

    fn charge_call_generic(
        &mut self,
        module_id: &ModuleId,
        func_name: &str,
        ty_args: impl ExactSizeIterator<Item = impl TypeView> + Clone,
        args: impl ExactSizeIterator<Item = impl ValueView> + Clone,
        num_locals: NumArgs,
    ) -> PartialVMResult<()> {
        let ty_tags = ty_args
            .clone()
            .map(|ty| ty.to_type_tag())
            .collect::<Vec<_>>();

        let (cost, res) = self.delegate_charge(|base| {
            base.charge_call_generic(module_id, func_name, ty_args, args, num_locals)
        });

        self.record_bytecode(Opcodes::CALL_GENERIC, cost);
        self.frames.push(CallFrame::new_function(
            module_id.clone(),
            Identifier::new(func_name).unwrap(),
            ty_tags,
        ));

        res
    }

    fn charge_load_resource(
        &mut self,
        addr: AccountAddress,
        ty: impl TypeView,
        val: Option<impl ValueView>,
        bytes_loaded: NumBytes,
    ) -> PartialVMResult<()> {
        let ty_tag = ty.to_type_tag();

        let (cost, res) =
            self.delegate_charge(|base| base.charge_load_resource(addr, ty, val, bytes_loaded));

        self.record_gas_event(ExecutionGasEvent::LoadResource {
            addr,
            ty: ty_tag,
            cost,
        });

        res
    }
}

fn write_op_type(op: &WriteOp) -> WriteOpType {
    use WriteOp as O;
    use WriteOpType as T;

    match op {
        O::Creation(..) | O::CreationWithMetadata { .. } => T::Creation,
        O::Modification(..) | O::ModificationWithMetadata { .. } => T::Modification,
        O::Deletion | O::DeletionWithMetadata { .. } => T::Deletion,
    }
}

impl<G> AptosGasMeter for GasProfiler<G>
where
    G: AptosGasMeter,
{
    type Algebra = G::Algebra;

    delegate! {
        fn algebra(&self) -> &Self::Algebra;

        fn storage_fee_for_state_slot(&self, op: &WriteOp) -> Fee;

        fn storage_fee_refund_for_state_slot(&self, op: &WriteOp) -> Fee;

        fn storage_fee_for_state_bytes(&self, key: &StateKey, maybe_value_size: Option<u64>) -> Fee;

        fn storage_fee_per_event(&self, event: &ContractEvent) -> Fee;

        fn storage_discount_for_events(&self, total_cost: Fee) -> Fee;

        fn storage_fee_for_transaction_storage(&self, txn_size: NumBytes) -> Fee;
    }

    delegate_mut! {
        fn algebra_mut(&mut self) -> &mut Self::Algebra;

        fn charge_storage_fee(
            &mut self,
            amount: Fee,
            gas_unit_price: FeePerGasUnit,
        ) -> PartialVMResult<()>;
    }

    fn charge_io_gas_for_write(&mut self, key: &StateKey, op: &WriteOp) -> VMResult<()> {
        let (cost, res) = self.delegate_charge(|base| base.charge_io_gas_for_write(key, op));

        self.total_exec_io += cost;
        self.write_set_transient.push(WriteTransient {
            key: key.clone(),
            cost,
            op_type: write_op_type(op),
        });

        res
    }

    fn charge_io_gas_for_group_write(
        &mut self,
        key: &StateKey,
        group_write: &GroupWrite,
    ) -> VMResult<()> {
        let (cost, res) =
            self.delegate_charge(|base| base.charge_io_gas_for_group_write(key, group_write));

        self.total_exec_io += cost;
        self.write_set_transient.push(WriteTransient {
            key: key.clone(),
            cost,
            op_type: write_op_type(group_write.metadata_op()),
        });

        res
    }

    fn process_storage_fee_for_all(
        &mut self,
        change_set: &mut VMChangeSet,
        txn_size: NumBytes,
        gas_unit_price: FeePerGasUnit,
    ) -> VMResult<Fee> {
        // The new storage fee are only active since version 7.
        if self.feature_version() < 7 {
            return Ok(0.into());
        }

        // TODO(Gas): right now, some of our tests use a unit price of 0 and this is a hack
        // to avoid causing them issues. We should revisit the problem and figure out a
        // better way to handle this.
        if gas_unit_price.is_zero() {
            return Ok(0.into());
        }

        // Writes
        let mut write_fee = Fee::new(0);
        let mut write_set_storage = vec![];
        let mut total_refund = Fee::new(0);
        for (key, op) in change_set.write_set_iter_mut() {
            let slot_fee = self.storage_fee_for_state_slot(op);
            let slot_refund = self.storage_fee_refund_for_state_slot(op);
            let bytes_fee =
                self.storage_fee_for_state_bytes(key, op.bytes().map(|data| data.len() as u64));

            Self::maybe_record_storage_deposit(op, slot_fee);
            total_refund += slot_refund;

            let fee = slot_fee + bytes_fee;
            write_set_storage.push(WriteStorage {
                key: key.clone(),
                op_type: write_op_type(op),
                cost: fee,
            });
            // TODO(gas): track storage refund in the profiler
            write_fee += fee;
        }

        for (key, group_write) in change_set.group_write_set_iter_mut() {
            let group_metadata_op = &mut group_write.metadata_op_mut();

            let slot_fee = self.storage_fee_for_state_slot(group_metadata_op);
            let refund = self.storage_fee_refund_for_state_slot(group_metadata_op);

            Self::maybe_record_storage_deposit(group_metadata_op, slot_fee);
            total_refund += refund;

            let bytes_fee = self.storage_fee_for_state_bytes(key, group_write.encoded_group_size());

            let fee = slot_fee + bytes_fee;
            // TODO: should we distringuish group writes.
            write_set_storage.push(WriteStorage {
                key: key.clone(),
                op_type: write_op_type(group_write.metadata_op()),
                cost: fee,
            });

            write_fee += fee;
        }

        // Events
        let mut event_fee = Fee::new(0);
        let mut event_fees = vec![];
        for event in change_set.events().iter() {
            let fee = self.storage_fee_per_event(event);
            event_fees.push(EventStorage {
                ty: event.type_tag().clone(),
                cost: fee,
            });
            event_fee += fee;
        }
        let event_discount = self.storage_discount_for_events(event_fee);
        let event_fee_with_discount = event_fee
            .checked_sub(event_discount)
            .expect("discount should always be less than or equal to total amount");

        // Txn
        let txn_fee = self.storage_fee_for_transaction_storage(txn_size);

        self.storage_fees = Some(StorageFees {
            total: write_fee + event_fee + txn_fee,
            write_set_storage,
            events: event_fees,
            event_discount,
            txn_storage: txn_fee,
        });

        self.charge_storage_fee(
            write_fee + event_fee_with_discount + txn_fee,
            gas_unit_price,
        )
        .map_err(|err| err.finish(Location::Undefined))?;

        Ok(total_refund)
    }

    fn charge_intrinsic_gas_for_transaction(&mut self, txn_size: NumBytes) -> VMResult<()> {
        let (cost, res) =
            self.delegate_charge(|base| base.charge_intrinsic_gas_for_transaction(txn_size));

        self.intrinsic_cost = Some(cost);
        self.total_exec_io += cost;

        res
    }
}

impl<G> GasProfiler<G>
where
    G: AptosGasMeter,
{
    pub fn finish(mut self) -> TransactionGasLog {
        while self.frames.len() > 1 {
            let cur = self.frames.pop().expect("frame must exist");
            let last = self.frames.last_mut().expect("frame must exist");
            last.events.push(ExecutionGasEvent::Call(cur));
        }

        TransactionGasLog {
            exec_io: ExecutionAndIOCosts {
                gas_scaling_factor: self.base.gas_unit_scaling_factor(),
                total: self.total_exec_io,
                intrinsic_cost: self.intrinsic_cost.unwrap_or_else(|| 0.into()),
                call_graph: self.frames.pop().expect("frame must exist"),
                write_set_transient: self.write_set_transient,
            },
            storage: self.storage_fees.unwrap_or_else(|| StorageFees {
                total: 0.into(),
                write_set_storage: vec![],
                events: vec![],
                event_discount: 0.into(),
                txn_storage: 0.into(),
            }),
        }
    }
}
