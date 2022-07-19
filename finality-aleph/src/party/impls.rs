use std::{collections::HashSet, fmt::Debug, marker::PhantomData, sync::Arc, time::Duration};

use aleph_bft::{DelayConfig, SpawnHandle};
use aleph_primitives::KEY_TYPE;
use async_trait::async_trait;
use futures::channel::oneshot;
use log::{debug, trace, warn};
use sc_client_api::Backend;
use sp_consensus::SelectChain;
use sp_keystore::CryptoStore;
use sp_runtime::traits::{Block as BlockT, Header, NumberFor, SaturatedConversion};

use crate::{
    crypto::{AuthorityPen, AuthorityVerifier, Keychain},
    data_io::{ChainTracker, DataStore, OrderedDataInterpreter},
    default_aleph_config, mpsc,
    network::{split, ManagerError, RequestBlocks, SessionManager},
    party::{
        aggregator,
        authority::{SubtaskCommon, Subtasks, Task as AuthorityTask},
        backup::ABFTBackup,
        traits::{AlephClient, Block, NodeSessionManager, SessionInfo},
    },
    AuthorityId, ClientForAleph, JustificationNotification, Metrics, NodeIndex, SessionBoundaries,
    SessionId, SessionPeriod, SplitData, UnitCreationDelay,
};

pub struct AuthoritySubtaskImpl<C, SC, B, RB, BE>
where
    B: BlockT,
    C: crate::ClientForAleph<B, BE> + Send + Sync + 'static,
    C::Api: aleph_primitives::AlephSessionApi<B>,
    BE: Backend<B> + 'static,
    SC: SelectChain<B> + 'static,
    RB: RequestBlocks<B>,
{
    client: Arc<C>,
    select_chain: SC,
    session_period: SessionPeriod,
    unit_creation_delay: UnitCreationDelay,
    authority_justification_tx: mpsc::UnboundedSender<JustificationNotification<B>>,
    block_requester: RB,
    metrics: Option<Metrics<<B::Header as Header>::Hash>>,
    spawn_handle: crate::SpawnHandle,
    session_manager: SessionManager<SplitData<B>>,
    keystore: Arc<dyn CryptoStore>,
    _phantom: PhantomData<BE>,
}

