use base64::{engine::general_purpose, Engine as _};
use bincode;
use raiko_core::{
    interfaces::{aggregate_proofs, ProofRequest},
    preflight::parse_l1_batch_proposal_tx_for_pacaya_fork,
    provider::rpc::RpcBlockDataProvider,
    Raiko,
};
use raiko_lib::{
    consts::SupportedChainSpecs,
    input::{AggregationGuestInput, AggregationGuestOutput, GuestBatchInput, GuestInput},
    prover::{IdWrite, Proof},
    utils::{zlib_compress_data, zlib_decompress_data},
};
use raiko_reqpool::{
    AggregationRequestEntity, BatchGuestInputRequestEntity, BatchProofRequestEntity,
    GuestInputRequestEntity, RequestEntity, RequestKey, SingleProofRequestEntity, Status,
    StatusWithContext,
};
use reth_primitives::B256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot, Semaphore,
};
use tracing::{debug, trace};

use crate::{Action, Pool};

/// Backend runs in the background, and handles the actions from the actor.
#[derive(Clone)]
pub(crate) struct Backend {
    pool: Pool,
    chain_specs: SupportedChainSpecs,
    internal_tx: Sender<RequestKey>,
    proving_semaphore: Arc<Semaphore>,
}

// TODO: load pool and notify internal channel
impl Backend {
    /// Run the backend in background.
    ///
    /// The returned channel sender is used to send actions to the actor, and the actor will
    /// act on the actions and send responses back.
    pub async fn serve_in_background(
        pool: Pool,
        chain_specs: SupportedChainSpecs,
        pause_rx: Receiver<()>,
        action_rx: Receiver<(Action, oneshot::Sender<Result<StatusWithContext, String>>)>,
        max_proving_concurrency: usize,
    ) {
        let channel_size = std::env::var("INTERNAL_CHANNEL_SIZE")
            .unwrap_or("1024".to_string())
            .parse::<usize>()
            .unwrap_or(1024);
        let (internal_tx, internal_rx) = mpsc::channel::<RequestKey>(channel_size);
        tokio::spawn(async move {
            Backend {
                pool,
                chain_specs,
                internal_tx,
                proving_semaphore: Arc::new(Semaphore::new(max_proving_concurrency)),
            }
            .serve(action_rx, internal_rx, pause_rx)
            .await;
        });
    }

    // There are three incoming channels:
    // 1. action_rx: actions from the external Actor
    // 2. internal_rx: internal signals from the backend itself
    // 3. pause_rx: pause signal from the external Actor
    async fn serve(
        mut self,
        mut action_rx: Receiver<(Action, oneshot::Sender<Result<StatusWithContext, String>>)>,
        mut internal_rx: Receiver<RequestKey>,
        mut pause_rx: Receiver<()>,
    ) {
        loop {
            tokio::select! {
                Some((action, resp_tx)) = action_rx.recv() => {
                    let request_key = action.request_key().clone();
                    let response = self.handle_external_action(action.clone()).await;

                    // Signal the request key to the internal channel, to move on to the next step, whatever the result is
                    //
                    // NOTE: Why signal whatever the result is? It's for fault tolerance, to ensure the request will be
                    // handled even when something unexpected happens.
                    self.ensure_internal_signal(request_key).await;

                    // When the client side is closed, the response channel is closed, and sending response to the
                    // channel will return an error. So we discard the result of sending response to the external actor.
                    let _discard = resp_tx.send(response.clone());
                }
                Some(request_key) = internal_rx.recv() => {
                    self.handle_internal_signal(request_key.clone()).await;
                }
                Some(()) = pause_rx.recv() => {
                    tracing::info!("Actor Backend received pause-signal, halting");
                    if let Err(err) = self.halt().await {
                        tracing::error!("Actor Backend failed to halt: {err:?}");
                    }
                }
                else => {
                    // All channels are closed, exit the loop
                    tracing::info!("Actor Backend exited");
                    break;
                }
            }
        }
    }

