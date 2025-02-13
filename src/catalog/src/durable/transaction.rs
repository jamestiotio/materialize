// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use derivative::Derivative;
use itertools::Itertools;
use mz_audit_log::{VersionedEvent, VersionedStorageUsage};
use mz_controller_types::{ClusterId, ReplicaId};
use mz_ore::collections::CollectionExt;
use mz_ore::soft_assert;
use mz_proto::RustType;
use mz_repr::adt::mz_acl_item::{AclMode, MzAclItem};
use mz_repr::role_id::RoleId;
use mz_repr::{Diff, GlobalId};
use mz_sql::catalog::{
    CatalogError as SqlCatalogError, ObjectType, RoleAttributes, RoleMembership, RoleVars,
};
use mz_sql::names::{CommentObjectId, DatabaseId, SchemaId};
use mz_sql::session::user::MZ_SYSTEM_ROLE_ID;
use mz_sql_parser::ast::QualifiedReplica;
use mz_stash::TableTransaction;
use mz_storage_types::controller::PersistTxnTablesImpl;
use mz_storage_types::sources::Timeline;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::builtin::BuiltinLog;
use crate::durable::initialize::{PERSIST_TXN_TABLES, SYSTEM_CONFIG_SYNCED_KEY};
use crate::durable::objects::serialization::proto;
use crate::durable::objects::{
    AuditLogKey, Cluster, ClusterConfig, ClusterIntrospectionSourceIndexKey,
    ClusterIntrospectionSourceIndexValue, ClusterKey, ClusterReplica, ClusterReplicaKey,
    ClusterReplicaValue, ClusterValue, CommentKey, CommentValue, Config, ConfigKey, ConfigValue,
    Database, DatabaseKey, DatabaseValue, DefaultPrivilegesKey, DefaultPrivilegesValue,
    DurableType, GidMappingKey, GidMappingValue, IdAllocKey, IdAllocValue,
    IntrospectionSourceIndex, Item, ItemKey, ItemValue, ReplicaConfig, Role, RoleKey, RoleValue,
    Schema, SchemaKey, SchemaValue, ServerConfigurationKey, ServerConfigurationValue, SettingKey,
    SettingValue, StorageUsageKey, SystemObjectMapping, SystemPrivilegesKey, SystemPrivilegesValue,
    TimestampKey, TimestampValue,
};
use crate::durable::{
    CatalogError, Comment, DefaultPrivilege, DurableCatalogState, Snapshot, SystemConfiguration,
    TimelineTimestamp, CATALOG_CONTENT_VERSION_KEY, DATABASE_ID_ALLOC_KEY, SCHEMA_ID_ALLOC_KEY,
    SYSTEM_ITEM_ALLOC_KEY, USER_ITEM_ALLOC_KEY, USER_ROLE_ID_ALLOC_KEY,
};

/// A [`Transaction`] batches multiple catalog operations together and commits them atomically.
#[derive(Derivative)]
#[derivative(Debug, PartialEq)]
pub struct Transaction<'a> {
    #[derivative(Debug = "ignore")]
    #[derivative(PartialEq = "ignore")]
    durable_catalog: &'a mut dyn DurableCatalogState,
    databases: TableTransaction<DatabaseKey, DatabaseValue>,
    schemas: TableTransaction<SchemaKey, SchemaValue>,
    items: TableTransaction<ItemKey, ItemValue>,
    comments: TableTransaction<CommentKey, CommentValue>,
    roles: TableTransaction<RoleKey, RoleValue>,
    clusters: TableTransaction<ClusterKey, ClusterValue>,
    cluster_replicas: TableTransaction<ClusterReplicaKey, ClusterReplicaValue>,
    introspection_sources:
        TableTransaction<ClusterIntrospectionSourceIndexKey, ClusterIntrospectionSourceIndexValue>,
    id_allocator: TableTransaction<IdAllocKey, IdAllocValue>,
    configs: TableTransaction<ConfigKey, ConfigValue>,
    settings: TableTransaction<SettingKey, SettingValue>,
    timestamps: TableTransaction<TimestampKey, TimestampValue>,
    system_gid_mapping: TableTransaction<GidMappingKey, GidMappingValue>,
    system_configurations: TableTransaction<ServerConfigurationKey, ServerConfigurationValue>,
    default_privileges: TableTransaction<DefaultPrivilegesKey, DefaultPrivilegesValue>,
    system_privileges: TableTransaction<SystemPrivilegesKey, SystemPrivilegesValue>,
    // Don't make this a table transaction so that it's not read into the
    // in-memory cache.
    audit_log_updates: Vec<(proto::AuditLogKey, (), i64)>,
    storage_usage_updates: Vec<(proto::StorageUsageKey, (), i64)>,
    connection_timeout: Option<Duration>,
}

