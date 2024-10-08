use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{
        Context,
        Poll,
    },
    time::Instant,
};

use astria_core::{
    generated::protocol::transactions::v1alpha1 as raw,
    primitive::v1::asset::IbcPrefixed,
    protocol::{
        abci::AbciErrorCode,
        transaction::v1alpha1::SignedTransaction,
    },
};
use astria_eyre::eyre::WrapErr as _;
use cnidarium::Storage;
use futures::{
    Future,
    FutureExt,
};
use prost::Message as _;
use tendermint::{
    abci::Code,
    v0_38::abci::{
        request,
        response,
        MempoolRequest,
        MempoolResponse,
    },
};
use tower::Service;
use tower_abci::BoxError;
use tracing::{
    instrument,
    Instrument as _,
};

use crate::{
    accounts,
    address,
    app::ActionHandler as _,
    mempool::{
        get_account_balances,
        Mempool as AppMempool,
        RemovalReason,
    },
    metrics::Metrics,
    transaction,
};

const MAX_TX_SIZE: usize = 256_000; // 256 KB

/// Mempool handles [`request::CheckTx`] abci requests.
//
/// It performs a stateless check of the given transaction,
/// returning a [`tendermint::v0_38::abci::response::CheckTx`].
#[derive(Clone)]
pub(crate) struct Mempool {
    storage: Storage,
    inner: AppMempool,
    metrics: &'static Metrics,
}

impl Mempool {
    pub(crate) fn new(storage: Storage, mempool: AppMempool, metrics: &'static Metrics) -> Self {
        Self {
            storage,
            inner: mempool,
            metrics,
        }
    }
}

impl Service<MempoolRequest> for Mempool {
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<MempoolResponse, BoxError>> + Send + 'static>>;
    type Response = MempoolResponse;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: MempoolRequest) -> Self::Future {
        use penumbra_tower_trace::v038::RequestExt as _;
        let span = req.create_span();
        let storage = self.storage.clone();
        let mut mempool = self.inner.clone();
        let metrics = self.metrics;
        async move {
            let rsp = match req {
                MempoolRequest::CheckTx(req) => MempoolResponse::CheckTx(
                    handle_check_tx(req, storage.latest_snapshot(), &mut mempool, metrics).await,
                ),
            };
            Ok(rsp)
        }
        .instrument(span)
        .boxed()
    }
}

