use std::collections::HashMap;

use astria_core::{
    primitive::v1::{
        asset,
        RollupId,
        TransactionId,
    },
    protocol::transaction::v1alpha1::{
        action::{
            Action,
            BridgeLockAction,
        },
        SignedTransaction,
        UnsignedTransaction,
    },
};
use astria_eyre::eyre::{
    ensure,
    Result,
    WrapErr as _,
};
use cnidarium::StateRead;
use tracing::instrument;

use crate::{
    accounts::StateReadExt as _,
    address::StateReadExt as _,
    bridge::StateReadExt as _,
    ibc::StateReadExt as _,
    state_ext::StateReadExt as _,
};

#[instrument(skip_all)]
pub(crate) async fn check_nonce_mempool<S: StateRead>(
    tx: &SignedTransaction,
    state: &S,
) -> Result<()> {
    let signer_address = state
        .try_base_prefixed(&tx.verification_key().address_bytes())
        .await
        .wrap_err(
            "failed constructing the signer address from signed transaction verification and \
             prefix provided by app state",
        )?;
    let curr_nonce = state
        .get_account_nonce(signer_address)
        .await
        .wrap_err("failed to get account nonce")?;
    ensure!(tx.nonce() >= curr_nonce, "nonce already used by account");
    Ok(())
}

#[instrument(skip_all)]
pub(crate) async fn check_chain_id_mempool<S: StateRead>(
    tx: &SignedTransaction,
    state: &S,
) -> Result<()> {
    let chain_id = state
        .get_chain_id()
        .await
        .wrap_err("failed to get chain id")?;
    ensure!(tx.chain_id() == chain_id.as_str(), "chain id mismatch");
    Ok(())
}

#[instrument(skip_all)]
pub(crate) async fn get_fees_for_transaction<S: StateRead>(
    tx: &UnsignedTransaction,
    state: &S,
) -> Result<HashMap<asset::IbcPrefixed, u128>> {
    let transfer_fee = state
        .get_transfer_base_fee()
        .await
        .wrap_err("failed to get transfer base fee")?;
    let ics20_withdrawal_fee = state
        .get_ics20_withdrawal_base_fee()
        .await
        .wrap_err("failed to get ics20 withdrawal base fee")?;
    let init_bridge_account_fee = state
        .get_init_bridge_account_base_fee()
        .await
        .wrap_err("failed to get init bridge account base fee")?;
    let bridge_lock_byte_cost_multiplier = state
        .get_bridge_lock_byte_cost_multiplier()
        .await
        .wrap_err("failed to get bridge lock byte cost multiplier")?;
    let bridge_sudo_change_fee = state
        .get_bridge_sudo_change_base_fee()
        .await
        .wrap_err("failed to get bridge sudo change fee")?;

    let mut fees_by_asset = HashMap::new();
    for (i, action) in tx.actions.iter().enumerate() {
        match action {
            Action::Transfer(act) => {
                transfer_update_fees(&act.fee_asset, &mut fees_by_asset, transfer_fee);
            }
            Action::Sequence(act) => {
                sequence_update_fees(state, &act.fee_asset, &mut fees_by_asset, &act.data).await?;
            }
            Action::Ics20Withdrawal(act) => ics20_withdrawal_updates_fees(
                &act.fee_asset,
                &mut fees_by_asset,
                ics20_withdrawal_fee,
            ),
            Action::InitBridgeAccount(act) => {
                fees_by_asset
                    .entry(act.fee_asset.to_ibc_prefixed())
                    .and_modify(|amt| *amt = amt.saturating_add(init_bridge_account_fee))
                    .or_insert(init_bridge_account_fee);
            }
            Action::BridgeLock(act) => {
                bridge_lock_update_fees(
                    act,
                    &mut fees_by_asset,
                    transfer_fee,
                    bridge_lock_byte_cost_multiplier,
                    i as u64,
                );
            }
            Action::BridgeUnlock(act) => {
                bridge_unlock_update_fees(&act.fee_asset, &mut fees_by_asset, transfer_fee);
            }
            Action::BridgeSudoChange(act) => {
                fees_by_asset
                    .entry(act.fee_asset.to_ibc_prefixed())
                    .and_modify(|amt| *amt = amt.saturating_add(bridge_sudo_change_fee))
                    .or_insert(bridge_sudo_change_fee);
            }
            Action::ValidatorUpdate(_)
            | Action::SudoAddressChange(_)
            | Action::IbcSudoChange(_)
            | Action::Ibc(_)
            | Action::IbcRelayerChange(_)
            | Action::FeeAssetChange(_)
            | Action::FeeChange(_) => {
                continue;
            }
        }
    }
    Ok(fees_by_asset)
}

