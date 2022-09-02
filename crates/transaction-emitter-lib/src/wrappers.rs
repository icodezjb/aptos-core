// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    args::{ClusterArgs, EmitArgs},
    cluster::Cluster,
    emitter::{stats::TxnStats, EmitJobMode, EmitJobRequest, TxnEmitter},
    instance::Instance,
};
use anyhow::{Context, Result};
use aptos_logger::info;
use aptos_rest_client::{Client as RestClient, State, Transaction};
use aptos_sdk::transaction_builder::TransactionFactory;
use aptos_sdk::types::LocalAccount;
use cached_packages::aptos_stdlib;
use rand::{rngs::StdRng, Rng};
use rand_core::{OsRng, SeedableRng};
use std::{
    cmp::{max, min},
    time::Duration,
};

pub async fn reconfig(
    client: &RestClient,
    transaction_factory: &TransactionFactory,
    root_account: &mut LocalAccount,
) -> State {
    let aptos_version = client.get_aptos_version().await.unwrap();
    let (current, state) = aptos_version.into_parts();
    let current_version = *current.major.inner();
    let txn = root_account.sign_with_transaction_builder(
        transaction_factory
            .clone()
            .with_max_gas_amount(100000)
            .payload(aptos_stdlib::version_set_version(current_version + 1)),
    );
    let result = client.submit_and_wait(&txn).await;
    if let Err(e) = result {
        let last_transactions = client
            .get_account_transactions(root_account.address(), None, None)
            .await
            .map(|result| {
                result
                    .into_inner()
                    .iter()
                    .map(|t| {
                        if let Transaction::UserTransaction(ut) = t {
                            format!(
                                "user seq={}, payload={:?}",
                                ut.request.sequence_number, ut.request.payload
                            )
                        } else {
                            t.type_str().to_string()
                        }
                    })
                    .collect::<Vec<_>>()
            });

        panic!(
            "Couldn't execute {:?}, for account {:?}, error {:?}, last account transactions: {:?}",
            txn,
            root_account,
            e,
            last_transactions.unwrap_or_default()
        )
    }

    let transaction = result.unwrap();
    // Next transaction after reconfig should be a new epoch.
    let new_state = client
        .wait_for_version(transaction.inner().version().unwrap() + 1)
        .await
        .unwrap();

    info!(
        "Changed aptos version from {} (epoch={}, ledger_v={}), to {}, (epoch={}, ledger_v={})",
        current_version,
        state.epoch,
        state.version,
        current_version + 1,
        new_state.epoch,
        new_state.version
    );
    assert_ne!(state.epoch, new_state.epoch);

    new_state
}

pub async fn emit_transactions(
    cluster_args: &ClusterArgs,
    emit_args: &EmitArgs,
) -> Result<TxnStats> {
    let cluster = Cluster::try_from_cluster_args(cluster_args)
        .await
        .context("Failed to build cluster")?;
    emit_transactions_with_cluster(&cluster, emit_args, cluster_args.reuse_accounts).await
}

pub async fn emit_transactions_with_cluster(
    cluster: &Cluster,
    args: &EmitArgs,
    reuse_accounts: bool,
) -> Result<TxnStats> {
    let emitter_mode = EmitJobMode::create(args.mempool_backlog, args.target_tps);

    let duration = Duration::from_secs(args.duration);
    let client = cluster.random_instance().rest_client();
    let mut root_account = cluster.load_aptos_root_account(&client).await?;

    let state = reconfig(
        &client,
        &TransactionFactory::new(cluster.chain_id)
            .with_gas_unit_price(1)
            .with_transaction_expiration_time(args.txn_expiration_time_secs),
        &mut root_account,
    )
    .await;

    panic!("done : {:?}", state);

    let mut emitter = TxnEmitter::new(
        &mut root_account,
        client,
        TransactionFactory::new(cluster.chain_id)
            .with_gas_unit_price(1)
            .with_transaction_expiration_time(args.txn_expiration_time_secs),
        StdRng::from_seed(OsRng.gen()),
    );

    let transaction_mix = if args.transaction_type_weights.is_empty() {
        args.transaction_type.iter().map(|t| (*t, 1)).collect()
    } else {
        assert_eq!(
            args.transaction_type_weights.len(),
            args.transaction_type.len(),
            "Transaction types and weights need to be the same length"
        );
        args.transaction_type
            .iter()
            .cloned()
            .zip(args.transaction_type_weights.iter().cloned())
            .collect()
    };

    let mut emit_job_request =
        EmitJobRequest::new(cluster.all_instances().map(Instance::rest_client).collect())
            .mode(emitter_mode)
            .invalid_transaction_ratio(args.invalid_tx)
            .transaction_mix(transaction_mix)
            .txn_expiration_time_secs(args.txn_expiration_time_secs)
            .gas_price(1);
    if reuse_accounts {
        emit_job_request = emit_job_request.reuse_accounts();
    }
    let stats = emitter
        .emit_txn_for_with_stats(
            emit_job_request,
            duration,
            min(10, max(args.duration / 5, 1)),
        )
        .await?;
    Ok(stats)
}