    async fn handle_external_action(
        &mut self,
        action: Action,
    ) -> Result<StatusWithContext, String> {
        match action {
            Action::Prove {
                request_key,
                request_entity,
            } => match self.pool.get_status(&request_key) {
                Ok(None) => {
                    tracing::debug!("Actor Backend received prove-action {request_key}, and it is not in pool, registering");
                    self.register(request_key.clone(), request_entity).await
                }
                Ok(Some(status)) => match status.status() {
                    Status::Registered | Status::WorkInProgress | Status::Success { .. } => {
                        tracing::debug!("Actor Backend received prove-action {request_key}, but it is already {status}, skipping");
                        Ok(status)
                    }
                    Status::Cancelled { .. } => {
                        tracing::warn!("Actor Backend received prove-action {request_key}, and it is cancelled, re-registering");
                        self.register(request_key, request_entity).await
                    }
                    Status::Failed { .. } => {
                        tracing::warn!("Actor Backend received prove-action {request_key}, and it is failed, re-registering");
                        self.register(request_key, request_entity).await
                    }
                },
                Err(err) => {
                    tracing::error!(
                        "Actor Backend failed to get status of prove-action {request_key}: {err:?}"
                    );
                    Err(err)
                }
            },
            Action::Cancel { request_key } => match self.pool.get_status(&request_key) {
                Ok(None) => {
                    tracing::warn!("Actor Backend received cancel-action {request_key}, but it is not in pool, skipping");
                    Err("request is not in pool".to_string())
                }
                Ok(Some(status)) => match status.status() {
                    Status::Registered | Status::WorkInProgress => {
                        tracing::debug!("Actor Backend received cancel-action {request_key}, and it is {status}, cancelling");
                        self.cancel(request_key, status).await
                    }

                    Status::Failed { .. } | Status::Cancelled { .. } | Status::Success { .. } => {
                        tracing::debug!("Actor Backend received cancel-action {request_key}, but it is already {status}, skipping");
                        Ok(status)
                    }
                },
                Err(err) => {
                    tracing::error!(
                        "Actor Backend failed to get status of cancel-action {request_key}: {err:?}"
                    );
                    Err(err)
                }
            },
        }
    }

    // Check the request status and then move on to the next step accordingly.
    async fn handle_internal_signal(&mut self, request_key: RequestKey) {
        match self.pool.get(&request_key) {
            Ok(Some((request_entity, status))) => match status.status() {
                Status::Registered => match request_entity {
                    RequestEntity::SingleProof(entity) => {
                        tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, proving single proof");
                        self.prove_single(request_key.clone(), entity).await;
                        self.ensure_internal_signal(request_key).await;
                    }
                    RequestEntity::Aggregation(entity) => {
                        tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, proving aggregation proof");
                        self.prove_aggregation(request_key.clone(), entity).await;
                        self.ensure_internal_signal(request_key).await;
                    }
                    RequestEntity::BatchProof(entity) => {
                        tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, proving batch proof");
                        self.prove_batch(request_key.clone(), entity).await;
                        self.ensure_internal_signal(request_key).await;
                    }
                    RequestEntity::GuestInput(entity) => {
                        tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, proving single proof");
                        self.generate_guest_input(request_key.clone(), entity).await;
                        self.ensure_internal_signal(request_key).await;
                    }
                    RequestEntity::BatchGuestInput(entity) => {
                        tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, proving single proof");
                        self.generate_batch_guest_input(request_key.clone(), entity)
                            .await;
                        self.ensure_internal_signal(request_key).await;
                    }
                },
                Status::WorkInProgress => {
                    // Wait for proving completion
                    tracing::debug!(
                        "Actor Backend checks a work-in-progress request {request_key}, elapsed: {elapsed:?}",
                        elapsed = chrono::Utc::now() - status.timestamp(),
                    );
                    self.ensure_internal_signal_after(request_key, Duration::from_secs(3))
                        .await;
                }
                Status::Success { .. } | Status::Cancelled { .. } | Status::Failed { .. } => {
                    tracing::debug!("Actor Backend received internal signal {request_key}, status: {status}, done");
                }
            },
            Ok(None) => {
                tracing::warn!(
                    "Actor Backend received internal signal {request_key}, but it is not in pool, skipping"
                );
            }
            Err(err) => {
                // Fault tolerance: re-enqueue the internal signal after 3 seconds
                tracing::warn!(
                    "Actor Backend failed to get status of internal signal {request_key}: {err:?}, performing fault tolerance and retrying later"
                );
                self.ensure_internal_signal_after(request_key, Duration::from_secs(3))
                    .await;
            }
        }
    }