impl<'a> Transaction<'a> {
    pub fn new(
        durable_catalog: &'a mut dyn DurableCatalogState,
        Snapshot {
            databases,
            schemas,
            roles,
            items,
            comments,
            clusters,
            cluster_replicas,
            introspection_sources,
            id_allocator,
            configs,
            settings,
            timestamps,
            system_object_mappings,
            system_configurations,
            default_privileges,
            system_privileges,
        }: Snapshot,
    ) -> Result<Transaction, CatalogError> {
        Ok(Transaction {
            durable_catalog,
            databases: TableTransaction::new(databases, |a: &DatabaseValue, b| a.name == b.name)?,
            schemas: TableTransaction::new(schemas, |a: &SchemaValue, b| {
                a.database_id == b.database_id && a.name == b.name
            })?,
            items: TableTransaction::new(items, |a: &ItemValue, b| {
                a.schema_id == b.schema_id && a.name == b.name
            })?,
            comments: TableTransaction::new(comments, |_a, _b| false)?,
            roles: TableTransaction::new(roles, |a: &RoleValue, b| a.name == b.name)?,
            clusters: TableTransaction::new(clusters, |a: &ClusterValue, b| a.name == b.name)?,
            cluster_replicas: TableTransaction::new(
                cluster_replicas,
                |a: &ClusterReplicaValue, b| a.cluster_id == b.cluster_id && a.name == b.name,
            )?,
            introspection_sources: TableTransaction::new(introspection_sources, |_a, _b| false)?,
            id_allocator: TableTransaction::new(id_allocator, |_a, _b| false)?,
            configs: TableTransaction::new(configs, |_a, _b| false)?,
            settings: TableTransaction::new(settings, |_a, _b| false)?,
            timestamps: TableTransaction::new(timestamps, |_a, _b| false)?,
            system_gid_mapping: TableTransaction::new(system_object_mappings, |_a, _b| false)?,
            system_configurations: TableTransaction::new(system_configurations, |_a, _b| false)?,
            default_privileges: TableTransaction::new(default_privileges, |_a, _b| false)?,
            system_privileges: TableTransaction::new(system_privileges, |_a, _b| false)?,
            audit_log_updates: Vec::new(),
            storage_usage_updates: Vec::new(),
            connection_timeout: None,
        })
    }

