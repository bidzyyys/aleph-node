use std::{
    collections::HashSet, default::Default, marker::PhantomData, path::PathBuf, sync::Arc,
    time::Duration,
};

use aleph_bft::{DelayConfig, SpawnHandle};
use aleph_primitives::KEY_TYPE;
use futures::channel::mpsc;
use futures_timer::Delay;
use log::{debug, error, info, trace, warn};
use sc_client_api::Backend;
use sp_consensus::SelectChain;
use sp_keystore::CryptoStore;
use sp_runtime::traits::{Block, Header};
use tokio::{task::spawn_blocking, time::sleep};

use crate::{
    crypto::{AuthorityPen, AuthorityVerifier, Keychain},
    data_io::{ChainTracker, DataStore, OrderedDataInterpreter},
    default_aleph_config,
    justification::JustificationNotification,
    last_block_of_session,
    network::{split, RequestBlocks, SessionManager, SessionNetwork},
    party::{
        authority::{
            SubtaskCommon as AuthoritySubtaskCommon, Subtasks as AuthoritySubtasks,
            Task as AuthorityTask,
        },
        backup::ABFTBackup,
        task::{Handle, Task},
    },
    session_id_from_block_num,
    session_map::ReadOnlySessionMap,
    AuthorityId, Metrics, NodeIndex, SessionBoundaries, SessionId, SessionPeriod, SplitData,
    UnitCreationDelay,
};

mod aggregator;
mod authority;
mod backup;
mod chain_tracker;
mod data_store;
mod member;
mod task;

async fn get_node_index(
    authorities: &[AuthorityId],
    keystore: Arc<dyn CryptoStore>,
) -> Option<NodeIndex> {
    let our_consensus_keys: HashSet<_> =
        keystore.keys(KEY_TYPE).await.unwrap().into_iter().collect();
    trace!(target: "aleph-data-store", "Found {:?} consensus keys in our local keystore {:?}", our_consensus_keys.len(), our_consensus_keys);
    authorities
        .iter()
        .position(|pkey| our_consensus_keys.contains(&pkey.into()))
        .map(|id| id.into())
}

pub(crate) struct ConsensusPartyParams<B: Block, SC, C, RB> {
    pub session_manager: SessionManager<SplitData<B>>,
    pub session_authorities: ReadOnlySessionMap,
    pub session_period: SessionPeriod,
    pub spawn_handle: crate::SpawnHandle,
    pub client: Arc<C>,
    pub select_chain: SC,
    pub keystore: Arc<dyn CryptoStore>,
    pub block_requester: RB,
    pub metrics: Option<Metrics<<B::Header as Header>::Hash>>,
    pub authority_justification_tx: mpsc::UnboundedSender<JustificationNotification<B>>,
    pub unit_creation_delay: UnitCreationDelay,
    pub backup_saving_path: Option<PathBuf>,
}

pub(crate) struct ConsensusParty<B, C, BE, SC, RB>
where
    B: Block,
    C: crate::ClientForAleph<B, BE> + Send + Sync + 'static,
    C::Api: aleph_primitives::AlephSessionApi<B>,
    BE: Backend<B> + 'static,
    SC: SelectChain<B> + 'static,
    RB: RequestBlocks<B> + 'static,
{
    session_manager: SessionManager<SplitData<B>>,
    session_authorities: ReadOnlySessionMap,
    session_period: SessionPeriod,
    spawn_handle: crate::SpawnHandle,
    client: Arc<C>,
    select_chain: SC,
    keystore: Arc<dyn CryptoStore>,
    block_requester: RB,
    phantom: PhantomData<BE>,
    metrics: Option<Metrics<<B::Header as Header>::Hash>>,
    authority_justification_tx: mpsc::UnboundedSender<JustificationNotification<B>>,
    unit_creation_delay: UnitCreationDelay,
    backup_saving_path: Option<PathBuf>,
}

const SESSION_STATUS_CHECK_PERIOD: Duration = Duration::from_millis(1000);