// Checks that the account has enough balance to cover the total fees and transferred values
// for all actions in the transaction.
#[instrument(skip_all)]
pub(crate) async fn check_balance_for_total_fees_and_transfers<S: StateRead>(
    tx: &SignedTransaction,
    state: &S,
) -> Result<()> {
    let cost_by_asset = get_total_transaction_cost(tx, state)
        .await
        .context("failed to get transaction costs")?;

    for (asset, total_fee) in cost_by_asset {
        let balance = state
            .get_account_balance(tx, asset)
            .await
            .context("failed to get account balance")?;
        ensure!(
            balance >= total_fee,
            "insufficient funds for asset {}",
            asset
        );
    }

    Ok(())
}

// Returns the total cost of the transaction (fees and transferred values for all actions in the
// transaction).
#[instrument(skip_all)]
pub(crate) async fn get_total_transaction_cost<S: StateRead>(
    tx: &SignedTransaction,
    state: &S,
) -> Result<HashMap<asset::IbcPrefixed, u128>> {
    let mut cost_by_asset: HashMap<asset::IbcPrefixed, u128> =
        get_fees_for_transaction(tx.unsigned_transaction(), state)
            .await
            .context("failed to get fees for transaction")?;

    // add values transferred within the tx to the cost
    for action in tx.actions() {
        match action {
            Action::Transfer(act) => {
                cost_by_asset
                    .entry(act.asset.to_ibc_prefixed())
                    .and_modify(|amt| *amt = amt.saturating_add(act.amount))
                    .or_insert(act.amount);
            }
            Action::Ics20Withdrawal(act) => {
                cost_by_asset
                    .entry(act.denom.to_ibc_prefixed())
                    .and_modify(|amt| *amt = amt.saturating_add(act.amount))
                    .or_insert(act.amount);
            }
            Action::BridgeLock(act) => {
                cost_by_asset
                    .entry(act.asset.to_ibc_prefixed())
                    .and_modify(|amt| *amt = amt.saturating_add(act.amount))
                    .or_insert(act.amount);
            }
            Action::BridgeUnlock(act) => {
                let asset = state
                    .get_bridge_account_ibc_asset(tx)
                    .await
                    .wrap_err("failed to get bridge account asset id")?;
                cost_by_asset
                    .entry(asset)
                    .and_modify(|amt| *amt = amt.saturating_add(act.amount))
                    .or_insert(act.amount);
            }
            Action::ValidatorUpdate(_)
            | Action::SudoAddressChange(_)
            | Action::IbcSudoChange(_)
            | Action::Sequence(_)
            | Action::InitBridgeAccount(_)
            | Action::BridgeSudoChange(_)
            | Action::Ibc(_)
            | Action::IbcRelayerChange(_)
            | Action::FeeAssetChange(_)
            | Action::FeeChange(_) => {
                continue;
            }
        }
    }

    Ok(cost_by_asset)
}

fn transfer_update_fees(
    fee_asset: &asset::Denom,
    fees_by_asset: &mut HashMap<asset::IbcPrefixed, u128>,
    transfer_fee: u128,
) {
    fees_by_asset
        .entry(fee_asset.to_ibc_prefixed())
        .and_modify(|amt| *amt = amt.saturating_add(transfer_fee))
        .or_insert(transfer_fee);
}