    // Ensure signal the request key to the internal channel.
    //
    // Note that this function will retry sending the signal until success.
    async fn ensure_internal_signal(&mut self, request_key: RequestKey) {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        let internal_tx = self.internal_tx.clone();
        tokio::spawn(async move {
            loop {
                ticker.tick().await; // first tick is immediate
                if let Err(err) = internal_tx.send(request_key.clone()).await {
                    tracing::error!("Actor Backend failed to send internal signal {request_key}: {err:?}, retrying. It should not happen, please issue a bug report");
                } else {
                    break;
                }
            }
        });
    }

    async fn ensure_internal_signal_after(&mut self, request_key: RequestKey, after: Duration) {
        let mut timer = tokio::time::interval(after);
        timer.tick().await; // first tick is immediate
        timer.tick().await;
        self.ensure_internal_signal(request_key).await
    }

    // Register a new request to the pool and notify the actor.
    async fn register(
        &mut self,
        request_key: RequestKey,
        request_entity: RequestEntity,
    ) -> Result<StatusWithContext, String> {
        // 1. Register to the pool
        let status = StatusWithContext::new_registered();
        if let Err(err) = self
            .pool
            .add(request_key.clone(), request_entity, status.clone())
        {
            return Err(err);
        }

        Ok(status)
    }

    async fn cancel(
        &mut self,
        request_key: RequestKey,
        old_status: StatusWithContext,
    ) -> Result<StatusWithContext, String> {
        if old_status.status() != &Status::Registered
            && old_status.status() != &Status::WorkInProgress
        {
            tracing::warn!("Actor Backend received cancel-action {request_key}, but it is not registered or work-in-progress, skipping");
            return Ok(old_status);
        }

        // Case: old_status is registered: mark the request as cancelled in the pool and return directly
        if old_status.status() == &Status::Registered {
            let status = StatusWithContext::new_cancelled();
            self.pool.update_status(request_key, status.clone())?;
            return Ok(status);
        }

        // Case: old_status is work-in-progress:
        // 1. Cancel the proving work by the cancel token // TODO: cancel token
        // 2. Remove the proof id from the pool
        // 3. Mark the request as cancelled in the pool
        match &request_key {
            RequestKey::GuestInput(..) => {
                let status = StatusWithContext::new_cancelled();
                self.pool.update_status(request_key, status.clone())?;
                Ok(status)
            }
            RequestKey::SingleProof(key) => {
                raiko_core::interfaces::cancel_proof(
                    key.proof_type().clone(),
                    (
                        key.chain_id().clone(),
                        key.block_number().clone(),
                        key.block_hash().clone(),
                        *key.proof_type() as u8,
                    ),
                    Box::new(&mut self.pool),
                )
                .await
                .or_else(|e| {
                    if e.to_string().contains("No data for query") {
                        tracing::warn!("Actor Backend received cancel-action {request_key}, but it is already cancelled or not yet started, skipping");
                        Ok(())
                    } else {
                        tracing::error!(
                            "Actor Backend received cancel-action {request_key}, but failed to cancel proof: {e:?}"
                        );
                        Err(format!("failed to cancel proof: {e:?}"))
                    }
                })?;

                // 3. Mark the request as cancelled in the pool
                let status = StatusWithContext::new_cancelled();
                self.pool.update_status(request_key, status.clone())?;
                Ok(status)
            }
            RequestKey::Aggregation(..) => {
                let status = StatusWithContext::new_cancelled();
                self.pool.update_status(request_key, status.clone())?;
                Ok(status)
            }
            RequestKey::BatchProof(..) => {
                let status = StatusWithContext::new_cancelled();
                self.pool.update_status(request_key, status.clone())?;
                Ok(status)
            }
            RequestKey::BatchGuestInput(..) => {
                let status = StatusWithContext::new_cancelled();
                self.pool.update_status(request_key, status.clone())?;
                Ok(status)
            }
        }
    }

