// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    loader::{Loader, ModuleStorageAdapter},
    logging::expect_no_verification_errors,
};
use bytes::Bytes;
use move_binary_format::{
    deserializer::DeserializerConfig, errors::*, file_format::CompiledModule,
};
use move_core_types::{
    account_address::AccountAddress,
    effects::{AccountChanges, ChangeSet, Changes},
    gas_algebra::NumBytes,
    language_storage::{ModuleId, TypeTag},
    metadata::Metadata,
    resolver::MoveResolver,
    value::MoveTypeLayout,
    vm_status::StatusCode,
};
use move_vm_types::{
    loaded_data::runtime_types::Type,
    value_serde::deserialize_and_allow_delayed_values,
    values::{GlobalValue, Value},
};
use sha3::{Digest, Sha3_256};
use std::{
    collections::btree_map::{self, BTreeMap},
    sync::Arc,
};

pub struct AccountDataCache {
    // The bool flag in the `data_map` indicates whether the resource contains
    // an aggregator or snapshot.
    data_map: BTreeMap<Type, (MoveTypeLayout, GlobalValue, bool)>,
}

impl AccountDataCache {
    fn new() -> Self {
        Self {
            data_map: BTreeMap::new(),
        }
    }
}

/// Transaction data cache. Keep updates within a transaction so they can all be published at
/// once when the transaction succeeds.
///
/// It also provides an implementation for the opcodes that refer to storage and gives the
/// proper guarantees of reference lifetime.
///
/// Dirty objects are serialized and returned in make_write_set.
///
/// It is a responsibility of the client to publish changes once the transaction is executed.
///
/// The Move VM takes a `DataStore` in input and this is the default and correct implementation
/// for a data store related to a transaction. Clients should create an instance of this type
/// and pass it to the Move VM.
pub(crate) struct TransactionDataCache<'r> {
    remote: &'r dyn MoveResolver<PartialVMError>,
    account_map: BTreeMap<AccountAddress, AccountDataCache>,

    deserializer_config: DeserializerConfig,

    compiled_modules: BTreeMap<ModuleId, (Arc<CompiledModule>, usize, [u8; 32])>,
}

impl<'r> TransactionDataCache<'r> {
    /// Create a `TransactionDataCache` with a `RemoteCache` that provides access to data
    /// not updated in the transaction.
    pub(crate) fn new(
        deserializer_config: DeserializerConfig,
        remote: &'r impl MoveResolver<PartialVMError>,
    ) -> Self {
        TransactionDataCache {
            remote,
            account_map: BTreeMap::new(),
            deserializer_config,
            compiled_modules: BTreeMap::new(),
        }
    }

