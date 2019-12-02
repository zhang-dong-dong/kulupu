//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use codec::Encode;
use kulupu_runtime::{self, opaque::Block, AccountId, GenesisConfig, RuntimeApi};
use network::{config::DummyFinalityProofRequestBuilder, construct_simple_protocol};
use primitives::H256;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::net::SocketAddr;
use substrate_client::LongestChain;
use substrate_executor::native_executor_instance;
pub use substrate_executor::NativeExecutor;
use substrate_service::{
    error::Error as ServiceError, AbstractService, Configuration,
};
use transaction_pool::{self, txpool::Pool as TransactionPool};
use std::{thread, time::Duration};
use jsonrpc_core::{
    futures, futures::future::Future, MetaIoHandler, Params, Value, futures::stream::Stream
};
use jsonrpc_pubsub::{PubSubHandler, Session, Subscriber, SubscriptionId};
use jsonrpc_ws_server::{RequestContext, ServerBuilder};
use std::sync::{Arc, Mutex, mpsc::{Sender, Receiver, channel}};
use hex;

pub const MINE_PARAMS: &str = "mine_params";
pub const SUB_GET_MINE_PARAMS: &str = "sub_get_mine_params";
pub const RAWSEAL_METHOD: &str = "raw_seal";
pub const PUB_RAW_SEAL: &str = "pub_raw_seal";

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
//macro_rules! new_full_start {
//    ($config:expr, $author:expr) => {{
//        let inherent_data_providers = crate::service::kulupu_inherent_data_providers($author)?;
//
//        let builder = substrate_service::ServiceBuilder::new_full::<
//            kulupu_runtime::opaque::Block,
//            kulupu_runtime::RuntimeApi,
//            crate::service::Executor,
//        >($config)?
//        .with_select_chain(|_config, backend| {
//            Ok(substrate_client::LongestChain::new(backend.clone()))
//        })?
//        .with_transaction_pool(|config, client| {
//            Ok(transaction_pool::txpool::Pool::new(
//                config,
//                transaction_pool::FullChainApi::new(client),
//            ))
//        })?
//        .with_import_queue(|_config, client, select_chain, _transaction_pool| {
//            let import_queue = consensus_pow::import_queue(
//                Box::new(client.clone()),
//                client.clone(),
//                kulupu_pow::RandomXAlgorithm::new(client.clone(), {"".to_string(),["".to_string()]}, {0}),
//                0,
//                select_chain,
//                inherent_data_providers.clone(),
//            )?;
//
//            Ok(import_queue)
//        })?;
//
//        (builder, inherent_data_providers)
//    }};
//}

/// Builds a new service for a full client.
pub fn new_full<C: Send + Default + 'static>(
    config: Configuration<C, GenesisConfig>,
    author: Option<&str>,
    threads: usize,
    round: u32,
    miner_listen_port: u32,
) -> Result<impl AbstractService, ServiceError> {
    let is_authority = config.roles.is_authority();
    let inherent_data_providers = crate::service::kulupu_inherent_data_providers(author)?;
    let (tx1, rx1)= channel();
    let (tx1, rx1) = (Arc::new(Mutex::new(tx1)), Arc::new(Mutex::new(rx1)));

    let (tx2, rx2)= channel();
    let (tx2, rx2) = (Arc::new(Mutex::new(tx2)), Arc::new(Mutex::new(rx2)));

    let (tx, rx)= channel();
    let (rx_init, tx_init) = (Arc::new(Mutex::new(rx)), Arc::new(Mutex::new(tx)));

    let builder = substrate_service::ServiceBuilder::new_full::<
            kulupu_runtime::opaque::Block,
            kulupu_runtime::RuntimeApi,
            crate::service::Executor,
        >(config)?
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
                kulupu_pow::RandomXAlgorithm::new(client.clone(), tx_init, rx_init),
                0,
                select_chain,
                inherent_data_providers.clone(),
            )?;

            Ok(import_queue)
        })?;

    let service = builder
        .with_network_protocol(|_| Ok(NodeProtocol::new()))?
        .with_finality_proof_provider(|_client, _backend| Ok(Arc::new(()) as _))?
        .build()?;
    new_web_server(miner_listen_port, tx2, rx1);
    if is_authority {

        let proposer = basic_authorship::ProposerFactory {
            client: service.client(),
            transaction_pool: service.transaction_pool(),
        };

        consensus_pow::start_mine(
            Box::new(service.client().clone()),
            service.client(),
            kulupu_pow::RandomXAlgorithm::new(service.client(), tx1.clone(), rx2),
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

fn new_web_server(port: u32, tx_seal: Arc<Mutex<Sender<String>>>, rx_params: Arc<Mutex<Receiver<String>>>) {
    let mut io = PubSubHandler::new(MetaIoHandler::default());
    io.add_subscription(
        MINE_PARAMS,
        (
            SUB_GET_MINE_PARAMS,
            move |params: Params, _, subscriber: Subscriber| {
                let rx_params = rx_params.clone();
                let th = thread::spawn(move || {
                    let sink = subscriber
                        .assign_id_async(SubscriptionId::Number(1))
                        .wait()
                        .unwrap();
                    loop {
                        if let Ok(rx_params) = rx_params.try_lock() {
                            match rx_params.recv_timeout(Duration::from_secs(60)) {
                                Ok(result) => {
                                    drop(rx_params);
                                    let params = serde_json::from_str(result.as_str()).unwrap();
                                    match sink.notify(Params::Map(params)).wait() {
                                        Ok(_) => {}
                                        Err(_) => {
                                            break;
                                        }
                                    }
                                }
                                Err(_) => {
                                    drop(rx_params);
                                }
                            }
                        }
                    }
                });
            },
        ),
        ("unsub_params", |_id: SubscriptionId, _| {
            futures::future::ok(Value::Bool(true))
        }),
    );
    let tx_seal = tx_seal.clone();
    io.add_subscription(
        RAWSEAL_METHOD,
        (
            PUB_RAW_SEAL,
            move |params: Params, _, _subscriber: Subscriber| {
                match params.parse::<Vec<u8>>() {
                    Ok(s) => {
                        tx_seal.lock().unwrap().send(hex::encode(s)).unwrap();
                    }
                    Err(_) => {
                        tx_seal.lock().unwrap().send(hex::encode(vec![0])).unwrap();
                    }

                }
            },
        ),
        ("unsub_seal", |_id: SubscriptionId, _| {
            futures::future::ok(Value::Bool(true))
        }),
    );
    let server = ServerBuilder::with_meta_extractor(io, |context: &RequestContext| {
        Arc::new(Session::new(context.sender()))
    })
        .start(&format!("127.0.0.1:{:}", port).as_str().parse::<SocketAddr>().unwrap())
        .expect("unable to start WS server");
    thread::spawn(move || {
        server.wait()
    });
}