impl<C, SC, B, RB, BE> AuthoritySubtaskImpl<C, SC, B, RB, BE>
where
    B: BlockT,
    C: crate::ClientForAleph<B, BE> + Send + Sync + 'static,
    C::Api: aleph_primitives::AlephSessionApi<B>,
    BE: Backend<B> + 'static,
    SC: SelectChain<B> + 'static,
    RB: RequestBlocks<B>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<C>,
        select_chain: SC,
        session_period: SessionPeriod,
        unit_creation_delay: UnitCreationDelay,
        authority_justification_tx: mpsc::UnboundedSender<JustificationNotification<B>>,
        block_requester: RB,
        metrics: Option<Metrics<<B::Header as Header>::Hash>>,
        spawn_handle: crate::SpawnHandle,
        session_manager: SessionManager<SplitData<B>>,
        keystore: Arc<dyn CryptoStore>,
    ) -> Self {
        Self {
            client,
            select_chain,
            session_period,
            unit_creation_delay,
            authority_justification_tx,
            block_requester,
            metrics,
            spawn_handle,
            session_manager,
            keystore,
            _phantom: PhantomData,
        }
    }

    async fn spawn_subtasks(
        &self,
        session_id: SessionId,
        authorities: &[AuthorityId],
        node_id: NodeIndex,
        exit_rx: oneshot::Receiver<()>,
        backup: ABFTBackup,
    ) -> Subtasks {
        debug!(target: "afa", "Authority task {:?}", session_id);

        let authority_verifier = AuthorityVerifier::new(authorities.to_vec());
        let authority_pen =
            AuthorityPen::new(authorities[node_id.0].clone(), self.keystore.clone())
                .await
                .expect("The keys should sign successfully");
        let multikeychain =
            Keychain::new(node_id, authority_verifier.clone(), authority_pen.clone());

        let session_boundaries = SessionBoundaries::new(session_id, self.session_period);
        let (blocks_for_aggregator, blocks_from_interpreter) = mpsc::unbounded();

        let consensus_config = create_aleph_config(
            authorities.len(),
            node_id,
            session_id,
            self.unit_creation_delay,
        );

        let (chain_tracker, data_provider) = ChainTracker::new(
            self.select_chain.clone(),
            self.client.clone(),
            session_boundaries.clone(),
            Default::default(),
            self.metrics.clone(),
        );

        let ordered_data_interpreter = OrderedDataInterpreter::<B, C>::new(
            blocks_for_aggregator,
            self.client.clone(),
            session_boundaries.clone(),
        );

        let subtask_common = SubtaskCommon {
            spawn_handle: self.spawn_handle.clone(),
            session_id: session_id.0,
        };
        let aggregator_io = aggregator::IO {
            blocks_from_interpreter,
            justifications_for_chain: self.authority_justification_tx.clone(),
        };

        let data_network = self
            .session_manager
            .start_validator_session(session_id, authority_verifier, node_id, authority_pen)
            .await
            .expect("Failed to start validator session!");

        let (unfiltered_aleph_network, rmc_network) = split(data_network);
        let (data_store, aleph_network) = DataStore::new(
            session_boundaries.clone(),
            self.client.clone(),
            self.block_requester.clone(),
            Default::default(),
            unfiltered_aleph_network,
        );

        Subtasks::new(
            exit_rx,
            crate::party::member::task(
                subtask_common.clone(),
                multikeychain.clone(),
                consensus_config,
                aleph_network.into(),
                data_provider,
                ordered_data_interpreter,
                backup,
            ),
            crate::party::aggregator::task(
                subtask_common.clone(),
                self.client.clone(),
                aggregator_io,
                session_boundaries,
                self.metrics.clone(),
                multikeychain,
                rmc_network,
            ),
            crate::party::chain_tracker::task(subtask_common.clone(), chain_tracker),
            crate::party::data_store::task(subtask_common, data_store),
        )
    }
}

#[derive(Debug)]
pub enum SessionManagerError {
    NotAuthority,
    ManagerError(ManagerError),
}

#[async_trait]
impl<C, SC, B, RB, BE> NodeSessionManager for AuthoritySubtaskImpl<C, SC, B, RB, BE>
where
    B: BlockT,
    C: crate::ClientForAleph<B, BE> + Send + Sync + 'static,
    C::Api: aleph_primitives::AlephSessionApi<B>,
    BE: Backend<B> + 'static,
    SC: SelectChain<B> + 'static,
    RB: RequestBlocks<B>,
{
    type Error = SessionManagerError;

    async fn spawn_authority_task_for_session(
        &self,
        session: SessionId,
        node_id: NodeIndex,
        backup: ABFTBackup,
        authorities: &[AuthorityId],
    ) -> AuthorityTask {
        let (exit, exit_rx) = futures::channel::oneshot::channel();
        let subtasks = self
            .spawn_subtasks(session, authorities, node_id, exit_rx, backup)
            .await;

        AuthorityTask::new(
            self.spawn_handle
                .spawn_essential("aleph/session_authority", async move {
                    if subtasks.failed().await {
                        warn!(target: "aleph-party", "Authority subtasks failed.");
                    }
                }),
            node_id,
            exit,
        )
    }

    async fn early_start_validator_session(
        &self,
        session: SessionId,
        authorities: &[AuthorityId],
    ) -> Result<(), Self::Error> {
        let node_id = match self.node_idx(authorities).await {
            Some(id) => id,
            None => return Err(SessionManagerError::NotAuthority),
        };
        let authority_verifier = AuthorityVerifier::new(authorities.to_vec());
        let authority_pen =
            AuthorityPen::new(authorities[node_id.0].clone(), self.keystore.clone())
                .await
                .expect("The keys should sign successfully");
        self.session_manager
            .early_start_validator_session(session, authority_verifier, node_id, authority_pen)
            .map_err(SessionManagerError::ManagerError)
    }

    fn start_nonvalidator_session(
        &self,
        session: SessionId,
        authorities: &[AuthorityId],
    ) -> Result<(), Self::Error> {
        let authority_verifier = AuthorityVerifier::new(authorities.to_vec());

        self.session_manager
            .start_nonvalidator_session(session, authority_verifier)
            .map_err(SessionManagerError::ManagerError)
    }

    fn stop_session(&self, session: SessionId) -> Result<(), Self::Error> {
        self.session_manager
            .stop_session(session)
            .map_err(SessionManagerError::ManagerError)
    }

    async fn node_idx(&self, authorities: &[AuthorityId]) -> Option<NodeIndex> {
        let our_consensus_keys: HashSet<_> = self
            .keystore
            .keys(KEY_TYPE)
            .await
            .unwrap()
            .into_iter()
            .collect();
        trace!(target: "aleph-data-store", "Found {:?} consensus keys in our local keystore {:?}", our_consensus_keys.len(), our_consensus_keys);
        authorities
            .iter()
            .position(|pkey| our_consensus_keys.contains(&pkey.into()))
            .map(|id| id.into())
    }
}