    /// Make a write set from the updated (dirty, deleted) global resources along with
    /// published modules.
    ///
    /// Gives all proper guarantees on lifetime of global data as well.
    pub(crate) fn into_effects(self, loader: &Loader) -> PartialVMResult<ChangeSet> {
        let resource_converter =
            |value: Value, layout: MoveTypeLayout, _: bool| -> PartialVMResult<Bytes> {
                value
                    .simple_serialize(&layout)
                    .map(Into::into)
                    .ok_or_else(|| {
                        PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)
                            .with_message(format!("Error when serializing resource {}.", value))
                    })
            };
        self.into_custom_effects(&resource_converter, loader)
    }

    /// Same like `into_effects`, but also allows clients to select the format of
    /// produced effects for resources.
    pub(crate) fn into_custom_effects<Resource>(
        self,
        resource_converter: &dyn Fn(Value, MoveTypeLayout, bool) -> PartialVMResult<Resource>,
        loader: &Loader,
    ) -> PartialVMResult<Changes<Bytes, Resource>> {
        let mut change_set = Changes::<Bytes, Resource>::new();
        for (addr, account_data_cache) in self.account_map.into_iter() {
            let mut resources = BTreeMap::new();
            for (ty, (layout, gv, has_aggregator_lifting)) in account_data_cache.data_map {
                if let Some(op) = gv.into_effect_with_layout(layout) {
                    let struct_tag = match loader.type_to_type_tag(&ty)? {
                        TypeTag::Struct(struct_tag) => *struct_tag,
                        _ => return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR)),
                    };
                    resources.insert(
                        struct_tag,
                        op.and_then(|(value, layout)| {
                            resource_converter(value, layout, has_aggregator_lifting)
                        })?,
                    );
                }
            }
            if !resources.is_empty() {
                change_set
                    .add_account_changeset(
                        addr,
                        AccountChanges::from_modules_resources(BTreeMap::new(), resources),
                    )
                    .expect("accounts should be unique");
            }
        }

        Ok(change_set)
    }

    fn get_mut_or_insert_with<'a, K, V, F>(map: &'a mut BTreeMap<K, V>, k: &K, gen: F) -> &'a mut V
    where
        F: FnOnce() -> (K, V),
        K: Ord,
    {
        if !map.contains_key(k) {
            let (k, v) = gen();
            map.insert(k, v);
        }
        map.get_mut(k).unwrap()
    }

    // Retrieves data from the local cache or loads it from the remote cache into the local cache.
    // All operations on the global data are based on this API and they all load the data
    // into the cache.
    pub(crate) fn load_resource(
        &mut self,
        loader: &Loader,
        addr: AccountAddress,
        ty: &Type,
        module_store: &ModuleStorageAdapter,
    ) -> PartialVMResult<(&mut GlobalValue, Option<NumBytes>)> {
        let account_cache = Self::get_mut_or_insert_with(&mut self.account_map, &addr, || {
            (addr, AccountDataCache::new())
        });

        let mut load_res = None;
        if !account_cache.data_map.contains_key(ty) {
            let ty_tag = match loader.type_to_type_tag(ty)? {
                TypeTag::Struct(s_tag) => s_tag,
                _ =>
                // non-struct top-level value; can't happen
                {
                    return Err(PartialVMError::new(StatusCode::INTERNAL_TYPE_ERROR))
                },
            };
            // TODO(Gas): Shall we charge for this?
            let (ty_layout, has_aggregator_lifting) =
                loader.type_to_type_layout_with_identifier_mappings(ty, module_store)?;

            let module = module_store.module_at(&ty_tag.module_id());
            let metadata: &[Metadata] = match &module {
                Some(module) => &module.module().metadata,
                None => &[],
            };

            // If we need to process aggregator lifting, we pass type layout to remote.
            // Remote, in turn ensures that all aggregator values are lifted if the resolved
            // resource comes from storage.
            let (data, bytes_loaded) = self.remote.get_resource_bytes_with_metadata_and_layout(
                &addr,
                &ty_tag,
                metadata,
                if has_aggregator_lifting {
                    Some(&ty_layout)
                } else {
                    None
                },
            )?;
            load_res = Some(NumBytes::new(bytes_loaded as u64));

            let gv = match data {
                Some(blob) => {
                    let val = match deserialize_and_allow_delayed_values(&blob, &ty_layout) {
                        Some(val) => val,
                        None => {
                            let msg =
                                format!("Failed to deserialize resource {} at {}!", ty_tag, addr);
                            return Err(PartialVMError::new(
                                StatusCode::FAILED_TO_DESERIALIZE_RESOURCE,
                            )
                            .with_message(msg));
                        },
                    };

                    GlobalValue::cached(val)?
                },
                None => GlobalValue::none(),
            };

            account_cache
                .data_map
                .insert(ty.clone(), (ty_layout, gv, has_aggregator_lifting));
        }

        Ok((
            account_cache
                .data_map
                .get_mut(ty)
                .map(|(_ty_layout, gv, _has_aggregator_lifting)| gv)
                .expect("global value must exist"),
            load_res,
        ))
    }

    pub(crate) fn load_compiled_module_to_cache(
        &mut self,
        id: ModuleId,
        allow_loading_failure: bool,
    ) -> VMResult<(Arc<CompiledModule>, usize, [u8; 32])> {
        let cache = &mut self.compiled_modules;
        match cache.entry(id) {
            btree_map::Entry::Occupied(entry) => Ok(entry.get().clone()),
            btree_map::Entry::Vacant(entry) => {
                // bytes fetching, allow loading to fail if the flag is set
                let module_id = entry.key();
                let bytes = match self
                    .remote
                    .get_module(module_id)
                    .map_err(|err| err.finish(Location::Undefined))?
                    .ok_or_else(|| {
                        PartialVMError::new(StatusCode::LINKER_ERROR).with_message(format!(
                            "Linker Error: Module {} doesn't exist",
                            module_id
                        ))
                    })
                    .map_err(|err| err.finish(Location::Undefined))
                {
                    Ok(bytes) => bytes,
                    Err(err) if allow_loading_failure => return Err(err),
                    Err(err) => {
                        return Err(expect_no_verification_errors(err));
                    },
                };

                let mut sha3_256 = Sha3_256::new();
                sha3_256.update(&bytes);
                let hash_value: [u8; 32] = sha3_256.finalize().into();

                // for bytes obtained from the data store, they should always deserialize and verify.
                // It is an invariant violation if they don't.
                let module =
                    CompiledModule::deserialize_with_config(&bytes, &self.deserializer_config)
                        .map_err(|err| {
                            let msg = format!("Deserialization error: {:?}", err);
                            PartialVMError::new(StatusCode::CODE_DESERIALIZATION_ERROR)
                                .with_message(msg)
                                .finish(Location::Module(entry.key().clone()))
                        })
                        .map_err(expect_no_verification_errors)?;

                Ok(entry
                    .insert((Arc::new(module), bytes.len(), hash_value))
                    .clone())
            },
        }
    }
}