    async fn generate_guest_input(
        &mut self,
        request_key: RequestKey,
        request_entity: GuestInputRequestEntity,
    ) {
        self.prove(request_key.clone(), |mut actor, request_key| async move {
            do_generate_guest_input(
                &mut actor.pool,
                &actor.chain_specs,
                request_key,
                request_entity,
            )
            .await
        })
        .await;
    }

    async fn generate_batch_guest_input(
        &mut self,
        request_key: RequestKey,
        request_entity: BatchGuestInputRequestEntity,
    ) {
        self.prove(request_key.clone(), |mut actor, request_key| async move {
            do_generate_batch_guest_input(
                &mut actor.pool,
                &actor.chain_specs,
                request_key,
                request_entity,
            )
            .await
        })
        .await;
    }

    async fn prove_single(
        &mut self,
        request_key: RequestKey,
        request_entity: SingleProofRequestEntity,
    ) {
        self.prove(request_key.clone(), |mut actor, request_key| async move {
            do_prove_single(
                &mut actor.pool,
                &actor.chain_specs,
                request_key,
                request_entity,
            )
            .await
        })
        .await;
    }

    async fn prove_aggregation(
        &mut self,
        request_key: RequestKey,
        request_entity: AggregationRequestEntity,
    ) {
        self.prove(request_key.clone(), |mut actor, request_key| async move {
            do_prove_aggregation(&mut actor.pool, request_key.clone(), request_entity).await
        })
        .await;
    }

    async fn prove_batch(
        &mut self,
        request_key: RequestKey,
        request_entity: BatchProofRequestEntity,
    ) {
        self.prove(request_key.clone(), |mut actor, request_key| async move {
            do_prove_batch(
                &mut actor.pool,
                &actor.chain_specs,
                request_key.clone(),
                request_entity,
            )
            .await
        })
        .await;
    }

    /// Generic method to handle proving for different types of proofs
    async fn prove<F, Fut>(&mut self, request_key: RequestKey, prove_fn: F)
    where
        F: FnOnce(Backend, RequestKey) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Proof, String>> + Send + 'static,
    {
        let request_key_ = request_key.clone();

        let pool_status = self
            .pool
            .get_status(&request_key)
            .unwrap()
            .unwrap()
            .into_status();
        if matches!(pool_status, Status::Success { .. } | Status::WorkInProgress) {
            tracing::warn!("Actor Backend received prove-action {request_key}, but it is not registered, skipping");
            return;
        }

        // 1. Update the request status in pool to WorkInProgress
        if let Err(err) = self
            .pool
            .update_status(request_key.clone(), Status::WorkInProgress.into())
        {
            tracing::error!(
                "Actor Backend failed to update status of prove-action {request_key}: {err:?}, status: {status}",
                status = Status::WorkInProgress,
            );
            return;
        }

        // 2. Start the proving work in a separate thread
        let mut actor = self.clone();
        let proving_semaphore = self.proving_semaphore.clone();
        let (semaphore_acquired_tx, semaphore_acquired_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            // Acquire a permit from the semaphore before starting the proving work
            let _permit = proving_semaphore
                .acquire()
                .await
                .expect("semaphore should not be closed");
            semaphore_acquired_tx.send(()).unwrap();

            // 2.1. Start the proving work
            let proven_status = prove_fn(actor.clone(), request_key.clone())
                .await
                .map(|proof| Status::Success { proof })
                .unwrap_or_else(|error| Status::Failed { error });

            match &proven_status {
                Status::Success { proof } => {
                    tracing::info!(
                        "Actor Backend successfully proved {request_key}. Proof: {proof}"
                    );
                }
                Status::Failed { error } => {
                    tracing::error!("Actor Backend failed to prove {request_key}: {error}");
                }
                _ => {}
            }

            // 2.2. Update the request status in pool to the resulted status
            if let Err(err) = actor
                .pool
                .update_status(request_key.clone(), proven_status.clone().into())
            {
                tracing::error!(
                    "Actor Backend failed to update status of prove-action {request_key}: {err:?}, status: {proven_status}"
                );
                return;
            }
            // The permit is automatically dropped here, releasing the semaphore
        });