pub struct AlephClientImpl<B, BE, CFA>
where
    B: BlockT,
    BE: Backend<B>,
    CFA: ClientForAleph<B, BE>,
{
    pub client: Arc<CFA>,
    pub _phantom: PhantomData<(B, BE)>,
}

impl<B, BE, CFA> AlephClient<B> for AlephClientImpl<B, BE, CFA>
where
    B: BlockT,
    BE: Backend<B>,
    CFA: ClientForAleph<B, BE>,
{
    fn best_block_number(&self) -> <B as Block>::Number {
        self.client.info().best_number
    }

    fn finalized_number(&self) -> <B as Block>::Number {
        self.client.info().finalized_number
    }
}

pub struct SessionInfoImpl {
    session_period: SessionPeriod,
}

impl SessionInfoImpl {
    pub fn new(session_period: SessionPeriod) -> Self {
        Self { session_period }
    }
}

impl<B: BlockT> SessionInfo<B> for SessionInfoImpl {
    fn session_id_from_block_num(&self, n: NumberFor<B>) -> SessionId {
        SessionId(n.saturated_into::<u32>() / self.session_period.0)
    }

    fn last_block_of_session(&self, session_id: SessionId) -> NumberFor<B> {
        ((session_id.0 + 1) * self.session_period.0 - 1).into()
    }

    fn first_block_of_session(&self, session_id: SessionId) -> NumberFor<B> {
        (session_id.0 * self.session_period.0).into()
    }
}

fn create_aleph_config(
    n_members: usize,
    node_id: NodeIndex,
    session_id: SessionId,
    unit_creation_delay: UnitCreationDelay,
) -> aleph_bft::Config {
    let mut consensus_config = default_aleph_config(n_members.into(), node_id, session_id.0 as u64);
    consensus_config.max_round = 7000;
    let unit_creation_delay = Arc::new(move |t| {
        if t == 0 {
            Duration::from_millis(2000)
        } else {
            exponential_slowdown(t, unit_creation_delay.0 as f64, 5000, 1.005)
        }
    });
    let unit_broadcast_delay = Arc::new(|t| exponential_slowdown(t, 4000., 0, 2.));
    let delay_config = DelayConfig {
        tick_interval: Duration::from_millis(100),
        requests_interval: Duration::from_millis(3000),
        unit_broadcast_delay,
        unit_creation_delay,
    };
    consensus_config.delay_config = delay_config;
    consensus_config
}

fn exponential_slowdown(
    t: usize,
    base_delay: f64,
    start_exp_delay: usize,
    exp_base: f64,
) -> Duration {
    // This gives:
    // base_delay, for t <= start_exp_delay,
    // base_delay * exp_base^(t - start_exp_delay), for t > start_exp_delay.
    let delay = if t < start_exp_delay {
        base_delay
    } else {
        let power = t - start_exp_delay;
        base_delay * exp_base.powf(power as f64)
    };
    let delay = delay.round() as u64;
    // the above will make it u64::MAX if it exceeds u64
    Duration::from_millis(delay)
}