    pub fn loaded_items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        self.items.for_values(|k, v| {
            items.push(Item::from_key_value(k.clone(), v.clone()));
        });
        items.sort_by_key(|Item { id, .. }| *id);
        items
    }

    pub fn insert_audit_log_event(&mut self, event: VersionedEvent) {
        self.audit_log_updates
            .push((AuditLogKey { event }.into_proto(), (), 1));
    }

    pub fn insert_storage_usage_event(&mut self, metric: VersionedStorageUsage) {
        self.storage_usage_updates
            .push((StorageUsageKey { metric }.into_proto(), (), 1));
    }

    pub fn insert_user_database(
        &mut self,
        database_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<DatabaseId, CatalogError> {
        let id = self.get_and_increment_id(DATABASE_ID_ALLOC_KEY.to_string())?;
        // TODO(parkertimmerman): Support creating databases in the System namespace.
        let id = DatabaseId::User(id);
        self.insert_database(id, database_name, owner_id, privileges)?;
        Ok(id)
    }

    pub(crate) fn insert_database(
        &mut self,
        id: DatabaseId,
        database_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<(), CatalogError> {
        match self.databases.insert(
            DatabaseKey { id },
            DatabaseValue {
                name: database_name.to_string(),
                owner_id,
                privileges,
            },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::DatabaseAlreadyExists(database_name.to_owned()).into()),
        }
    }

    pub fn insert_system_schema(
        &mut self,
        id: SchemaId,
        schema_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<SchemaId, CatalogError> {
        soft_assert!(id.is_system(), "ID {id:?} is not system variant");
        self.insert_schema(id, None, schema_name.to_string(), owner_id, privileges)?;
        Ok(id)
    }

    pub fn insert_user_schema(
        &mut self,
        database_id: DatabaseId,
        schema_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<SchemaId, CatalogError> {
        let id = self.get_and_increment_id(SCHEMA_ID_ALLOC_KEY.to_string())?;
        // TODO(parkertimmerman): Support creating schemas in the System namespace.
        let id = SchemaId::User(id);
        self.insert_schema(
            id,
            Some(database_id),
            schema_name.to_string(),
            owner_id,
            privileges,
        )?;
        Ok(id)
    }

    pub(crate) fn insert_schema(
        &mut self,
        schema_id: SchemaId,
        database_id: Option<DatabaseId>,
        schema_name: String,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<(), CatalogError> {
        match self.schemas.insert(
            SchemaKey { id: schema_id },
            SchemaValue {
                database_id,
                name: schema_name.clone(),
                owner_id,
                privileges,
            },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::SchemaAlreadyExists(schema_name).into()),
        }
    }

    pub fn insert_user_role(
        &mut self,
        name: String,
        attributes: RoleAttributes,
        membership: RoleMembership,
        vars: RoleVars,
    ) -> Result<RoleId, CatalogError> {
        let id = self.get_and_increment_id(USER_ROLE_ID_ALLOC_KEY.to_string())?;
        let id = RoleId::User(id);
        self.insert_role(id, name, attributes, membership, vars)?;
        Ok(id)
    }

    pub(crate) fn insert_role(
        &mut self,
        id: RoleId,
        name: String,
        attributes: RoleAttributes,
        membership: RoleMembership,
        vars: RoleVars,
    ) -> Result<(), CatalogError> {
        match self.roles.insert(
            RoleKey { id },
            RoleValue {
                name: name.clone(),
                attributes,
                membership,
                vars,
            },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::RoleAlreadyExists(name).into()),
        }
    }

    /// Panics if any introspection source id is not a system id
    pub fn insert_user_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        linked_object_id: Option<GlobalId>,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), CatalogError> {
        self.insert_cluster(
            cluster_id,
            cluster_name,
            linked_object_id,
            introspection_source_indexes,
            owner_id,
            privileges,
            config,
        )
    }

    /// Panics if any introspection source id is not a system id
    pub fn insert_system_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), CatalogError> {
        self.insert_cluster(
            cluster_id,
            cluster_name,
            None,
            introspection_source_indexes,
            MZ_SYSTEM_ROLE_ID,
            privileges,
            config,
        )
    }

    fn insert_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        linked_object_id: Option<GlobalId>,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), CatalogError> {
        if let Err(_) = self.clusters.insert(
            ClusterKey { id: cluster_id },
            ClusterValue {
                name: cluster_name.to_string(),
                linked_object_id,
                owner_id,
                privileges,
                config,
            },
        ) {
            return Err(SqlCatalogError::ClusterAlreadyExists(cluster_name.to_owned()).into());
        };

        for (builtin, index_id) in introspection_source_indexes {
            let introspection_source_index = IntrospectionSourceIndex {
                cluster_id,
                name: builtin.name.to_string(),
                index_id,
            };
            let (key, value) = introspection_source_index.into_key_value();
            self.introspection_sources
                .insert(key, value)
                .expect("no uniqueness violation");
        }

        Ok(())
    }

    pub fn rename_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        cluster_to_name: &str,
    ) -> Result<(), CatalogError> {
        let key = ClusterKey { id: cluster_id };

        match self.clusters.update(|k, v| {
            if *k == key {
                let mut value = v.clone();
                value.name = cluster_to_name.to_string();
                Some(value)
            } else {
                None
            }
        })? {
            0 => Err(SqlCatalogError::UnknownCluster(cluster_name.to_string()).into()),
            1 => Ok(()),
            n => panic!(
                "Expected to update single cluster {cluster_name} ({cluster_id}), updated {n}"
            ),
        }
    }

    pub fn check_migration_has_run(&mut self, name: String) -> Result<bool, CatalogError> {
        let key = SettingKey { name };
        // If the key does not exist, then the migration has not been run.
        let has_run = self.settings.get(&key).as_ref().is_some();

        Ok(has_run)
    }

    pub fn mark_migration_has_run(&mut self, name: String) -> Result<(), CatalogError> {
        let key = SettingKey { name };
        let val = SettingValue {
            value: true.to_string(),
        };
        self.settings.insert(key, val)?;

        Ok(())
    }

    pub fn rename_cluster_replica(
        &mut self,
        replica_id: ReplicaId,
        replica_name: &QualifiedReplica,
        replica_to_name: &str,
    ) -> Result<(), CatalogError> {
        let key = ClusterReplicaKey { id: replica_id };

        match self.cluster_replicas.update(|k, v| {
            if *k == key {
                let mut value = v.clone();
                value.name = replica_to_name.to_string();
                Some(value)
            } else {
                None
            }
        })? {
            0 => Err(SqlCatalogError::UnknownClusterReplica(replica_name.to_string()).into()),
            1 => Ok(()),
            n => panic!(
                "Expected to update single cluster replica {replica_name} ({replica_id}), updated {n}"
            ),
        }
    }

    pub fn insert_cluster_replica(
        &mut self,
        cluster_id: ClusterId,
        replica_id: ReplicaId,
        replica_name: &str,
        config: ReplicaConfig,
        owner_id: RoleId,
    ) -> Result<(), CatalogError> {
        if let Err(_) = self.cluster_replicas.insert(
            ClusterReplicaKey { id: replica_id },
            ClusterReplicaValue {
                cluster_id,
                name: replica_name.into(),
                config,
                owner_id,
            },
        ) {
            let cluster = self
                .clusters
                .get(&ClusterKey { id: cluster_id })
                .expect("cluster exists");
            return Err(SqlCatalogError::DuplicateReplica(
                replica_name.to_string(),
                cluster.name.to_string(),
            )
            .into());
        };
        Ok(())
    }

    /// Updates persisted information about persisted introspection source
    /// indexes.
    ///
    /// Panics if provided id is not a system id.
    pub fn update_introspection_source_index_gids(
        &mut self,
        mappings: impl Iterator<Item = (ClusterId, impl Iterator<Item = (String, GlobalId)>)>,
    ) -> Result<(), CatalogError> {
        for (cluster_id, updates) in mappings {
            for (name, index_id) in updates {
                let introspection_source_index = IntrospectionSourceIndex {
                    cluster_id,
                    name,
                    index_id,
                };
                let (key, value) = introspection_source_index.into_key_value();

                let prev = self.introspection_sources.set(key, Some(value))?;
                if prev.is_none() {
                    return Err(SqlCatalogError::FailedBuiltinSchemaMigration(format!(
                        "{index_id}"
                    ))
                    .into());
                }
            }
        }
        Ok(())
    }

    pub fn insert_item(
        &mut self,
        id: GlobalId,
        schema_id: SchemaId,
        item_name: &str,
        create_sql: String,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<(), CatalogError> {
        match self.items.insert(
            ItemKey { gid: id },
            ItemValue {
                schema_id,
                name: item_name.to_string(),
                create_sql,
                owner_id,
                privileges,
            },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::ItemAlreadyExists(id, item_name.to_owned()).into()),
        }
    }

    pub fn insert_timestamp(
        &mut self,
        timeline: Timeline,
        ts: mz_repr::Timestamp,
    ) -> Result<(), CatalogError> {
        match self.timestamps.insert(
            TimestampKey {
                id: timeline.to_string(),
            },
            TimestampValue { ts },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::TimelineAlreadyExists(timeline.to_string()).into()),
        }
    }

    pub fn get_and_increment_id(&mut self, key: String) -> Result<u64, CatalogError> {
        Ok(self.get_and_increment_id_by(key, 1)?.into_element())
    }

    pub fn get_and_increment_id_by(
        &mut self,
        key: String,
        amount: u64,
    ) -> Result<Vec<u64>, CatalogError> {
        let current_id = self
            .id_allocator
            .items()
            .get(&IdAllocKey { name: key.clone() })
            .unwrap_or_else(|| panic!("{key} id allocator missing"))
            .next_id;
        let next_id = current_id
            .checked_add(amount)
            .ok_or(SqlCatalogError::IdExhaustion)?;
        let prev = self
            .id_allocator
            .set(IdAllocKey { name: key }, Some(IdAllocValue { next_id }))?;
        assert_eq!(
            prev,
            Some(IdAllocValue {
                next_id: current_id
            })
        );
        Ok((current_id..next_id).collect())
    }

    pub fn allocate_system_item_ids(&mut self, amount: u64) -> Result<Vec<GlobalId>, CatalogError> {
        Ok(self
            .get_and_increment_id_by(SYSTEM_ITEM_ALLOC_KEY.to_string(), amount)?
            .into_iter()
            .map(GlobalId::System)
            .collect())
    }

    pub fn allocate_user_item_ids(&mut self, amount: u64) -> Result<Vec<GlobalId>, CatalogError> {
        Ok(self
            .get_and_increment_id_by(USER_ITEM_ALLOC_KEY.to_string(), amount)?
            .into_iter()
            .map(GlobalId::User)
            .collect())
    }

    pub(crate) fn insert_id_allocator(
        &mut self,
        name: String,
        next_id: u64,
    ) -> Result<(), CatalogError> {
        match self
            .id_allocator
            .insert(IdAllocKey { name: name.clone() }, IdAllocValue { next_id })
        {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::IdAllocatorAlreadyExists(name).into()),
        }
    }

    pub fn remove_database(&mut self, id: &DatabaseId) -> Result<(), CatalogError> {
        let prev = self.databases.set(DatabaseKey { id: *id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownDatabase(id.to_string()).into())
        }
    }

    pub fn remove_schema(
        &mut self,
        database_id: &Option<DatabaseId>,
        schema_id: &SchemaId,
    ) -> Result<(), CatalogError> {
        let prev = self.schemas.set(SchemaKey { id: *schema_id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            let database_name = match database_id {
                Some(id) => format!("{id}."),
                None => "".to_string(),
            };
            Err(SqlCatalogError::UnknownSchema(format!("{}.{}", database_name, schema_id)).into())
        }
    }

    pub fn remove_role(&mut self, name: &str) -> Result<(), CatalogError> {
        let roles = self.roles.delete(|_k, v| v.name == name);
        assert!(
            roles.iter().all(|(k, _)| k.id.is_user()),
            "cannot delete non-user roles"
        );
        let n = roles.len();
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownRole(name.to_owned()).into())
        }
    }

    pub fn remove_cluster(&mut self, id: ClusterId) -> Result<(), CatalogError> {
        let deleted = self.clusters.delete(|k, _v| k.id == id);
        if deleted.is_empty() {
            Err(SqlCatalogError::UnknownCluster(id.to_string()).into())
        } else {
            assert_eq!(deleted.len(), 1);
            // Cascade delete introspection sources and cluster replicas.
            //
            // TODO(benesch): this doesn't seem right. Cascade deletions should
            // be entirely the domain of the higher catalog layer, not the
            // storage layer.
            self.cluster_replicas.delete(|_k, v| v.cluster_id == id);
            self.introspection_sources
                .delete(|k, _v| k.cluster_id == id);
            Ok(())
        }
    }

    pub fn remove_cluster_replica(&mut self, id: ReplicaId) -> Result<(), CatalogError> {
        let deleted = self.cluster_replicas.delete(|k, _v| k.id == id);
        if deleted.len() == 1 {
            Ok(())
        } else {
            assert!(deleted.is_empty());
            Err(SqlCatalogError::UnknownClusterReplica(id.to_string()).into())
        }
    }

    /// Removes all storage usage events in `events` from the transaction.
    pub(crate) fn remove_storage_usage_events(&mut self, events: Vec<VersionedStorageUsage>) {
        let events = events
            .into_iter()
            .map(|event| (StorageUsageKey { metric: event }.into_proto(), (), -1));
        self.storage_usage_updates.extend(events);
    }

    /// Removes item `id` from the transaction.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the catalog.
    /// DO NOT call this function in a loop, use [`Self::remove_items`] instead.
    pub fn remove_item(&mut self, id: GlobalId) -> Result<(), CatalogError> {
        let prev = self.items.set(ItemKey { gid: id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Removes all items in `ids` from the transaction.
    ///
    /// Returns an error if any id in `ids` is not found.
    ///
    /// NOTE: On error, there still may be some items removed from the transaction. It is
    /// up to the called to either abort the transaction or commit.
    pub fn remove_items(&mut self, ids: BTreeSet<GlobalId>) -> Result<(), CatalogError> {
        let n = self.items.delete(|k, _v| ids.contains(&k.gid)).len();
        if n == ids.len() {
            Ok(())
        } else {
            let item_gids = self.items.items().keys().map(|k| k.gid).collect();
            let mut unknown = ids.difference(&item_gids);
            Err(SqlCatalogError::UnknownItem(unknown.join(", ")).into())
        }
    }

    /// Updates item `id` in the transaction to `item_name` and `item`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the catalog.
    /// DO NOT call this function in a loop, use [`Self::update_items`] instead.
    pub fn update_item(&mut self, id: GlobalId, item: Item) -> Result<(), CatalogError> {
        let n = self.items.update(|k, v| {
            if k.gid == id {
                let item = item.clone();
                // Schema IDs cannot change.
                assert_eq!(item.schema_id, v.schema_id);
                let (_, new_value) = item.into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Updates all items with ids matching the keys of `items` in the transaction, to the
    /// corresponding value in `items`.
    ///
    /// Returns an error if any id in `items` is not found.
    ///
    /// NOTE: On error, there still may be some items updated in the transaction. It is
    /// up to the called to either abort the transaction or commit.
    pub fn update_items(&mut self, items: BTreeMap<GlobalId, Item>) -> Result<(), CatalogError> {
        let n = self.items.update(|k, v| {
            if let Some(item) = items.get(&k.gid) {
                // Schema IDs cannot change.
                assert_eq!(item.schema_id, v.schema_id);
                let (_, new_value) = item.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        let n = usize::try_from(n).expect("Must be positive and fit in usize");
        if n == items.len() {
            Ok(())
        } else {
            let update_ids: BTreeSet<_> = items.into_keys().collect();
            let item_ids: BTreeSet<_> = self.items.items().keys().map(|k| k.gid).collect();
            let mut unknown = update_ids.difference(&item_ids);
            Err(SqlCatalogError::UnknownItem(unknown.join(", ")).into())
        }
    }

    /// Updates role `id` in the transaction to `role`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the catalog.
    /// DO NOT call this function in a loop, implement and use some `Self::update_roles` instead.
    /// You should model it after [`Self::update_items`].
    pub fn update_role(&mut self, id: RoleId, role: Role) -> Result<(), CatalogError> {
        let n = self.roles.update(move |k, _v| {
            if k.id == id {
                let role = role.clone();
                let (_, new_value) = role.into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Updates persisted mapping from system objects to global IDs and fingerprints. Each element
    /// of `mappings` should be (old-global-id, new-system-object-mapping).
    ///
    /// Panics if provided id is not a system id.
    pub fn update_system_object_mappings(
        &mut self,
        mappings: BTreeMap<GlobalId, SystemObjectMapping>,
    ) -> Result<(), CatalogError> {
        let n = self.system_gid_mapping.update(|_k, v| {
            if let Some(mapping) = mappings.get(&GlobalId::System(v.id)) {
                let (_, new_value) = mapping.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;

        if usize::try_from(n).expect("update diff should fit into usize") != mappings.len() {
            let id_str = mappings.keys().map(|id| id.to_string()).join(",");
            return Err(SqlCatalogError::FailedBuiltinSchemaMigration(id_str).into());
        }

        Ok(())
    }

    /// Updates cluster `id` in the transaction to `cluster`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of clusters in the catalog.
    /// DO NOT call this function in a loop.
    pub fn update_cluster(&mut self, id: ClusterId, cluster: Cluster) -> Result<(), CatalogError> {
        let n = self.clusters.update(|k, _v| {
            if k.id == id {
                let (_, new_value) = cluster.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownCluster(id.to_string()).into())
        }
    }

    /// Updates cluster replica `replica_id` in the transaction to `replica`.
    ///
    /// Returns an error if `replica_id` is not found.
    ///
    /// Runtime is linear with respect to the total number of cluster replicas in the catalog.
    /// DO NOT call this function in a loop.
    pub fn update_cluster_replica(
        &mut self,
        replica_id: ReplicaId,
        replica: ClusterReplica,
    ) -> Result<(), CatalogError> {
        let n = self.cluster_replicas.update(|k, _v| {
            if k.id == replica_id {
                let (_, new_value) = replica.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownClusterReplica(replica_id.to_string()).into())
        }
    }

    /// Updates database `id` in the transaction to `database`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of databases in the catalog.
    /// DO NOT call this function in a loop.
    pub fn update_database(
        &mut self,
        id: DatabaseId,
        database: Database,
    ) -> Result<(), CatalogError> {
        let n = self.databases.update(|k, _v| {
            if id == k.id {
                let (_, new_value) = database.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownDatabase(id.to_string()).into())
        }
    }

    /// Updates schema `schema_id` in the transaction to `schema`.
    ///
    /// Returns an error if `schema_id` is not found.
    ///
    /// Runtime is linear with respect to the total number of schemas in the catalog.
    /// DO NOT call this function in a loop.
    pub fn update_schema(
        &mut self,
        schema_id: SchemaId,
        schema: Schema,
    ) -> Result<(), CatalogError> {
        let n = self.schemas.update(|k, _v| {
            if schema_id == k.id {
                let schema = schema.clone();
                let (_, new_value) = schema.clone().into_key_value();
                Some(new_value)
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownSchema(schema_id.to_string()).into())
        }
    }

    /// Set persisted default privilege.
    ///
    /// DO NOT call this function in a loop, use [`Self::set_default_privileges`] instead.
    pub fn set_default_privilege(
        &mut self,
        role_id: RoleId,
        database_id: Option<DatabaseId>,
        schema_id: Option<SchemaId>,
        object_type: ObjectType,
        grantee: RoleId,
        privileges: Option<AclMode>,
    ) -> Result<(), CatalogError> {
        self.default_privileges.set(
            DefaultPrivilegesKey {
                role_id,
                database_id,
                schema_id,
                object_type,
                grantee,
            },
            privileges.map(|privileges| DefaultPrivilegesValue { privileges }),
        )?;
        Ok(())
    }

    /// Set persisted default privileges.
    pub fn set_default_privileges(
        &mut self,
        default_privileges: Vec<DefaultPrivilege>,
    ) -> Result<(), CatalogError> {
        let default_privileges = default_privileges
            .into_iter()
            .map(DurableType::into_key_value)
            .map(|(k, v)| (k, Some(v)))
            .collect();
        self.default_privileges.set_many(default_privileges)?;
        Ok(())
    }

    /// Set persisted system privilege.
    ///
    /// DO NOT call this function in a loop, use [`Self::set_system_privileges`] instead.
    pub fn set_system_privilege(
        &mut self,
        grantee: RoleId,
        grantor: RoleId,
        acl_mode: Option<AclMode>,
    ) -> Result<(), CatalogError> {
        self.system_privileges.set(
            SystemPrivilegesKey { grantee, grantor },
            acl_mode.map(|acl_mode| SystemPrivilegesValue { acl_mode }),
        )?;
        Ok(())
    }

    /// Set persisted system privileges.
    pub fn set_system_privileges(
        &mut self,
        system_privileges: Vec<MzAclItem>,
    ) -> Result<(), CatalogError> {
        let system_privileges = system_privileges
            .into_iter()
            .map(DurableType::into_key_value)
            .map(|(k, v)| (k, Some(v)))
            .collect();
        self.system_privileges.set_many(system_privileges)?;
        Ok(())
    }

    /// Set persisted setting.
    pub(crate) fn set_setting(
        &mut self,
        name: String,
        value: Option<String>,
    ) -> Result<(), CatalogError> {
        self.settings.set(
            SettingKey { name },
            value.map(|value| SettingValue { value }),
        )?;
        Ok(())
    }

    pub fn set_catalog_content_version(&mut self, version: String) -> Result<(), CatalogError> {
        self.set_setting(CATALOG_CONTENT_VERSION_KEY.to_string(), Some(version))
    }

    /// Set persisted introspection source index.
    pub fn set_introspection_source_indexes(
        &mut self,
        introspection_source_indexes: Vec<IntrospectionSourceIndex>,
    ) -> Result<(), CatalogError> {
        let introspection_source_indexes = introspection_source_indexes
            .into_iter()
            .map(DurableType::into_key_value)
            .map(|(k, v)| (k, Some(v)))
            .collect();
        self.introspection_sources
            .set_many(introspection_source_indexes)?;
        Ok(())
    }

    /// Set persisted system object mappings.
    pub fn set_system_object_mappings(
        &mut self,
        mappings: Vec<SystemObjectMapping>,
    ) -> Result<(), CatalogError> {
        let mappings = mappings
            .into_iter()
            .map(DurableType::into_key_value)
            .map(|(k, v)| (k, Some(v)))
            .collect();
        self.system_gid_mapping.set_many(mappings)?;
        Ok(())
    }

    /// Set persisted timestamp.
    pub fn set_timestamp(
        &mut self,
        timeline: Timeline,
        ts: mz_repr::Timestamp,
    ) -> Result<(), CatalogError> {
        let timeline_timestamp = TimelineTimestamp { timeline, ts };
        let (key, value) = timeline_timestamp.into_key_value();
        self.timestamps.set(key, Some(value))?;
        Ok(())
    }

    /// Set persisted replica.
    pub fn set_replicas(&mut self, replicas: Vec<ClusterReplica>) -> Result<(), CatalogError> {
        let replicas = replicas
            .into_iter()
            .map(DurableType::into_key_value)
            .map(|(k, v)| (k, Some(v)))
            .collect();
        self.cluster_replicas.set_many(replicas)?;
        Ok(())
    }

    /// Set persisted configuration.
    pub(crate) fn set_config(&mut self, key: String, value: u64) -> Result<(), CatalogError> {
        let config = Config { key, value };
        let (key, value) = config.into_key_value();
        self.configs.set(key, Some(value))?;
        Ok(())
    }

    /// Updates the catalog `persist_txn_tables` "config" value to
    /// match the `persist_txn_tables` "system var" value.
    ///
    /// These are mirrored so that we can toggle the flag with Launch Darkly,
    /// but use it in boot before Launch Darkly is available.
    pub fn set_persist_txn_tables(
        &mut self,
        value: PersistTxnTablesImpl,
    ) -> Result<(), CatalogError> {
        self.set_config(PERSIST_TXN_TABLES.into(), u64::from(value))?;
        Ok(())
    }

    /// Updates the catalog `system_config_synced` "config" value to true.
    pub fn set_system_config_synced_once(&mut self) -> Result<(), CatalogError> {
        self.set_config(SYSTEM_CONFIG_SYNCED_KEY.into(), 1)
    }

    pub fn update_comment(
        &mut self,
        object_id: CommentObjectId,
        sub_component: Option<usize>,
        comment: Option<String>,
    ) -> Result<(), CatalogError> {
        let key = CommentKey {
            object_id,
            sub_component,
        };
        let value = comment.map(|c| CommentValue { comment: c });
        self.comments.set(key, value)?;

        Ok(())
    }

    pub fn drop_comments(
        &mut self,
        object_id: CommentObjectId,
    ) -> Result<Vec<(CommentObjectId, Option<usize>, String)>, CatalogError> {
        let deleted = self.comments.delete(|k, _v| k.object_id == object_id);
        let deleted = deleted
            .into_iter()
            .map(|(k, v)| (k.object_id, k.sub_component, v.comment))
            .collect();
        Ok(deleted)
    }

    /// Upserts persisted system configuration `name` to `value`.
    pub fn upsert_system_config(&mut self, name: &str, value: String) -> Result<(), CatalogError> {
        let key = ServerConfigurationKey {
            name: name.to_string(),
        };
        let value = ServerConfigurationValue { value };
        self.system_configurations.set(key, Some(value))?;
        Ok(())
    }

    /// Removes persisted system configuration `name`.
    pub fn remove_system_config(&mut self, name: &str) {
        let key = ServerConfigurationKey {
            name: name.to_string(),
        };
        self.system_configurations
            .set(key, None)
            .expect("cannot have uniqueness violation");
    }

    /// Removes all persisted system configurations.
    pub fn clear_system_configs(&mut self) {
        self.system_configurations.delete(|_k, _v| true);
    }

    pub(crate) fn insert_config(&mut self, key: String, value: u64) -> Result<(), CatalogError> {
        match self
            .configs
            .insert(ConfigKey { key: key.clone() }, ConfigValue { value })
        {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::ConfigAlreadyExists(key).into()),
        }
    }

    pub fn get_clusters(&self) -> impl Iterator<Item = Cluster> {
        self.clusters
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_cluster_replicas(&self) -> impl Iterator<Item = ClusterReplica> {
        self.cluster_replicas
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_databases(&self) -> impl Iterator<Item = Database> {
        self.databases
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_schemas(&self) -> impl Iterator<Item = Schema> {
        self.schemas
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_roles(&self) -> impl Iterator<Item = Role> {
        self.roles
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_default_privileges(&self) -> impl Iterator<Item = DefaultPrivilege> {
        self.default_privileges
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_system_privileges(&self) -> impl Iterator<Item = MzAclItem> {
        self.system_privileges
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_comments(&self) -> impl Iterator<Item = Comment> {
        self.comments
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_system_configurations(&self) -> impl Iterator<Item = SystemConfiguration> {
        self.system_configurations
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_system_items(&self) -> impl Iterator<Item = SystemObjectMapping> {
        self.system_gid_mapping
            .items()
            .clone()
            .into_iter()
            .map(|(k, v)| DurableType::from_key_value(k, v))
    }

    pub fn get_timestamp(&self, timeline: &Timeline) -> Option<mz_repr::Timestamp> {
        self.timestamps
            .get(&TimestampKey {
                id: timeline.to_string(),
            })
            .map(|value| value.ts)
    }

    pub fn get_introspection_source_indexes(
        &mut self,
        cluster_id: ClusterId,
    ) -> BTreeMap<String, GlobalId> {
        self.introspection_sources
            .items()
            .into_iter()
            .filter(|(k, _v)| k.cluster_id == cluster_id)
            .map(|(k, v)| (k.name, GlobalId::System(v.index_id)))
            .collect()
    }

    pub fn get_catalog_content_version(&self) -> Option<String> {
        self.settings
            .get(&SettingKey {
                name: CATALOG_CONTENT_VERSION_KEY.to_string(),
            })
            .map(|value| value.value.clone())
    }

    pub fn set_connection_timeout(&mut self, timeout: Duration) {
        self.connection_timeout = Some(timeout);
    }

    pub(crate) fn into_parts(self) -> (TransactionBatch, &'a mut dyn DurableCatalogState) {
        let txn_batch = TransactionBatch {
            databases: self.databases.pending(),
            schemas: self.schemas.pending(),
            items: self.items.pending(),
            comments: self.comments.pending(),
            roles: self.roles.pending(),
            clusters: self.clusters.pending(),
            cluster_replicas: self.cluster_replicas.pending(),
            introspection_sources: self.introspection_sources.pending(),
            id_allocator: self.id_allocator.pending(),
            configs: self.configs.pending(),
            settings: self.settings.pending(),
            timestamps: self.timestamps.pending(),
            system_gid_mapping: self.system_gid_mapping.pending(),
            system_configurations: self.system_configurations.pending(),
            default_privileges: self.default_privileges.pending(),
            system_privileges: self.system_privileges.pending(),
            audit_log_updates: self.audit_log_updates,
            storage_usage_updates: self.storage_usage_updates,
            connection_timeout: self.connection_timeout,
        };
        (txn_batch, self.durable_catalog)
    }

    /// Commits the storage transaction to durable storage. Any error returned indicates the catalog may be
    /// in an indeterminate state and needs to be fully re-read before proceeding. In general, this
    /// must be fatal to the calling process. We do not panic/halt inside this function itself so
    /// that errors can bubble up during initialization.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn commit(self) -> Result<(), CatalogError> {
        let (txn_batch, durable_catalog) = self.into_parts();
        durable_catalog.commit_transaction(txn_batch).await
    }
}

/// Describes a set of changes to apply as the result of a catalog transaction.
#[derive(Debug, Clone)]
pub struct TransactionBatch {
    pub(crate) databases: Vec<(proto::DatabaseKey, proto::DatabaseValue, Diff)>,
    pub(crate) schemas: Vec<(proto::SchemaKey, proto::SchemaValue, Diff)>,
    pub(crate) items: Vec<(proto::ItemKey, proto::ItemValue, Diff)>,
    pub(crate) comments: Vec<(proto::CommentKey, proto::CommentValue, Diff)>,
    pub(crate) roles: Vec<(proto::RoleKey, proto::RoleValue, Diff)>,
    pub(crate) clusters: Vec<(proto::ClusterKey, proto::ClusterValue, Diff)>,
    pub(crate) cluster_replicas: Vec<(proto::ClusterReplicaKey, proto::ClusterReplicaValue, Diff)>,
    pub(crate) introspection_sources: Vec<(
        proto::ClusterIntrospectionSourceIndexKey,
        proto::ClusterIntrospectionSourceIndexValue,
        Diff,
    )>,
    pub(crate) id_allocator: Vec<(proto::IdAllocKey, proto::IdAllocValue, Diff)>,
    pub(crate) configs: Vec<(proto::ConfigKey, proto::ConfigValue, Diff)>,
    pub(crate) settings: Vec<(proto::SettingKey, proto::SettingValue, Diff)>,
    pub(crate) timestamps: Vec<(proto::TimestampKey, proto::TimestampValue, Diff)>,
    pub(crate) system_gid_mapping: Vec<(proto::GidMappingKey, proto::GidMappingValue, Diff)>,
    pub(crate) system_configurations: Vec<(
        proto::ServerConfigurationKey,
        proto::ServerConfigurationValue,
        Diff,
    )>,
    pub(crate) default_privileges: Vec<(
        proto::DefaultPrivilegesKey,
        proto::DefaultPrivilegesValue,
        Diff,
    )>,
    pub(crate) system_privileges: Vec<(
        proto::SystemPrivilegesKey,
        proto::SystemPrivilegesValue,
        Diff,
    )>,
    pub(crate) audit_log_updates: Vec<(proto::AuditLogKey, (), Diff)>,
    pub(crate) storage_usage_updates: Vec<(proto::StorageUsageKey, (), Diff)>,
    pub(crate) connection_timeout: Option<Duration>,
}

impl TransactionBatch {
    pub fn is_empty(&self) -> bool {
        let TransactionBatch {
            databases,
            schemas,
            items,
            comments,
            roles,
            clusters,
            cluster_replicas,
            introspection_sources,
            id_allocator,
            configs,
            settings,
            timestamps,
            system_gid_mapping,
            system_configurations,
            default_privileges,
            system_privileges,
            audit_log_updates,
            storage_usage_updates,
            // This doesn't get written down anywhere.
            connection_timeout: _,
        } = self;

        databases.is_empty()
            && schemas.is_empty()
            && items.is_empty()
            && comments.is_empty()
            && roles.is_empty()
            && clusters.is_empty()
            && cluster_replicas.is_empty()
            && introspection_sources.is_empty()
            && id_allocator.is_empty()
            && configs.is_empty()
            && settings.is_empty()
            && timestamps.is_empty()
            && system_gid_mapping.is_empty()
            && system_configurations.is_empty()
            && default_privileges.is_empty()
            && system_privileges.is_empty()
            && audit_log_updates.is_empty()
            && storage_usage_updates.is_empty()
    }
}
