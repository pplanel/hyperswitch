use common_utils::ext_traits::Encode;
use data_models::{
    errors::StorageError,
    payouts::payouts::{Payouts, PayoutsInterface, PayoutsNew, PayoutsUpdate},
};
use diesel_models::{
    enums::MerchantStorageScheme,
    kv,
    payouts::{
        Payouts as DieselPayouts, PayoutsNew as DieselPayoutsNew,
        PayoutsUpdate as DieselPayoutsUpdate,
    },
};
use error_stack::{IntoReport, ResultExt};
use redis_interface::HsetnxReply;
use router_env::{instrument, tracing};

use crate::{
    diesel_error_to_data_error,
    errors::RedisErrorExt,
    redis::kv_store::{kv_wrapper, KvOperation},
    utils::{self, pg_connection_read, pg_connection_write},
    DataModelExt, DatabaseStore, KVRouterStore,
};

#[async_trait::async_trait]
impl<T: DatabaseStore> PayoutsInterface for KVRouterStore<T> {
    #[instrument(skip_all)]
    async fn insert_payout(
        &self,
        new: PayoutsNew,
        storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        match storage_scheme {
            MerchantStorageScheme::PostgresOnly => {
                self.router_store.insert_payout(new, storage_scheme).await
            }
            MerchantStorageScheme::RedisKv => {
                let key = format!("mid_{}_po_{}", new.merchant_id, new.payout_id);
                let field = format!("po_{}", new.payout_id);
                let now = common_utils::date_time::now();
                let created_payout = Payouts {
                    payout_id: new.payout_id.clone(),
                    merchant_id: new.merchant_id.clone(),
                    customer_id: new.customer_id.clone(),
                    address_id: new.address_id.clone(),
                    payout_type: new.payout_type,
                    payout_method_id: new.payout_method_id.clone(),
                    amount: new.amount,
                    destination_currency: new.destination_currency,
                    source_currency: new.source_currency,
                    description: new.description.clone(),
                    recurring: new.recurring,
                    auto_fulfill: new.auto_fulfill,
                    return_url: new.return_url.clone(),
                    entity_type: new.entity_type,
                    metadata: new.metadata.clone(),
                    created_at: new.created_at.unwrap_or(now),
                    last_modified_at: new.last_modified_at.unwrap_or(now),
                    profile_id: new.profile_id.clone(),
                    status: new.status,
                    attempt_count: new.attempt_count,
                };

                let redis_entry = kv::TypedSql {
                    op: kv::DBOperation::Insert {
                        insertable: kv::Insertable::Payouts(new.to_storage_model()),
                    },
                };

                match kv_wrapper::<DieselPayouts, _, _>(
                    self,
                    KvOperation::<DieselPayouts>::HSetNx(
                        &field,
                        &created_payout.clone().to_storage_model(),
                        redis_entry,
                    ),
                    &key,
                )
                .await
                .map_err(|err| err.to_redis_failed_response(&key))?
                .try_into_hsetnx()
                {
                    Ok(HsetnxReply::KeyNotSet) => Err(StorageError::DuplicateValue {
                        entity: "payouts",
                        key: Some(key),
                    })
                    .into_report(),
                    Ok(HsetnxReply::KeySet) => Ok(created_payout),
                    Err(error) => Err(error.change_context(StorageError::KVError)),
                }
            }
        }
    }

    #[instrument(skip_all)]
    async fn update_payout(
        &self,
        this: &Payouts,
        payout_update: PayoutsUpdate,
        storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        match storage_scheme {
            MerchantStorageScheme::PostgresOnly => {
                self.router_store
                    .update_payout(this, payout_update, storage_scheme)
                    .await
            }
            MerchantStorageScheme::RedisKv => {
                let key = format!("mid_{}_po_{}", this.merchant_id, this.payout_id);
                let field = format!("po_{}", this.payout_id);

                let diesel_payout_update = payout_update.to_storage_model();
                let origin_diesel_payout = this.clone().to_storage_model();

                let diesel_payout = diesel_payout_update
                    .clone()
                    .apply_changeset(origin_diesel_payout.clone());
                // Check for database presence as well Maybe use a read replica here ?

                let redis_value = diesel_payout
                    .encode_to_string_of_json()
                    .change_context(StorageError::SerializationFailed)?;

                let redis_entry = kv::TypedSql {
                    op: kv::DBOperation::Update {
                        updatable: kv::Updateable::PayoutsUpdate(kv::PayoutsUpdateMems {
                            orig: origin_diesel_payout,
                            update_data: diesel_payout_update,
                        }),
                    },
                };

                kv_wrapper::<(), _, _>(
                    self,
                    KvOperation::<DieselPayouts>::Hset((&field, redis_value), redis_entry),
                    &key,
                )
                .await
                .map_err(|err| err.to_redis_failed_response(&key))?
                .try_into_hset()
                .change_context(StorageError::KVError)?;

                Ok(Payouts::from_storage_model(diesel_payout))
            }
        }
    }