        // Only set up panic handler if we have a backup request key (for single proofs)
        let mut pool_ = self.pool.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                if e.is_panic() {
                    tracing::error!("Actor Backend panicked while proving: {e:?}");
                    let status = Status::Failed {
                        error: e.to_string(),
                    };
                    if let Err(err) =
                        pool_.update_status(request_key_.clone(), status.clone().into())
                    {
                        tracing::error!(
                                "Actor Backend failed to update status of prove-action {request_key_}: {err:?}, status: {status}",
                                status = status,
                            );
                    }
                } else {
                    tracing::error!("Actor Backend failed to prove: {e:?}");
                }
            }
        });

        // Wait for the semaphore to be acquired
        semaphore_acquired_rx.await.unwrap();
    }

    async fn halt(&mut self) -> Result<(), String> {
        // TODO: implement halt for pause
        Ok(())
    }
}

pub async fn do_generate_guest_input(
    _pool: &mut Pool,
    chain_specs: &SupportedChainSpecs,
    request_key: RequestKey,
    request_entity: GuestInputRequestEntity,
) -> Result<Proof, String> {
    tracing::info!("Generating proof for {request_key}");

    let l1_chain_spec = chain_specs
        .get_chain_spec(&request_entity.l1_network())
        .ok_or_else(|| {
            format!(
                "unsupported l1 network: {}, it should not happen, please issue a bug report",
                request_entity.l1_network()
            )
        })?;
    let taiko_chain_spec = chain_specs
        .get_chain_spec(&request_entity.network())
        .ok_or_else(|| {
            format!(
                "unsupported raiko network: {}, it should not happen, please issue a bug report",
                request_entity.network()
            )
        })?;
    let proof_request = ProofRequest {
        block_number: *request_entity.block_number(),
        l1_inclusion_block_number: *request_entity.l1_inclusion_block_number(),
        network: request_entity.network().clone(),
        l1_network: request_entity.l1_network().clone(),
        graffiti: request_entity.graffiti().clone(),
        prover: Default::default(),
        proof_type: Default::default(),
        blob_proof_type: request_entity.blob_proof_type().clone(),
        prover_args: request_entity.prover_args().clone(),
        batch_id: 0,
        l2_block_numbers: Vec::new(),
    };
    let raiko = Raiko::new(l1_chain_spec, taiko_chain_spec.clone(), proof_request);
    let provider = RpcBlockDataProvider::new(
        &taiko_chain_spec.rpc.clone(),
        request_entity.block_number() - 1,
    )
    .await
    .map_err(|err| format!("failed to create rpc block data provider: {err:?}"))?;

    let input = raiko
        .generate_input(provider)
        .await
        .map_err(|e| format!("failed to generate input: {e:?}"))?;

    let input_proof = serde_json::to_string(&input).expect("input serialize ok");
    Ok(Proof {
        proof: Some(input_proof),
        ..Default::default()
    })
}

