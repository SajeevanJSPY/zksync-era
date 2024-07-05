use std::time::Duration;

use secp256k1::{ecdsa::Signature, Message, PublicKey, Secp256k1, SecretKey};
use url::Url;
use zksync_basic_types::H256;
use zksync_node_framework::{
    service::{ServiceContext, StopReceiver},
    task::{Task, TaskId},
    wiring_layer::{WiringError, WiringLayer},
};
use zksync_prover_interface::inputs::TeeVerifierInput;
use zksync_tee_verifier::Verify;
use zksync_types::{tee_types::TeeType, L1BatchNumber};

use crate::{api_client::TeeApiClient, error::TeeProverError};

/// Wiring layer for `TeeProver`
///
/// ## Requests resources
///
/// no resources requested
///
/// ## Adds tasks
///
/// - `TeeProver`
#[derive(Debug)]
pub struct TeeProverLayer {
    api_url: Url,
    signing_key: SecretKey,
    attestation_quote_bytes: Vec<u8>,
    tee_type: TeeType,
}

impl TeeProverLayer {
    pub fn new(
        api_url: Url,
        signing_key: SecretKey,
        attestation_quote_bytes: Vec<u8>,
        tee_type: TeeType,
    ) -> Self {
        Self {
            api_url,
            signing_key,
            attestation_quote_bytes,
            tee_type,
        }
    }
}

#[async_trait::async_trait]
impl WiringLayer for TeeProverLayer {
    fn layer_name(&self) -> &'static str {
        "tee_prover_layer"
    }

    async fn wire(self: Box<Self>, mut context: ServiceContext<'_>) -> Result<(), WiringError> {
        let tee_prover_task = TeeProver {
            config: Default::default(),
            signing_key: self.signing_key,
            public_key: self.signing_key.public_key(&Secp256k1::new()),
            attestation_quote_bytes: self.attestation_quote_bytes,
            tee_type: self.tee_type,
            api_client: TeeApiClient::new(self.api_url),
        };
        context.add_task(tee_prover_task);
        Ok(())
    }
}

struct TeeProver {
    config: TeeProverConfig,
    signing_key: SecretKey,
    public_key: PublicKey,
    attestation_quote_bytes: Vec<u8>,
    tee_type: TeeType,
    api_client: TeeApiClient,
}

impl TeeProver {
    fn verify(
        &self,
        tvi: TeeVerifierInput,
    ) -> Result<(Signature, L1BatchNumber, H256), TeeProverError> {
        match tvi {
            TeeVerifierInput::V1(tvi) => {
                let verification_result = tvi.verify().map_err(TeeProverError::Verification)?;
                let root_hash_bytes = verification_result.value_hash.as_bytes();
                let batch_number = verification_result.batch_number;
                let msg_to_sign = Message::from_slice(root_hash_bytes)
                    .map_err(|e| TeeProverError::Verification(e.into()))?;
                let signature = self.signing_key.sign_ecdsa(msg_to_sign);
                Ok((signature, batch_number, verification_result.value_hash))
            }
            _ => Err(TeeProverError::Verification(anyhow::anyhow!(
                "Only TeeVerifierInput::V1 verification supported."
            ))),
        }
    }

    async fn step(&self) -> Result<(), TeeProverError> {
        match self.api_client.get_job().await? {
            Some(job) => {
                let (signature, batch_number, root_hash) = self.verify(*job)?;
                self.api_client
                    .submit_proof(
                        batch_number,
                        signature,
                        &self.public_key,
                        root_hash,
                        self.tee_type,
                    )
                    .await?;
            }
            None => tracing::trace!("There are currently no pending batches to be proven"),
        }
        Ok(())
    }
}

/// TEE prover configuration options.
#[derive(Debug, Clone)]
pub struct TeeProverConfig {
    /// Number of retries for transient errors before giving up on recovery (i.e., returning an error
    /// from [`Self::run()`]).
    pub max_retries: usize,
    /// Initial back-off interval when retrying recovery on a transient error. Each subsequent retry interval
    /// will be multiplied by [`Self.retry_backoff_multiplier`].
    pub initial_retry_backoff: Duration,
    pub retry_backoff_multiplier: f32,
    pub max_backoff: Duration,
}

impl Default for TeeProverConfig {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_retry_backoff: Duration::from_secs(1),
            retry_backoff_multiplier: 2.0,
            max_backoff: Duration::from_secs(128),
        }
    }
}

#[async_trait::async_trait]
impl Task for TeeProver {
    fn id(&self) -> TaskId {
        "tee_prover".into()
    }

    async fn run(self: Box<Self>, mut stop_receiver: StopReceiver) -> anyhow::Result<()> {
        tracing::info!("Starting the task {}", self.id());

        self.api_client
            .register_attestation(self.attestation_quote_bytes.clone(), &self.public_key)
            .await?;

        let mut retries = 1;
        let mut backoff = self.config.initial_retry_backoff;

        loop {
            if *stop_receiver.0.borrow() {
                tracing::info!("Stop signal received, shutting down TEE Prover component");
                return Ok(());
            }
            let result = self.step().await;
            match result {
                Ok(()) => {
                    retries = 1;
                    backoff = self.config.initial_retry_backoff;
                }
                Err(err) => {
                    if !err.is_transient() || retries > self.config.max_retries {
                        return Err(err.into());
                    }
                    retries += 1;
                    tracing::warn!(%err, "Failed TEE prover step function {retries}/{}, retrying in {} milliseconds.", self.config.max_retries, backoff.as_millis());
                    tokio::time::timeout(backoff, stop_receiver.0.changed())
                        .await
                        .ok();
                    backoff = std::cmp::min(
                        backoff.mul_f32(self.config.retry_backoff_multiplier),
                        self.config.max_backoff,
                    );
                }
            }
        }
    }
}