    #[instrument(skip_all)]
    async fn find_payout_by_merchant_id_payout_id(
        &self,
        merchant_id: &str,
        payout_id: &str,
        storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        let database_call = || async {
            let conn = pg_connection_read(self).await?;
            DieselPayouts::find_by_merchant_id_payout_id(&conn, merchant_id, payout_id)
                .await
                .map_err(|er| {
                    let new_err = diesel_error_to_data_error(er.current_context());
                    er.change_context(new_err)
                })
        };
        match storage_scheme {
            MerchantStorageScheme::PostgresOnly => database_call().await,
            MerchantStorageScheme::RedisKv => {
                let key = format!("mid_{merchant_id}_po_{payout_id}");
                let field = format!("po_{payout_id}");
                Box::pin(utils::try_redis_get_else_try_database_get(
                    async {
                        kv_wrapper::<DieselPayouts, _, _>(
                            self,
                            KvOperation::<DieselPayouts>::HGet(&field),
                            &key,
                        )
                        .await?
                        .try_into_hget()
                    },
                    database_call,
                ))
                .await
            }
        }
        .map(Payouts::from_storage_model)
    }

    #[instrument(skip_all)]
    async fn find_optional_payout_by_merchant_id_payout_id(
        &self,
        merchant_id: &str,
        payout_id: &str,
        storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Option<Payouts>, StorageError> {
        let database_call = || async {
            let conn = pg_connection_read(self).await?;
            DieselPayouts::find_optional_by_merchant_id_payout_id(&conn, merchant_id, payout_id)
                .await
                .map_err(|er| {
                    let new_err = diesel_error_to_data_error(er.current_context());
                    er.change_context(new_err)
                })
        };
        match storage_scheme {
            MerchantStorageScheme::PostgresOnly => {
                let maybe_payouts = database_call().await?;
                Ok(maybe_payouts.and_then(|payout| {
                    if payout.payout_id == payout_id {
                        Some(payout)
                    } else {
                        None
                    }
                }))
            }
            MerchantStorageScheme::RedisKv => {
                let key = format!("mid_{merchant_id}_po_{payout_id}");
                let field = format!("po_{payout_id}");
                Box::pin(utils::try_redis_get_else_try_database_get(
                    async {
                        kv_wrapper::<DieselPayouts, _, _>(
                            self,
                            KvOperation::<DieselPayouts>::HGet(&field),
                            &key,
                        )
                        .await?
                        .try_into_hget()
                        .map(Some)
                    },
                    database_call,
                ))
                .await
            }
        }
        .map(|payout| payout.map(Payouts::from_storage_model))
    }
}

#[async_trait::async_trait]
impl<T: DatabaseStore> PayoutsInterface for crate::RouterStore<T> {
    #[instrument(skip_all)]
    async fn insert_payout(
        &self,
        new: PayoutsNew,
        _storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        let conn = pg_connection_write(self).await?;
        new.to_storage_model()
            .insert(&conn)
            .await
            .map_err(|er| {
                let new_err = diesel_error_to_data_error(er.current_context());
                er.change_context(new_err)
            })
            .map(Payouts::from_storage_model)
    }

    #[instrument(skip_all)]
    async fn update_payout(
        &self,
        this: &Payouts,
        payout: PayoutsUpdate,
        _storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        let conn = pg_connection_write(self).await?;
        this.clone()
            .to_storage_model()
            .update(&conn, payout.to_storage_model())
            .await
            .map_err(|er| {
                let new_err = diesel_error_to_data_error(er.current_context());
                er.change_context(new_err)
            })
            .map(Payouts::from_storage_model)
    }

    #[instrument(skip_all)]
    async fn find_payout_by_merchant_id_payout_id(
        &self,
        merchant_id: &str,
        payout_id: &str,
        _storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Payouts, StorageError> {
        let conn = pg_connection_read(self).await?;
        DieselPayouts::find_by_merchant_id_payout_id(&conn, merchant_id, payout_id)
            .await
            .map(Payouts::from_storage_model)
            .map_err(|er| {
                let new_err = diesel_error_to_data_error(er.current_context());
                er.change_context(new_err)
            })
    }

    #[instrument(skip_all)]
    async fn find_optional_payout_by_merchant_id_payout_id(
        &self,
        merchant_id: &str,
        payout_id: &str,
        _storage_scheme: MerchantStorageScheme,
    ) -> error_stack::Result<Option<Payouts>, StorageError> {
        let conn = pg_connection_read(self).await?;
        DieselPayouts::find_optional_by_merchant_id_payout_id(&conn, merchant_id, payout_id)
            .await
            .map(|x| x.map(Payouts::from_storage_model))
            .map_err(|er| {
                let new_err = diesel_error_to_data_error(er.current_context());
                er.change_context(new_err)
            })
    }
}

impl DataModelExt for Payouts {
    type StorageModel = DieselPayouts;