// TODO: cache input, reference to raiko_host::cache
// TODO: memory tracking
// TODO: metrics
// TODO: measurement
pub async fn do_prove_single(
    pool: &mut dyn IdWrite,
    chain_specs: &SupportedChainSpecs,
    request_key: RequestKey,
    request_entity: SingleProofRequestEntity,
) -> Result<Proof, String> {
    tracing::info!("Generating proof for {request_key}");

    let l1_chain_spec = chain_specs
        .get_chain_spec(&request_entity.l1_network())
        .ok_or_else(|| {
            format!(
                "unsupported l1 network: {}, it should not happen, please issue a bug report",
                request_entity.l1_network()
            )
        })?;
    let taiko_chain_spec = chain_specs
        .get_chain_spec(&request_entity.network())
        .ok_or_else(|| {
            format!(
                "unsupported raiko network: {}, it should not happen, please issue a bug report",
                request_entity.network()
            )
        })?;
    let proof_request = ProofRequest {
        block_number: *request_entity.block_number(),
        l1_inclusion_block_number: *request_entity.l1_inclusion_block_number(),
        network: request_entity.network().clone(),
        l1_network: request_entity.l1_network().clone(),
        graffiti: request_entity.graffiti().clone(),
        prover: request_entity.prover().clone(),
        proof_type: request_entity.proof_type().clone(),
        blob_proof_type: request_entity.blob_proof_type().clone(),
        prover_args: request_entity.prover_args().clone(),
        batch_id: 0,
        l2_block_numbers: Vec::new(),
    };
    let raiko = Raiko::new(l1_chain_spec, taiko_chain_spec.clone(), proof_request);
    let provider = RpcBlockDataProvider::new(
        &taiko_chain_spec.rpc.clone(),
        request_entity.block_number() - 1,
    )
    .await
    .map_err(|err| format!("failed to create rpc block data provider: {err:?}"))?;

    // double check if we already have the guest_input
    let input: GuestInput =
        if let Some(guest_input_value) = request_entity.prover_args().get("guest_input") {
            let guest_input_json: String = serde_json::from_value(guest_input_value.clone())
                .expect("guest_input should be a string");
            let mut input: GuestInput = serde_json::from_str(&guest_input_json)
                .map_err(|err| format!("failed to deserialize guest_input: {err:?}"))?;
            // update missing fields
            let prover_data = &input.taiko.prover_data;
            if !(prover_data.graffiti.eq(request_entity.graffiti())
                && prover_data.prover.eq(request_entity.prover()))
            {
                input.taiko.prover_data = raiko_lib::input::TaikoProverData {
                    graffiti: request_entity.graffiti().clone(),
                    prover: request_entity.prover().clone(),
                }
            }
            input
        } else {
            // 1. Generate the proof input
            raiko
                .generate_input(provider)
                .await
                .map_err(|e| format!("failed to generate input: {e:?}"))?
        };

    // 2. Generate the proof output
    let output = raiko
        .get_output(&input)
        .map_err(|e| format!("failed to get output: {e:?}"))?;

    // 3. Generate the proof
    let proof = raiko
        .prove(input, &output, Some(pool))
        .await
        .map_err(|err| format!("failed to generate single proof: {err:?}"))?;

    Ok(proof)
}

async fn do_prove_aggregation(
    pool: &mut dyn IdWrite,
    request_key: RequestKey,
    request_entity: AggregationRequestEntity,
) -> Result<Proof, String> {
    let proof_type = request_key.proof_type().clone();
    let proofs = request_entity.proofs().clone();

    let input = AggregationGuestInput { proofs };
    let output = AggregationGuestOutput { hash: B256::ZERO };
    let config = serde_json::to_value(request_entity.prover_args())
        .map_err(|err| format!("failed to serialize prover args: {err:?}"))?;

    let proof = aggregate_proofs(proof_type, input, &output, &config, Some(pool))
        .await
        .map_err(|err| format!("failed to generate aggregation proof: {err:?}"))?;

    Ok(proof)
}

async fn new_raiko_for_batch_request(
    chain_specs: &SupportedChainSpecs,
    request_entity: BatchProofRequestEntity,
) -> Result<Raiko, String> {
    let l1_chain_spec = chain_specs
        .get_chain_spec(&request_entity.guest_input_entity().l1_network())
        .expect("unsupported l1 network");
    let taiko_chain_spec = chain_specs
        .get_chain_spec(&request_entity.guest_input_entity().network())
        .expect("unsupported taiko network");
    let batch_id = request_entity.guest_input_entity().batch_id();
    let l1_include_block_number = request_entity
        .guest_input_entity()
        .l1_inclusion_block_number();
    // parse the batch proposal tx to get all prove blocks
    let all_prove_blocks = parse_l1_batch_proposal_tx_for_pacaya_fork(
        &l1_chain_spec,
        &taiko_chain_spec,
        *l1_include_block_number,
        *batch_id,
    )
    .await
    .map_err(|err| format!("Could not parse L1 batch proposal tx: {err:?}"))?;

    let proof_request = ProofRequest {
        block_number: 0,
        batch_id: *request_entity.guest_input_entity().batch_id(),
        l1_inclusion_block_number: *request_entity
            .guest_input_entity()
            .l1_inclusion_block_number(),
        network: request_entity.guest_input_entity().network().clone(),
        l1_network: request_entity.guest_input_entity().l1_network().clone(),
        graffiti: request_entity.guest_input_entity().graffiti().clone(),
        prover: request_entity.prover().clone(),
        proof_type: request_entity.proof_type().clone(),
        blob_proof_type: request_entity
            .guest_input_entity()
            .blob_proof_type()
            .clone(),
        prover_args: request_entity.prover_args().clone(),
        l2_block_numbers: all_prove_blocks.clone(),
    };

    Ok(Raiko::new(l1_chain_spec, taiko_chain_spec, proof_request))
}

