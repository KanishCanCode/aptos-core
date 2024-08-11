// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::view::LatestView;
use aptos_types::{
    executable::Executable, state_store::TStateView,
    transaction::BlockExecutableTransaction as Transaction,
};
use aptos_vm_types::module_and_script_storage::module_storage::AptosModuleStorage;
use move_binary_format::{errors::PartialVMResult, CompiledModule};
use move_core_types::{account_address::AccountAddress, identifier::IdentStr, metadata::Metadata};
use move_vm_runtime::{Module, ModuleStorage};
use std::sync::Arc;

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> AptosModuleStorage
    for LatestView<'a, T, S, X>
{
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> ModuleStorage
    for LatestView<'a, T, S, X>
{
    fn check_module_exists(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> PartialVMResult<bool> {
        todo!()
    }

    fn fetch_module_size_in_bytes(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> PartialVMResult<usize> {
        todo!()
    }

    fn fetch_module_metadata(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> PartialVMResult<Vec<Metadata>> {
        todo!()
    }

    fn fetch_deserialized_module(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> PartialVMResult<Arc<CompiledModule>> {
        todo!()
    }

    fn fetch_or_create_verified_module(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
        _f: &dyn Fn(Arc<CompiledModule>) -> PartialVMResult<Module>,
    ) -> PartialVMResult<Arc<Module>> {
        todo!()
    }
}