    fn to_storage_model(self) -> Self::StorageModel {
        DieselPayouts {
            payout_id: self.payout_id,
            merchant_id: self.merchant_id,
            customer_id: self.customer_id,
            address_id: self.address_id,
            payout_type: self.payout_type,
            payout_method_id: self.payout_method_id,
            amount: self.amount,
            destination_currency: self.destination_currency,
            source_currency: self.source_currency,
            description: self.description,
            recurring: self.recurring,
            auto_fulfill: self.auto_fulfill,
            return_url: self.return_url,
            entity_type: self.entity_type,
            metadata: self.metadata,
            created_at: self.created_at,
            last_modified_at: self.last_modified_at,
            profile_id: self.profile_id,
            status: self.status,
            attempt_count: self.attempt_count,
        }
    }

    fn from_storage_model(storage_model: Self::StorageModel) -> Self {
        Self {
            payout_id: storage_model.payout_id,
            merchant_id: storage_model.merchant_id,
            customer_id: storage_model.customer_id,
            address_id: storage_model.address_id,
            payout_type: storage_model.payout_type,
            payout_method_id: storage_model.payout_method_id,
            amount: storage_model.amount,
            destination_currency: storage_model.destination_currency,
            source_currency: storage_model.source_currency,
            description: storage_model.description,
            recurring: storage_model.recurring,
            auto_fulfill: storage_model.auto_fulfill,
            return_url: storage_model.return_url,
            entity_type: storage_model.entity_type,
            metadata: storage_model.metadata,
            created_at: storage_model.created_at,
            last_modified_at: storage_model.last_modified_at,
            profile_id: storage_model.profile_id,
            status: storage_model.status,
            attempt_count: storage_model.attempt_count,
        }
    }
}
impl DataModelExt for PayoutsNew {
    type StorageModel = DieselPayoutsNew;

    fn to_storage_model(self) -> Self::StorageModel {
        DieselPayoutsNew {
            payout_id: self.payout_id,
            merchant_id: self.merchant_id,
            customer_id: self.customer_id,
            address_id: self.address_id,
            payout_type: self.payout_type,
            payout_method_id: self.payout_method_id,
            amount: self.amount,
            destination_currency: self.destination_currency,
            source_currency: self.source_currency,
            description: self.description,
            recurring: self.recurring,
            auto_fulfill: self.auto_fulfill,
            return_url: self.return_url,
            entity_type: self.entity_type,
            metadata: self.metadata,
            created_at: self.created_at,
            last_modified_at: self.last_modified_at,
            profile_id: self.profile_id,
            status: self.status,
            attempt_count: self.attempt_count,
        }
    }

    fn from_storage_model(storage_model: Self::StorageModel) -> Self {
        Self {
            payout_id: storage_model.payout_id,
            merchant_id: storage_model.merchant_id,
            customer_id: storage_model.customer_id,
            address_id: storage_model.address_id,
            payout_type: storage_model.payout_type,
            payout_method_id: storage_model.payout_method_id,
            amount: storage_model.amount,
            destination_currency: storage_model.destination_currency,
            source_currency: storage_model.source_currency,
            description: storage_model.description,
            recurring: storage_model.recurring,
            auto_fulfill: storage_model.auto_fulfill,
            return_url: storage_model.return_url,
            entity_type: storage_model.entity_type,
            metadata: storage_model.metadata,
            created_at: storage_model.created_at,
            last_modified_at: storage_model.last_modified_at,
            profile_id: storage_model.profile_id,
            status: storage_model.status,
            attempt_count: storage_model.attempt_count,
        }
    }
}
impl DataModelExt for PayoutsUpdate {
    type StorageModel = DieselPayoutsUpdate;
    fn to_storage_model(self) -> Self::StorageModel {
        match self {
            Self::Update {
                amount,
                destination_currency,
                source_currency,
                description,
                recurring,
                auto_fulfill,
                return_url,
                entity_type,
                metadata,
                profile_id,
                status,
            } => DieselPayoutsUpdate::Update {
                amount,
                destination_currency,
                source_currency,
                description,
                recurring,
                auto_fulfill,
                return_url,
                entity_type,
                metadata,
                profile_id,
                status,
            },
            Self::PayoutMethodIdUpdate { payout_method_id } => {
                DieselPayoutsUpdate::PayoutMethodIdUpdate { payout_method_id }
            }
            Self::RecurringUpdate { recurring } => {
                DieselPayoutsUpdate::RecurringUpdate { recurring }
            }
            Self::AttemptCountUpdate { attempt_count } => {
                DieselPayoutsUpdate::AttemptCountUpdate { attempt_count }
            }
        }
    }

    #[allow(clippy::todo)]
    fn from_storage_model(_storage_model: Self::StorageModel) -> Self {
        todo!("Reverse map should no longer be needed")
    }
}