async fn sequence_update_fees<S: StateRead>(
    state: &S,
    fee_asset: &asset::Denom,
    fees_by_asset: &mut HashMap<asset::IbcPrefixed, u128>,
    data: &[u8],
) -> Result<()> {
    let fee = crate::sequence::calculate_fee_from_state(data, state)
        .await
        .wrap_err("fee for sequence action overflowed; data too large")?;
    fees_by_asset
        .entry(fee_asset.to_ibc_prefixed())
        .and_modify(|amt| *amt = amt.saturating_add(fee))
        .or_insert(fee);
    Ok(())
}

fn ics20_withdrawal_updates_fees(
    fee_asset: &asset::Denom,
    fees_by_asset: &mut HashMap<asset::IbcPrefixed, u128>,
    ics20_withdrawal_fee: u128,
) {
    fees_by_asset
        .entry(fee_asset.to_ibc_prefixed())
        .and_modify(|amt| *amt = amt.saturating_add(ics20_withdrawal_fee))
        .or_insert(ics20_withdrawal_fee);
}

fn bridge_lock_update_fees(
    act: &BridgeLockAction,
    fees_by_asset: &mut HashMap<asset::IbcPrefixed, u128>,
    transfer_fee: u128,
    bridge_lock_byte_cost_multiplier: u128,
    tx_index_of_action: u64,
) {
    use astria_core::sequencerblock::v1alpha1::block::Deposit;

    let expected_deposit_fee = transfer_fee.saturating_add(
        crate::bridge::calculate_base_deposit_fee(&Deposit {
            bridge_address: act.to,
            // rollup ID doesn't matter here, as this is only used as a size-check
            rollup_id: RollupId::from_unhashed_bytes([0; 32]),
            amount: act.amount,
            asset: act.asset.clone(),
            destination_chain_address: act.destination_chain_address.clone(),
            source_transaction_id: TransactionId::new([0; 32]),
            source_action_index: tx_index_of_action,
        })
        .unwrap()
        .saturating_mul(bridge_lock_byte_cost_multiplier),
    );

    fees_by_asset
        .entry(act.asset.to_ibc_prefixed())
        .and_modify(|amt| *amt = amt.saturating_add(expected_deposit_fee))
        .or_insert(expected_deposit_fee);
}

fn bridge_unlock_update_fees(
    fee_asset: &asset::Denom,
    fees_by_asset: &mut HashMap<asset::IbcPrefixed, u128>,
    transfer_fee: u128,
) {
    fees_by_asset
        .entry(fee_asset.to_ibc_prefixed())
        .and_modify(|amt| *amt = amt.saturating_add(transfer_fee))
        .or_insert(transfer_fee);
}

#[cfg(test)]
mod tests {
    use astria_core::{
        primitive::v1::{
            asset::Denom,
            RollupId,
            ADDRESS_LEN,
        },
        protocol::transaction::v1alpha1::{
            action::{
                SequenceAction,
                TransferAction,
            },
            TransactionParams,
        },
    };
    use bytes::Bytes;
    use cnidarium::StateDelta;

    use super::*;
    use crate::{
        accounts::StateWriteExt as _,
        address::{
            StateReadExt,
            StateWriteExt as _,
        },
        app::test_utils::*,
        assets::StateWriteExt as _,
        bridge::StateWriteExt as _,
        ibc::StateWriteExt as _,
        sequence::StateWriteExt as _,
        test_utils::ASTRIA_PREFIX,
    };

    #[tokio::test]
    async fn check_balance_total_fees_transfers_ok() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state_tx = StateDelta::new(snapshot);

        state_tx.put_base_prefix("astria");
        state_tx.put_native_asset(&crate::test_utils::nria());
        state_tx.put_transfer_base_fee(12).unwrap();
        state_tx.put_sequence_action_base_fee(0);
        state_tx.put_sequence_action_byte_cost_multiplier(1);
        state_tx.put_ics20_withdrawal_base_fee(1).unwrap();
        state_tx.put_init_bridge_account_base_fee(12);
        state_tx.put_bridge_lock_byte_cost_multiplier(1);
        state_tx.put_bridge_sudo_change_base_fee(24);