/// Handles a [`request::CheckTx`] request.
///
/// Performs stateless checks (decoding and signature check),
/// as well as stateful checks (nonce and balance checks).
///
/// If the tx passes all checks, status code 0 is returned.
#[allow(clippy::too_many_lines)]
#[instrument(skip_all)]
async fn handle_check_tx<S: accounts::StateReadExt + address::StateReadExt + 'static>(
    req: request::CheckTx,
    state: S,
    mempool: &mut AppMempool,
    metrics: &'static Metrics,
) -> response::CheckTx {
    use sha2::Digest as _;

    let start_parsing = Instant::now();

    let request::CheckTx {
        tx, ..
    } = req;

    let tx_hash = sha2::Sha256::digest(&tx).into();
    let tx_len = tx.len();

    if tx_len > MAX_TX_SIZE {
        metrics.increment_check_tx_removed_too_large();
        return response::CheckTx {
            code: Code::Err(AbciErrorCode::TRANSACTION_TOO_LARGE.value()),
            log: format!(
                "transaction size too large; allowed: {MAX_TX_SIZE} bytes, got {}",
                tx.len()
            ),
            info: AbciErrorCode::TRANSACTION_TOO_LARGE.info(),
            ..response::CheckTx::default()
        };
    }

    let raw_signed_tx = match raw::SignedTransaction::decode(tx) {
        Ok(tx) => tx,
        Err(e) => {
            return response::CheckTx {
                code: Code::Err(AbciErrorCode::INVALID_PARAMETER.value()),
                log: format!("{e:#}"),
                info: "failed decoding bytes as a protobuf SignedTransaction".into(),
                ..response::CheckTx::default()
            };
        }
    };
    let signed_tx = match SignedTransaction::try_from_raw(raw_signed_tx) {
        Ok(tx) => tx,
        Err(e) => {
            return response::CheckTx {
                code: Code::Err(AbciErrorCode::INVALID_PARAMETER.value()),
                info: "the provided bytes was not a valid protobuf-encoded SignedTransaction, or \
                       the signature was invalid"
                    .into(),
                log: format!("{e:#}"),
                ..response::CheckTx::default()
            };
        }
    };

    let finished_parsing = Instant::now();
    metrics.record_check_tx_duration_seconds_parse_tx(
        finished_parsing.saturating_duration_since(start_parsing),
    );

    if let Err(e) = signed_tx.check_stateless().await {
        metrics.increment_check_tx_removed_failed_stateless();
        return response::CheckTx {
            code: Code::Err(AbciErrorCode::INVALID_PARAMETER.value()),
            info: "transaction failed stateless check".into(),
            log: format!("{e:#}"),
            ..response::CheckTx::default()
        };
    };

    let finished_check_stateless = Instant::now();
    metrics.record_check_tx_duration_seconds_check_stateless(
        finished_check_stateless.saturating_duration_since(finished_parsing),
    );

    if let Err(e) = transaction::check_nonce_mempool(&signed_tx, &state).await {
        metrics.increment_check_tx_removed_stale_nonce();
        return response::CheckTx {
            code: Code::Err(AbciErrorCode::INVALID_NONCE.value()),
            info: "failed verifying transaction nonce".into(),
            log: format!("{e:#}"),
            ..response::CheckTx::default()
        };
    };

    let finished_check_nonce = Instant::now();
    metrics.record_check_tx_duration_seconds_check_nonce(
        finished_check_nonce.saturating_duration_since(finished_check_stateless),
    );

    if let Err(e) = transaction::check_chain_id_mempool(&signed_tx, &state).await {
        return response::CheckTx {
            code: Code::Err(AbciErrorCode::INVALID_CHAIN_ID.value()),
            info: "failed verifying chain id".into(),
            log: format!("{e:#}"),
            ..response::CheckTx::default()
        };
    }

    let finished_check_chain_id = Instant::now();
    metrics.record_check_tx_duration_seconds_check_chain_id(
        finished_check_chain_id.saturating_duration_since(finished_check_nonce),
    );

    if let Some(removal_reason) = mempool.check_removed_comet_bft(tx_hash).await {
        match removal_reason {
            RemovalReason::Expired => {
                metrics.increment_check_tx_removed_expired();
                return response::CheckTx {
                    code: Code::Err(AbciErrorCode::TRANSACTION_EXPIRED.value()),
                    info: "transaction expired in app's mempool".into(),
                    log: "Transaction expired in the app's mempool".into(),
                    ..response::CheckTx::default()
                };
            }
            RemovalReason::FailedPrepareProposal(err) => {
                metrics.increment_check_tx_removed_failed_execution();
                return response::CheckTx {
                    code: Code::Err(AbciErrorCode::TRANSACTION_FAILED.value()),
                    info: "transaction failed execution in prepare_proposal()".into(),
                    log: format!("transaction failed execution because: {err}"),
                    ..response::CheckTx::default()
                };
            }
            RemovalReason::NonceStale => {
                return response::CheckTx {
                    code: Code::Err(AbciErrorCode::INVALID_NONCE.value()),
                    info: "transaction removed from app mempool due to stale nonce".into(),
                    log: "Transaction from app mempool due to stale nonce".into(),
                    ..response::CheckTx::default()
                };
            }
            RemovalReason::LowerNonceInvalidated => {
                return response::CheckTx {
                    code: Code::Err(AbciErrorCode::LOWER_NONCE_INVALIDATED.value()),
                    info: "transaction removed from app mempool due to lower nonce being \
                           invalidated"
                        .into(),
                    log: "Transaction removed from app mempool due to lower nonce being \
                          invalidated"
                        .into(),
                    ..response::CheckTx::default()
                };
            }
        }
    };

    let finished_check_removed = Instant::now();
    metrics.record_check_tx_duration_seconds_check_removed(
        finished_check_removed.saturating_duration_since(finished_check_chain_id),
    );

    // tx is valid, push to mempool with current state
    let address = match state
        .try_base_prefixed(&signed_tx.verification_key().address_bytes())
        .await
        .context("failed to generate address for signed transaction")
    {
        Err(err) => {
            return response::CheckTx {
                code: Code::Err(AbciErrorCode::INTERNAL_ERROR.value()),
                info: AbciErrorCode::INTERNAL_ERROR.info(),
                log: format!("failed to generate address because: {err:#}"),
                ..response::CheckTx::default()
            };
        }
        Ok(address) => address,
    };

    // fetch current account
    let current_account_nonce = match state
        .get_account_nonce(address)
        .await
        .wrap_err("failed fetching nonce for account")
    {
        Err(err) => {
            return response::CheckTx {
                code: Code::Err(AbciErrorCode::INTERNAL_ERROR.value()),
                info: AbciErrorCode::INTERNAL_ERROR.info(),
                log: format!("failed to fetch account nonce because: {err:#}"),
                ..response::CheckTx::default()
            };
        }
        Ok(nonce) => nonce,
    };

    let finished_convert_address = Instant::now();
    metrics.record_check_tx_duration_seconds_convert_address(
        finished_convert_address.saturating_duration_since(finished_check_removed),
    );

    // grab cost of transaction
    let transaction_cost = match transaction::get_total_transaction_cost(&signed_tx, &state)
        .await
        .context("failed fetching cost of the transaction")
    {
        Err(err) => {
            return response::CheckTx {
                code: Code::Err(AbciErrorCode::INTERNAL_ERROR.value()),
                info: AbciErrorCode::INTERNAL_ERROR.info(),
                log: format!("failed to fetch cost of the transaction because: {err:#}"),
                ..response::CheckTx::default()
            };
        }
        Ok(transaction_cost) => transaction_cost,
    };

    let finished_fetch_tx_cost = Instant::now();
    metrics.record_check_tx_duration_seconds_fetch_tx_cost(
        finished_fetch_tx_cost.saturating_duration_since(finished_convert_address),
    );

    // grab current account's balances
    let current_account_balance: HashMap<IbcPrefixed, u128> =
        match get_account_balances(&state, address)
            .await
            .with_context(|| "failed fetching balances for account `{address}`")
        {
            Err(err) => {
                return response::CheckTx {
                    code: Code::Err(AbciErrorCode::INTERNAL_ERROR.value()),
                    info: AbciErrorCode::INTERNAL_ERROR.info(),
                    log: format!("failed to fetch account balances because: {err:#}"),
                    ..response::CheckTx::default()
                };
            }
            Ok(account_balance) => account_balance,
        };

    let finished_fetch_balances = Instant::now();
    metrics.record_check_tx_duration_seconds_fetch_balances(
        finished_fetch_balances.saturating_duration_since(finished_fetch_tx_cost),
    );

    let actions_count = signed_tx.actions().len();

    if let Err(err) = mempool
        .insert(
            Arc::new(signed_tx),
            current_account_nonce,
            current_account_balance,
            transaction_cost,
        )
        .await
    {
        return response::CheckTx {
            code: Code::Err(AbciErrorCode::TRANSACTION_INSERTION_FAILED.value()),
            info: "transaction insertion failed".into(),
            log: format!("transaction insertion failed because: {err:#}"),
            ..response::CheckTx::default()
        };
    }

    let mempool_len = mempool.len().await;

    metrics
        .record_check_tx_duration_seconds_insert_to_app_mempool(finished_fetch_balances.elapsed());
    metrics.record_actions_per_transaction_in_mempool(actions_count);
    metrics.record_transaction_in_mempool_size_bytes(tx_len);
    metrics.set_transactions_in_mempool_total(mempool_len);

    response::CheckTx::default()
}