impl<B, C, BE, SC, RB> ConsensusParty<B, C, BE, SC, RB>
where
    B: Block,
    C: crate::ClientForAleph<B, BE> + Send + Sync + 'static,
    C::Api: aleph_primitives::AlephSessionApi<B>,
    BE: Backend<B> + 'static,
    SC: SelectChain<B> + 'static,
    RB: RequestBlocks<B> + 'static,
{
    pub(crate) fn new(params: ConsensusPartyParams<B, SC, C, RB>) -> Self {
        let ConsensusPartyParams {
            session_manager,
            session_authorities,
            session_period,
            spawn_handle,
            client,
            select_chain,
            keystore,
            block_requester,
            metrics,
            authority_justification_tx,
            unit_creation_delay,
            backup_saving_path,
        } = params;
        Self {
            session_manager,
            client,
            keystore,
            select_chain,
            block_requester,
            metrics,
            authority_justification_tx,
            session_authorities,
            session_period,
            spawn_handle,
            phantom: PhantomData,
            unit_creation_delay,
            backup_saving_path,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_authority_subtasks(
        &self,
        node_id: NodeIndex,
        multikeychain: Keychain,
        data_network: SessionNetwork<SplitData<B>>,
        session_id: SessionId,
        authorities: Vec<AuthorityId>,
        backup: ABFTBackup,
        exit_rx: futures::channel::oneshot::Receiver<()>,
    ) -> AuthoritySubtasks {
        debug!(target: "aleph-party", "Authority task {:?}", session_id);
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

        let subtask_common = AuthoritySubtaskCommon {
            spawn_handle: self.spawn_handle.clone(),
            session_id: session_id.0,
        };
        let aggregator_io = aggregator::IO {
            blocks_from_interpreter,
            justifications_for_chain: self.authority_justification_tx.clone(),
        };

        let (unfiltered_aleph_network, rmc_network) = split(data_network);
        let (data_store, aleph_network) = DataStore::new(
            session_boundaries.clone(),
            self.client.clone(),
            self.block_requester.clone(),
            Default::default(),
            unfiltered_aleph_network,
        );

        AuthoritySubtasks::new(
            exit_rx,
            member::task(
                subtask_common.clone(),
                multikeychain.clone(),
                consensus_config,
                aleph_network.into(),
                data_provider,
                ordered_data_interpreter,
                backup,
            ),
            aggregator::task(
                subtask_common.clone(),
                self.client.clone(),
                aggregator_io,
                session_boundaries,
                self.metrics.clone(),
                multikeychain,
                rmc_network,
            ),
            chain_tracker::task(subtask_common.clone(), chain_tracker),
            data_store::task(subtask_common, data_store),
        )
    }

    async fn spawn_authority_task(
        &self,
        session_id: SessionId,
        node_id: NodeIndex,
        authorities: Vec<AuthorityId>,
        backup: ABFTBackup,
    ) -> AuthorityTask {
        let authority_verifier = AuthorityVerifier::new(authorities.clone());
        let authority_pen =
            AuthorityPen::new(authorities[node_id.0].clone(), self.keystore.clone())
                .await
                .expect("The keys should sign successfully");

        let keychain = Keychain::new(node_id, authority_verifier.clone(), authority_pen.clone());

        let data_network = self
            .session_manager
            .start_validator_session(session_id, authority_verifier, node_id, authority_pen)
            .await
            .expect("Failed to start validator session!");

        let (exit, exit_rx) = futures::channel::oneshot::channel();
        let authority_subtasks = self
            .spawn_authority_subtasks(
                node_id,
                keychain,
                data_network,
                session_id,
                authorities,
                backup,
                exit_rx,
            )
            .await;
        AuthorityTask::new(
            self.spawn_handle
                .spawn_essential("aleph/session_authority", async move {
                    if authority_subtasks.failed().await {
                        warn!(target: "aleph-party", "Authority subtasks failed.");
                    }
                }),
            node_id,
            exit,
        )
    }

    async fn run_session(&mut self, session_id: SessionId) {
        let last_block = last_block_of_session::<B>(session_id, self.session_period);
        if let Some(previous_session_id) = session_id.0.checked_sub(1) {
            let backup_saving_path = self.backup_saving_path.clone();
            spawn_blocking(move || backup::remove(backup_saving_path, previous_session_id));
        }

        // Early skip attempt -- this will trigger during catching up (initial sync).
        if self.client.info().best_number >= last_block {
            // We need to give the JustificationHandler some time to pick up the keychain for the new session,
            // validate justifications and finalize blocks. We wait 2000ms in total, checking every 200ms
            // if the last block has been finalized.
            for attempt in 0..10 {
                // We don't wait before the first attempt.
                if attempt != 0 {
                    Delay::new(Duration::from_millis(200)).await;
                }
                let last_finalized_number = self.client.info().finalized_number;
                if last_finalized_number >= last_block {
                    debug!(target: "aleph-party", "Skipping session {:?} early because block {:?} is already finalized", session_id, last_finalized_number);
                    return;
                }
            }
        }

        // We need to wait until session authority data is available for current session.
        // This should only be needed for the first ever session as all other session are known
        // at least one session earlier.
        let authority_data = match self
            .session_authorities
            .subscribe_to_insertion(session_id)
            .await
            .await
        {
            Err(e) => panic!(
                "Error while receiving the notification about current session {:?}",
                e
            ),
            Ok(authority_data) => authority_data,
        };
        let authorities = authority_data.authorities();

        trace!(target: "aleph-party", "Authority data for session {:?}: {:?}", session_id, authorities);
        let mut maybe_authority_task = if let Some(node_id) =
            get_node_index(authorities, self.keystore.clone()).await
        {
            match backup::rotate(self.backup_saving_path.clone(), session_id.0) {
                Ok(backup) => {
                    debug!(target: "aleph-party", "Running session {:?} as authority id {:?}", session_id, node_id);
                    Some(
                        self.spawn_authority_task(session_id, node_id, authorities.clone(), backup)
                            .await,
                    )
                }
                Err(err) => {
                    error!(
                        target: "AlephBFT-member",
                        "Error setting up backup saving for session {:?}. Not running the session: {}",
                        session_id, err
                    );
                    return;
                }
            }
        } else {
            debug!(target: "aleph-party", "Running session {:?} as non-authority", session_id);
            if let Err(e) = self
                .session_manager
                .start_nonvalidator_session(session_id, AuthorityVerifier::new(authorities.clone()))
            {
                warn!(target: "aleph-party", "Failed to start nonvalidator session{:?}:{:?}", session_id, e);
            }
            None
        };
        let mut check_session_status = Delay::new(SESSION_STATUS_CHECK_PERIOD);
        let next_session_id = SessionId(session_id.0 + 1);
        let mut start_next_session_network = Some(
            self.session_authorities
                .subscribe_to_insertion(next_session_id)
                .await,
        );
        loop {
            tokio::select! {
                _ = &mut check_session_status => {
                    let last_finalized_number = self.client.info().finalized_number;
                    if last_finalized_number >= last_block {
                        debug!(target: "aleph-party", "Terminating session {:?}", session_id);
                        break;
                    }
                    check_session_status = Delay::new(SESSION_STATUS_CHECK_PERIOD);
                },
                Some(next_session_authority_data) = async {
                    match &mut start_next_session_network {
                        Some(notification) => {
                            match notification.await {
                                Err(e) => {
                                    warn!(target: "aleph-party", "Error with subscription {:?}", e);
                                    start_next_session_network = Some(self.session_authorities.subscribe_to_insertion(next_session_id).await);
                                    None
                                },
                                Ok(next_session_authority_data) => {
                                    Some(next_session_authority_data)
                                }
                            }
                        },
                        None => None,
                    }
                } => {
                    let authority_verifier = AuthorityVerifier::new(next_session_authority_data.authorities().clone());
                    match get_node_index(next_session_authority_data.authorities(), self.keystore.clone()).await {
                        Some(node_id) => {
                            let authority_pen = AuthorityPen::new(
                                next_session_authority_data.authorities()[node_id.0].clone(),
                                self.keystore.clone(),
                            )
                            .await
                            .expect("The keys should sign successfully");

                            if let Err(e) = self
                                .session_manager
                                .early_start_validator_session(
                                    next_session_id,
                                    authority_verifier,
                                    node_id,
                                    authority_pen,
                                )
                            {
                                warn!(target: "aleph-party", "Failed to early start validator session{:?}:{:?}", next_session_id, e);
                            }
                        }
                        None => {
                            if let Err(e) = self
                                .session_manager
                                .start_nonvalidator_session(next_session_id, authority_verifier)
                            {
                                warn!(target: "aleph-party", "Failed to early start nonvalidator session{:?}:{:?}", next_session_id, e);
                            }
                        }
                    }
                    start_next_session_network = None;
                },
                Some(_) = async {
                    match maybe_authority_task.as_mut() {
                        Some(task) => Some(task.stopped().await),
                        None => None,
                    } } => {
                    warn!(target: "aleph-party", "Authority task ended prematurely, giving up for this session.");
                    maybe_authority_task = None;
                },
            }
        }
        if let Some(task) = maybe_authority_task {
            debug!(target: "aleph-party", "Stopping the authority task.");
            task.stop().await;
        }
        if let Err(e) = self.session_manager.stop_session(session_id) {
            warn!(target: "aleph-party", "Session Manager failed to stop in session {:?}: {:?}", session_id, e)
        }
    }

    pub async fn run(mut self) {
        let starting_session = self.catch_up().await;
        for curr_id in starting_session.0.. {
            info!(target: "aleph-party", "Running session {:?}.", curr_id);
            self.run_session(SessionId(curr_id)).await;
        }
    }

    async fn catch_up(&mut self) -> SessionId {
        let mut finalized_number = self.client.info().finalized_number;
        let mut previous_finalized_number = None;
        while self.block_requester.is_major_syncing()
            && Some(finalized_number) != previous_finalized_number
        {
            sleep(Duration::from_millis(500)).await;
            previous_finalized_number = Some(finalized_number);
            finalized_number = self.client.info().finalized_number;
        }
        session_id_from_block_num::<B>(finalized_number, self.session_period)
    }
}

pub(crate) fn create_aleph_config(
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

pub fn exponential_slowdown(
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

// TODO: :(
#[cfg(test)]
mod tests {}
