// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Substrate blockchain API.

pub mod error;
pub mod number;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use self::error::Result;
use jsonrpc_core::futures::{stream, Future, Sink, Stream};
use jsonrpc_core::Result as RpcResult;
use crate::subscriptions::Subscriptions;
use client::{self, BlockchainEvents, Client};
use jsonrpc_derive::rpc;
use jsonrpc_pubsub::{typed::Subscriber, SubscriptionId};
use log::warn;
use primitives::{Blake2Hasher, H256};
use runtime_primitives::generic::{BlockId, SignedBlock};
use runtime_primitives::traits::{Block as BlockT, Header, NumberFor};


/// Substrate blockchain API
#[rpc]
pub trait MineApi<Header> {
    /// RPC metadata
    type Metadata;

    /// New head subscription
    #[pubsub(
        subscription = "mine_newParams",
        subscribe,
        name = "mine_SubscribeNewParams",
        alias("Subscribe_newParams")
    )]
    fn subscribe_new_params(&self, metadata: Option<Self::Metadata>, subscriber: Subscriber<Header>);

    /// Unsubscribe from new head subscription.
    #[pubsub(
        subscription = "mine_newParams",
        unsubscribe,
        name = "mine_unsubscribeNewParams",
        alias("unsubscribe_newParams")
    )]
    fn unsubscribe_new_params(
        &self,
        metadata: Option<Self::Metadata>,
        id: SubscriptionId,
    ) -> RpcResult<bool>;
}

/// Chain API with subscriptions support.
pub struct Mine<B, E, Block: BlockT, RA> {
    /// Substrate client.
    client: Arc<Client<B, E, Block, RA>>,
    /// Current subscriptions.
    subscriptions: Subscriptions,
}

impl<B, E, Block: BlockT, RA> Mine<B, E, Block, RA> {
    /// Create new Chain API RPC handler.
    pub fn new(client: Arc<Client<B, E, Block, RA>>, subscriptions: Subscriptions) -> Self {
        Self {
            client,
            subscriptions,
        }
    }
}

impl<B, E, Block, RA> Mine<B, E, Block, RA>
where
    Block: BlockT<Hash = H256> + 'static,
    B: client::backend::Backend<Block, Blake2Hasher> + Send + Sync + 'static,
    E: client::CallExecutor<Block, Blake2Hasher> + Send + Sync + 'static,
    RA: Send + Sync + 'static,
{
    fn unwrap_or_best(&self, hash: Option<Block::Hash>) -> Result<Block::Hash> {
        Ok(match hash.into() {
            None => self.client.info().chain.best_hash,
            Some(hash) => hash,
        })
    }

    fn subscribe_params<F, G, S, ERR>(
        &self,
        subscriber: Subscriber<Block::Header>,
        stream: F,
    ) where
        F: FnOnce() -> S,
        G: FnOnce() -> Result<Option<Block::Hash>>,
        ERR: ::std::fmt::Debug,
        S: Stream<Item = Block::Header, Error = ERR> + Send + 'static,
    {
        self.subscriptions.add(subscriber, |sink| {
            let difficulty = self.client.runtime_api().difficulty(parent)
                .map_err(|e| format!("Fetching difficulty from runtime failed: {:?}", e));

            // send current head right at the start.
            let header = best_block_hash()
                .and_then(|hash| self.header(hash.into()))
                .and_then(|header| header.ok_or_else(|| "Best header missing.".to_owned().into()))
                .map_err(Into::into);

            // send further subscriptions
            let stream = stream()
                .map(|res| Ok(res))
                .map_err(|e| warn!("Block notification stream error: {:?}", e));

            sink
                .sink_map_err(|e| warn!("Error sending notifications: {:?}", e))
                .send_all(
                    stream::iter_result(vec![Ok(header)])
                        .chain(stream)
                )
                // we ignore the resulting Stream (if the first stream is over we are unsubscribed)
                .map(|_| ())
        });
    }
}

impl<B, E, Block, RA> MineApi<Block::Header> for Mine<B, E, Block, RA>
where
    Block: BlockT<Hash = H256> + 'static,
    B: client::backend::Backend<Block, Blake2Hasher> + Send + Sync + 'static,
    E: client::CallExecutor<Block, Blake2Hasher> + Send + Sync + 'static,
    RA: Send + Sync + 'static,
{
    type Metadata = crate::metadata::Metadata;

    fn subscribe_new_params(&self, _metadata: Option<Self::Metadata>, subscriber: Subscriber<Block::Header>) {
        self.subscribe_params(
            subscriber,
            || self.block_hash(None.into())
        )
    }

    fn unsubscribe_new_params(
        &self,
        _metadata: Option<Self::Metadata>,
        id: SubscriptionId,
    ) -> RpcResult<bool> {
        Ok(self.subscriptions.cancel(id))
    }
}