async fn generate_input_for_batch(raiko: &Raiko) -> Result<GuestBatchInput, String> {
    let provider_target_blocks = (raiko.request.l2_block_numbers[0] - 1
        ..=*raiko.request.l2_block_numbers.last().unwrap())
        .collect();
    let provider =
        RpcBlockDataProvider::new_batch(&raiko.taiko_chain_spec.rpc, provider_target_blocks)
            .await
            .expect("Could not create RpcBlockDataProvider");
    let input = raiko
        .generate_batch_input(provider)
        .await
        .map_err(|e| format!("failed to generate batch input: {e:?}"))?;
    Ok(input)
}

pub async fn do_generate_batch_guest_input(
    _pool: &mut Pool,
    chain_specs: &SupportedChainSpecs,
    request_key: RequestKey,
    request_entity: BatchGuestInputRequestEntity,
) -> Result<Proof, String> {
    trace!("batch guest input for: {request_key:?}");
    let batch_proof_request_entity = BatchProofRequestEntity::new_with_guest_input_entity(
        request_entity.clone(),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let raiko = new_raiko_for_batch_request(chain_specs, batch_proof_request_entity)
        .await
        .map_err(|err| format!("failed to create raiko: {err:?}"))?;
    let input = generate_input_for_batch(&raiko)
        .await
        .map_err(|err| format!("failed to generate batch guest input: {err:?}"))?;
    let input_proof = bincode::serialize(&input)
        .map_err(|err| format!("failed to serialize input to bincode: {err:?}"))?;
    let compressed_bytes = zlib_compress_data(&input_proof).unwrap();
    let compressed_b64: String = general_purpose::STANDARD.encode(&compressed_bytes);
    tracing::debug!(
        "compress redis input: input_proof {} bytes to compressed_b64 {} bytes.",
        input_proof.len(),
        compressed_b64.len()
    );
    Ok(Proof {
        proof: Some(compressed_b64),
        ..Default::default()
    })
}

async fn do_prove_batch(
    pool: &mut dyn IdWrite,
    chain_specs: &SupportedChainSpecs,
    request_key: RequestKey,
    request_entity: BatchProofRequestEntity,
) -> Result<Proof, String> {
    tracing::info!("Generating proof for {request_key}");

    let raiko = new_raiko_for_batch_request(chain_specs, request_entity).await?;
    let input = if let Some(batch_guest_input) = raiko.request.prover_args.get("batch_guest_input")
    {
        // Tricky: originally the input was created (and pass around) by prove() infra,
        // so it's a base64 string(in Proof).
        // after we get it from db somewhere before, we need to pass it down here, but there is no known
        // string carrier in key / entity, so we call deser twice, value -> string -> struct.
        let b64_encoded_string: String = serde_json::from_value(batch_guest_input.clone())
            .map_err(|err| {
                format!("failed to deserialize batch_guest_input from value: {err:?}")
            })?;
        let compressed_bytes = general_purpose::STANDARD
            .decode(&b64_encoded_string)
            .unwrap();
        let decompressed_bytes = zlib_decompress_data(&compressed_bytes)
            .map_err(|err| format!("failed to decompress batch_guest_input: {err:?}"))?;
        let guest_input: GuestBatchInput = bincode::deserialize(&decompressed_bytes)
            .map_err(|err| format!("failed to deserialize bincode batch_guest_input: {err:?}"))?;
        guest_input
    } else {
        tracing::warn!("rebuild batch guest input for request: {request_key:?}");
        generate_input_for_batch(&raiko)
            .await
            .map_err(|err| format!("failed to generate batch guest input: {err:?}"))?
    };

    let output = raiko
        .get_batch_output(&input)
        .map_err(|e| format!("failed to get guest batch output: {e:?}"))?;
    debug!("batch guest output: {output:?}");
    let proof = raiko
        .batch_prove(input, &output, Some(pool))
        .await
        .map_err(|e| format!("failed to generate batch proof: {e:?}"))?;
    Ok(proof)
}
