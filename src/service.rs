//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use codec::Encode;
use kulupu_runtime::{self, opaque::Block, AccountId, GenesisConfig, RuntimeApi};
use network::{config::DummyFinalityProofRequestBuilder, construct_simple_protocol};
use primitives::H256;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::io;
use std::net::SocketAddr;
use substrate_client::LongestChain;
use substrate_executor::native_executor_instance;
pub use substrate_executor::NativeExecutor;
use substrate_service::{
    error::Error as ServiceError, AbstractService, Configuration, ServiceBuilder, TaskExecutor
};
use transaction_pool::{self, txpool::Pool as TransactionPool};

use rpc_servers;

// Our native executor instance.
native_executor_instance!(
	pub Executor,
	kulupu_runtime::api::dispatch,
	kulupu_runtime::native_version,
);

construct_simple_protocol! {
    /// Demo protocol attachment for substrate.
    pub struct NodeProtocol where Block = Block { }
}

pub fn kulupu_inherent_data_providers(
    author: Option<&str>,
) -> Result<inherents::InherentDataProviders, ServiceError> {
    let inherent_data_providers = inherents::InherentDataProviders::new();

    if !inherent_data_providers.has_provider(&srml_timestamp::INHERENT_IDENTIFIER) {
        inherent_data_providers
            .register_provider(srml_timestamp::InherentDataProvider)
            .map_err(Into::into)
            .map_err(consensus_common::Error::InherentData)?;
    }

    if !inherent_data_providers.has_provider(&srml_anyupgrade::INHERENT_IDENTIFIER) {
        let upgrades = BTreeMap::default();
        // To plan a new hard fork, insert an item such as:
        // ```
        // 	srml_anyupgrade::Call::<kulupu_runtime::Runtime>::any(
        //		Box::new(srml_system::Call::set_code(<wasm>).into())
        //	).encode()
        // ```

        inherent_data_providers
            .register_provider(srml_anyupgrade::InherentDataProvider((0, upgrades)))
            .map_err(Into::into)
            .map_err(consensus_common::Error::InherentData)?;
    }

    if let Some(author) = author {
        if !inherent_data_providers.has_provider(&srml_rewards::INHERENT_IDENTIFIER) {
            inherent_data_providers
                .register_provider(srml_rewards::InherentDataProvider(
                    AccountId::from_h256(
                        H256::from_str(if author.starts_with("0x") {
                            &author[2..]
                        } else {
                            author
                        })
                        .expect("Invalid author account"),
                    )
                    .encode(),
                ))
                .map_err(Into::into)
                .map_err(consensus_common::Error::InherentData)?;
        }
    }

    Ok(inherent_data_providers)
}

/// Builds a new service for a full client.
pub fn new_full<C: Send + Default + 'static>(
    config: Configuration<C, GenesisConfig>,
    author: Option<&str>,
    threads: usize,
    round: u32,
    miner_listen_port: u32,
    task_executor: TaskExecutor,
) -> Result<impl AbstractService, ServiceError> {
    let is_authority = config.roles.is_authority();

    let (builder, inherent_data_providers) = new_full_start!(config, author);

    let service = builder
        .with_network_protocol(|_| Ok(NodeProtocol::new()))?
        .with_finality_proof_provider(|_client, _backend| Ok(Arc::new(()) as _))?
        .build()?;

    let handler = || {
        let subscriptions = rpc::Subscriptions::new(task_executor.clone());
        let mine = rpc::mine::Mine::new(service.client(), subscriptions.clone());
        rpc_servers::rpc_handler(mine)
    };

    let rpc_ws: Result<Option<rpc::WsServer>, io::Error> =
        maybe_start_server(config.rpc_ws, |address| {
            rpc_servers::start_ws(
                address,
                config.rpc_ws_max_connections,
                config.rpc_cors.as_ref(),
                handler(),
            )
        });

    if is_authority {
        let proposer = basic_authorship::ProposerFactory {
            client: service.client(),
            transaction_pool: service.transaction_pool(),
        };
        consensus_pow::start_mine(
            Box::new(service.client().clone()),
            service.client(),
            kulupu_pow::RandomXAlgorithm::new(service.client()),
            proposer,
            None,
            round,
            service.network(),
            std::time::Duration::new(2, 0),
            service.select_chain().map(|v| v.clone()),
            inherent_data_providers.clone(),
        );
    }
    Ok(service)
}

/// Builds a new service for a light client.
pub fn new_light<C: Send + Default + 'static>(
    config: Configuration<C, GenesisConfig>,
    author: Option<&str>,
) -> Result<impl AbstractService, ServiceError> {
    let inherent_data_providers = kulupu_inherent_data_providers(author)?;

    ServiceBuilder::new_light::<Block, RuntimeApi, Executor>(config)?
        .with_select_chain(|_config, backend| Ok(LongestChain::new(backend.clone())))?
        .with_transaction_pool(|config, client| {
            Ok(TransactionPool::new(
                config,
                transaction_pool::FullChainApi::new(client),
            ))
        })?
        .with_import_queue_and_fprb(
            |_config, client, _backend, _fetcher, select_chain, _transaction_pool| {
                let fprb = Box::new(DummyFinalityProofRequestBuilder::default()) as Box<_>;
                let import_queue = consensus_pow::import_queue(
                    Box::new(client.clone()),
                    client.clone(),
                    kulupu_pow::RandomXAlgorithm::new(client.clone(), 8011),
                    0,
                    select_chain,
                    inherent_data_providers.clone(),
                )?;

                Ok((import_queue, fprb))
            },
        )?
        .with_finality_proof_provider(|_client, _backend| Ok(Arc::new(()) as _))?
        .with_network_protocol(|_| Ok(NodeProtocol::new()))?
        .build()
}

fn maybe_start_server<T, F>(address: Option<SocketAddr>, start: F) -> Result<Option<T>, io::Error>
    where
        F: Fn(&SocketAddr) -> Result<T, io::Error>,
{
    Ok(match address {
        Some(mut address) => Some(start(&address).or_else(|e| match e.kind() {
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied => {
                warn!("Unable to bind server to {}. Trying random port.", address);
                address.set_port(0);
                start(&address)
            }
            _ => Err(e),
        })?),
        None => None,
    })
}