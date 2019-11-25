//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use codec::Encode;
use kulupu_runtime::{self, opaque::Block, AccountId, GenesisConfig, RuntimeApi};
use network::{config::DummyFinalityProofRequestBuilder, construct_simple_protocol};
use primitives::H256;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use substrate_client::LongestChain;
use substrate_executor::native_executor_instance;
pub use substrate_executor::NativeExecutor;
use substrate_service::{
    error::Error as ServiceError, AbstractService, Configuration, ServiceBuilder,
};
use transaction_pool::{self, txpool::Pool as TransactionPool};
use std::{thread, time};
use std::cell::RefCell;
use jsonrpc_core::{
    futures, futures::future::Future, Error, ErrorCode, MetaIoHandler, Params, Value,BoxFuture
};
use jsonrpc_pubsub::{PubSubHandler, Session, Subscriber, SubscriptionId, Sink};
use jsonrpc_ws_server::{RequestContext, ServerBuilder};

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

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
macro_rules! new_full_start {
    ($config:expr, $author:expr) => {{
        let inherent_data_providers = crate::service::kulupu_inherent_data_providers($author)?;

        let builder = substrate_service::ServiceBuilder::new_full::<
            kulupu_runtime::opaque::Block,
            kulupu_runtime::RuntimeApi,
            crate::service::Executor,
        >($config)?
        .with_select_chain(|_config, backend| {
            Ok(substrate_client::LongestChain::new(backend.clone()))
        })?
        .with_transaction_pool(|config, client| {
            Ok(transaction_pool::txpool::Pool::new(
                config,
                transaction_pool::FullChainApi::new(client),
            ))
        })?
        .with_import_queue(|_config, client, select_chain, _transaction_pool| {
            let import_queue = consensus_pow::import_queue(
                Box::new(client.clone()),
                client.clone(),
                kulupu_pow::RandomXAlgorithm::new(client.clone(), Sink::default()),
                0,
                select_chain,
                inherent_data_providers.clone(),
            )?;

            Ok(import_queue)
        })?;

        (builder, inherent_data_providers)
    }};
}

/// Builds a new service for a full client.
pub fn new_full<C: Send + Default + 'static>(
    config: Configuration<C, GenesisConfig>,
    author: Option<&str>,
    threads: usize,
    round: u32,
    miner_listen_port: u32,
) -> Result<impl AbstractService, ServiceError> {
    let is_authority = config.roles.is_authority();

    let (builder, inherent_data_providers) = new_full_start!(config, author);

    let service = builder
        .with_network_protocol(|_| Ok(NodeProtocol::new()))?
        .with_finality_proof_provider(|_client, _backend| Ok(Arc::new(()) as _))?
        .build()?;
    let web_sink = new_web_server(miner_listen_port).unwrap();
    if is_authority {
        for _ in 0..threads {
            let proposer = basic_authorship::ProposerFactory {
                client: service.client(),
                transaction_pool: service.transaction_pool(),
            };

            consensus_pow::start_mine(
                Box::new(service.client().clone()),
                service.client(),
                kulupu_pow::RandomXAlgorithm::new(service.client(), web_sink),
                proposer,
                None,
                round,
                service.network(),
                std::time::Duration::new(2, 0),
                service.select_chain().map(|v| v.clone()),
                inherent_data_providers.clone(),
            );
        }
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

fn new_web_server(port: u32) ->Result<Sink, &'static str>{
    let mut sink_params;
    let mut sink_seal;

    let mut io = PubSubHandler::new(MetaIoHandler::default());
    io.add_method("add_method mine_params", |_params: Params| {
        Ok(Value::String("hello".to_string()))
    });
    io.add_subscription(
        "mine_params",
        (
            "sub_get_mine_params",
            move |params: Params, _, subscriber: Subscriber| {
                println!("new_miner_server mine_params");

                if params != Params::None {
                    subscriber
                        .reject(Error {
                            code: ErrorCode::ParseError,
                            message: "Invalid parameters. Subscription rejected.".into(),
                            data: None,
                        })
                        .unwrap();
                    return;
                }
                sink_params = subscriber
                        .assign_id_async(SubscriptionId::Number(5))
                        .wait()
                        .unwrap();
                    println!("new_miner_server notify");

            },
        ),
        ("remove_get_mine_params", |_id: SubscriptionId, _| {
            println!("Closing subscription");
            futures::future::ok(Value::Bool(true))
        }),
    );
    io.add_subscription(
        "raw_seal",
        (
            "pub_raw_seal",
            move |params: Params, _, subscriber: Subscriber| {
                println!("raw_seal:{:?}", params);
                 sink_seal = subscriber
                        .assign_id_async(SubscriptionId::Number(5))
                        .wait()
                        .unwrap();
            },
        ),
        ("remove_get_info", |_id: SubscriptionId, _| {
            println!("Closing subscription");
            futures::future::ok(Value::Bool(true))
        }),
    );
    let server = ServerBuilder::with_meta_extractor(io, |context: &RequestContext| {
        Arc::new(Session::new(context.sender()))
    })
        .start(&"127.0.0.1:8011".parse().unwrap())
        .expect("Unable to start RPC server");
    thread::spawn(move || {
        server.wait()
    });
    Ok(sink_params)
}