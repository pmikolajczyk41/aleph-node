//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use crate::aleph_cli::AlephCli;
use crate::executor::AlephExecutor;
use aleph_primitives::AlephSessionApi;
use aleph_runtime::{self, opaque::Block, RuntimeApi, MAX_BLOCK_SIZE};
use finality_aleph::{
    run_aleph_consensus, AlephBlockImport, AlephConfig, JustificationNotification, Metrics,
    MillisecsPerBlock, Protocol, SessionPeriod,
};
use futures::channel::mpsc;
use log::warn;
use sc_client_api::ExecutorProvider;
use sc_consensus_aura::{ImportQueueParams, SlotProportion, StartAuraParams};
use sc_service::{error::Error as ServiceError, Configuration, TFullClient, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sp_api::ProvideRuntimeApi;
use sp_consensus::SlotData;
use sp_consensus_aura::sr25519::AuthorityPair as AuraPair;
use sp_runtime::{
    generic::BlockId,
    traits::{Block as BlockT, Header as HeaderT, Zero},
};
use std::sync::Arc;

type FullClient = sc_service::TFullClient<Block, RuntimeApi, AlephExecutor>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

#[allow(clippy::type_complexity)]
pub fn new_partial(
    config: &Configuration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block, FullClient>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            AlephBlockImport<Block, FullBackend, FullClient>,
            mpsc::UnboundedReceiver<JustificationNotification<Block>>,
            Option<Telemetry>,
            Option<Metrics<<<Block as BlockT>::Header as HeaderT>::Hash>>,
        ),
    >,
    ServiceError,
> {
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = AlephExecutor::new(
        config.wasm_method,
        config.default_heap_pages,
        config.max_runtime_instances,
    );

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, AlephExecutor>(
            config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
            executor,
        )?;

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager
            .spawn_handle()
            .spawn("telemetry", None, worker.run());
        telemetry
    });

    let client: Arc<TFullClient<_, _, _>> = Arc::new(client);

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let metrics = config.prometheus_registry().cloned().and_then(|r| {
        Metrics::register(&r)
            .map_err(|err| {
                warn!("Failed to register Prometheus metrics\n{:?}", err);
            })
            .ok()
    });

    let (justification_tx, justification_rx) = mpsc::unbounded();
    let aleph_block_import =
        AlephBlockImport::new(client.clone() as Arc<_>, justification_tx, metrics.clone());

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?.slot_duration();

    let import_queue =
        sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(ImportQueueParams {
            block_import: aleph_block_import.clone(),
            justification_import: Some(Box::new(aleph_block_import.clone())),
            client: client.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
                    sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
                        *timestamp,
                        slot_duration,
                    );

                Ok((timestamp, slot))
            },
            spawner: &task_manager.spawn_essential_handle(),
            registry: config.prometheus_registry(),
            can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(
                client.executor().clone(),
            ),
            check_for_equivocation: Default::default(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        })?;

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (aleph_block_import, justification_rx, telemetry, metrics),
    })
}

/// Builds a new service for a full client.
pub fn new_full(
    mut config: Configuration,
    aleph_config: AlephCli,
) -> Result<TaskManager, ServiceError> {
    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (block_import, justification_rx, mut telemetry, metrics),
    } = new_partial(&config)?;

    config
        .network
        .extra_sets
        .push(finality_aleph::peers_set_config(Protocol::Generic));

    config
        .network
        .extra_sets
        .push(finality_aleph::peers_set_config(Protocol::Validator));

    let (network, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            block_announce_validator_builder: None,
            warp_sync: None,
        })?;

    let session_period = SessionPeriod(
        client
            .runtime_api()
            .session_period(&BlockId::Number(Zero::zero()))
            .unwrap(),
    );

    let millisecs_per_block = MillisecsPerBlock(
        client
            .runtime_api()
            .millisecs_per_block(&BlockId::Number(Zero::zero()))
            .unwrap(),
    );

    let unit_creation_delay = aleph_config.unit_creation_delay();

    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks: Option<()> = None;
    let prometheus_registry = config.prometheus_registry().cloned();

    let rpc_extensions_builder = {
        let client = client.clone();
        let pool = transaction_pool.clone();

        Box::new(move |deny_unsafe, _| {
            let deps = crate::rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                deny_unsafe,
            };

            Ok(crate::rpc::create_full(deps))
        })
    };

    let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        network: network.clone(),
        client: client.clone(),
        keystore: keystore_container.sync_keystore(),
        task_manager: &mut task_manager,
        transaction_pool: transaction_pool.clone(),
        rpc_extensions_builder,
        backend,
        system_rpc_tx,
        config,
        telemetry: telemetry.as_mut(),
    })?;

    let mut proposer_factory = sc_basic_authorship::ProposerFactory::new(
        task_manager.spawn_handle(),
        client.clone(),
        transaction_pool,
        prometheus_registry.as_ref(),
        None,
    );
    proposer_factory.set_default_block_size_limit(MAX_BLOCK_SIZE as usize);

    let can_author_with = sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
    let raw_slot_duration = slot_duration.slot_duration();

    let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _, _>(
        StartAuraParams {
            slot_duration,
            client: client.clone(),
            select_chain: select_chain.clone(),
            block_import,
            proposer_factory,
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
                    sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
                        *timestamp,
                        raw_slot_duration,
                    );

                Ok((timestamp, slot))
            },
            force_authoring,
            backoff_authoring_blocks,
            keystore: keystore_container.sync_keystore(),
            can_author_with,
            sync_oracle: network.clone(),
            justification_sync_link: network.clone(),
            block_proposal_slot_portion: SlotProportion::new(2f32 / 3f32),
            max_block_proposal_slot_portion: None,
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        },
    )?;

    task_manager
        .spawn_essential_handle()
        .spawn_blocking("aura", None, aura);

    let aleph_config = AlephConfig {
        network,
        client,
        select_chain,
        session_period,
        millisecs_per_block,
        spawn_handle: task_manager.spawn_handle(),
        keystore: keystore_container.keystore(),
        justification_rx,
        metrics,
        unit_creation_delay,
    };
    task_manager.spawn_essential_handle().spawn_blocking(
        "aleph",
        None,
        run_aleph_consensus(aleph_config),
    );

    network_starter.start_network();
    Ok(task_manager)
}