        let other_asset = "other".parse::<Denom>().unwrap();

        let alice = get_alice_signing_key();
        let amount = 100;
        let data = Bytes::from_static(&[0; 32]);
        let transfer_fee = state_tx.get_transfer_base_fee().await.unwrap();
        state_tx
            .increase_balance(
                state_tx
                    .try_base_prefixed(&alice.address_bytes())
                    .await
                    .unwrap(),
                crate::test_utils::nria(),
                transfer_fee
                    + crate::sequence::calculate_fee_from_state(&data, &state_tx)
                        .await
                        .unwrap(),
            )
            .await
            .unwrap();
        state_tx
            .increase_balance(
                state_tx
                    .try_base_prefixed(&alice.address_bytes())
                    .await
                    .unwrap(),
                &other_asset,
                amount,
            )
            .await
            .unwrap();

        let actions = vec![
            Action::Transfer(TransferAction {
                asset: other_asset.clone(),
                amount,
                fee_asset: crate::test_utils::nria().into(),
                to: state_tx.try_base_prefixed(&[0; ADDRESS_LEN]).await.unwrap(),
            }),
            Action::Sequence(SequenceAction {
                rollup_id: RollupId::from_unhashed_bytes([0; 32]),
                data,
                fee_asset: crate::test_utils::nria().into(),
            }),
        ];

        let params = TransactionParams::builder()
            .nonce(0)
            .chain_id("test-chain-id")
            .build();
        let tx = UnsignedTransaction {
            actions,
            params,
        };

        let signed_tx = tx.into_signed(&alice);
        check_balance_for_total_fees_and_transfers(&signed_tx, &state_tx)
            .await
            .expect("sufficient balance for all actions");
    }

    #[tokio::test]
    async fn check_balance_total_fees_and_transfers_insufficient_other_asset_balance() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state_tx = StateDelta::new(snapshot);

        state_tx.put_base_prefix(ASTRIA_PREFIX);
        state_tx.put_native_asset(&crate::test_utils::nria());
        state_tx.put_transfer_base_fee(12).unwrap();
        state_tx.put_sequence_action_base_fee(0);
        state_tx.put_sequence_action_byte_cost_multiplier(1);
        state_tx.put_ics20_withdrawal_base_fee(1).unwrap();
        state_tx.put_init_bridge_account_base_fee(12);
        state_tx.put_bridge_lock_byte_cost_multiplier(1);
        state_tx.put_bridge_sudo_change_base_fee(24);

        let other_asset = "other".parse::<Denom>().unwrap();

        let alice = get_alice_signing_key();
        let amount = 100;
        let data = Bytes::from_static(&[0; 32]);
        let transfer_fee = state_tx.get_transfer_base_fee().await.unwrap();
        state_tx
            .increase_balance(
                state_tx
                    .try_base_prefixed(&alice.address_bytes())
                    .await
                    .unwrap(),
                crate::test_utils::nria(),
                transfer_fee
                    + crate::sequence::calculate_fee_from_state(&data, &state_tx)
                        .await
                        .unwrap(),
            )
            .await
            .unwrap();

        let actions = vec![
            Action::Transfer(TransferAction {
                asset: other_asset.clone(),
                amount,
                fee_asset: crate::test_utils::nria().into(),
                to: state_tx.try_base_prefixed(&[0; ADDRESS_LEN]).await.unwrap(),
            }),
            Action::Sequence(SequenceAction {
                rollup_id: RollupId::from_unhashed_bytes([0; 32]),
                data,
                fee_asset: crate::test_utils::nria().into(),
            }),
        ];

        let params = TransactionParams::builder()
            .nonce(0)
            .chain_id("test-chain-id")
            .build();
        let tx = UnsignedTransaction {
            actions,
            params,
        };

        let signed_tx = tx.into_signed(&alice);
        let err = check_balance_for_total_fees_and_transfers(&signed_tx, &state_tx)
            .await
            .err()
            .unwrap();
        assert!(
            err.root_cause()
                .to_string()
                .contains(&other_asset.to_ibc_prefixed().to_string())
        );
    }
}
