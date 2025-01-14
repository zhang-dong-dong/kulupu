use codec::{Decode, Encode};
use std::cell::RefCell;
use consensus_pow::{Error, PowAlgorithm};
use consensus_pow_primitives::{DifficultyApi, Seal as RawSeal};
use kulupu_primitives::{AlgorithmApi, Difficulty, DAY_HEIGHT, HOUR_HEIGHT};
use log::*;
use lru_cache::LruCache;
use primitives::{H256, U256};
use rand::{rngs::SmallRng, thread_rng, SeedableRng};
use sr_primitives::generic::BlockId;
use sr_primitives::traits::{
    Block as BlockT, Header as HeaderT, ProvideRuntimeApi, UniqueSaturatedInto,
};
use std::time::Duration;
use std::sync::{Arc, Mutex, mpsc::{Sender, Receiver}};
use serde::Serialize;
use client_api::{blockchain::HeaderBackend, backend::AuxStore};

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct Seal {
    pub difficulty: Difficulty,
    pub work: H256,
    pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct Calculation {
    pub difficulty: Difficulty,
    pub pre_hash: H256,
    pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Compute {
    pub key_hash: H256,
    pub pre_hash: H256,
    pub difficulty: Difficulty,
    pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct MineParams {
    pub keyhash: H256,
    pub prehash: H256,
    pub difficulty: Difficulty,
    pub round: u32,
}

thread_local!(static MACHINES: RefCell<LruCache<H256, randomx::FullVM>> = RefCell::new(LruCache::new(3)));

impl Compute {
    pub fn compute(self) -> Seal {
        MACHINES.with(|m| {
            let mut ms = m.borrow_mut();
            let calculation = Calculation {
                difficulty: self.difficulty,
                pre_hash: self.pre_hash,
                nonce: self.nonce,
            };

            let work = if let Some(vm) = ms.get_mut(&self.key_hash) {
                vm.calculate(&calculation.encode()[..])
            } else {
                let mut vm = randomx::FullVM::new(&self.key_hash[..]);
                let work = vm.calculate(&calculation.encode()[..]);
                ms.insert(self.key_hash, vm);
                work
            };

            Seal {
                nonce: self.nonce,
                difficulty: self.difficulty,
                work: H256::from(work),
            }
        })
    }
}

fn is_valid_hash(hash: &H256, difficulty: Difficulty) -> bool {
    let num_hash = U256::from(&hash[..]);
    let (_, overflowed) = num_hash.overflowing_mul(difficulty);

    !overflowed
}

fn key_hash<B, C>(
	client: &C,
	parent: &BlockId<B>
) -> Result<H256, consensus_pow::Error<B>> where
	B: BlockT<Hash=H256>,
	C: HeaderBackend<B>,
{
    const PERIOD: u64 = 2 * DAY_HEIGHT;
    const OFFSET: u64 = 2 * HOUR_HEIGHT;

	let parent_header = client.header(parent.clone())
		.map_err(|e| consensus_pow::Error::Environment(
			format!("Client execution error: {:?}", e)
		))?
		.ok_or(consensus_pow::Error::Environment(
			"Parent header not found".to_string()
		))?;
	let parent_number = UniqueSaturatedInto::<u64>::unique_saturated_into(*parent_header.number());

    let mut key_number = parent_number.saturating_sub(parent_number % PERIOD);
    if parent_number.saturating_sub(key_number) < OFFSET {
        key_number = key_number.saturating_sub(PERIOD);
    }

	let mut current = parent_header;
	while UniqueSaturatedInto::<u64>::unique_saturated_into(*current.number()) != key_number {
		current = client.header(BlockId::Hash(*current.parent_hash()))
			.map_err(|e| consensus_pow::Error::Environment(
				format!("Client execution error: {:?}", e)
			))?
			.ok_or(consensus_pow::Error::Environment(
				format!("Block with hash {:?} not found", current.hash())
			))?;
	}

    Ok(current.hash())
}

pub struct RandomXAlgorithm<C> {
    client: Arc<C>,
    tx1: Arc<Mutex<Sender<String>>>,
    rx2: Arc<Mutex<Receiver<String>>>,
}

impl<C> RandomXAlgorithm<C> {
    pub fn new(client: Arc<C>, tx1: Arc<Mutex<Sender<String>>>, rx2: Arc<Mutex<Receiver<String>>>) -> Self {
        Self {client, tx1, rx2}
    }
}

impl<B: BlockT<Hash = H256>, C> PowAlgorithm<B> for RandomXAlgorithm<C>
where
    C: HeaderBackend<B> + AuxStore + ProvideRuntimeApi,
    C::Api: DifficultyApi<B, Difficulty> + AlgorithmApi<B>,
{
    type Difficulty = Difficulty;

	fn difficulty(&self, parent: &BlockId<B>) -> Result<Difficulty, consensus_pow::Error<B>> {
		let difficulty = self.client.runtime_api().difficulty(parent)
			.map_err(|e| consensus_pow::Error::Environment(
				format!("Fetching difficulty from runtime failed: {:?}", e)
			));

        info!("Next block's difficulty: {:?}", difficulty);
        difficulty
    }

	fn verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seal: &RawSeal,
		difficulty: Difficulty,
	) -> Result<bool, consensus_pow::Error<B>> {
		assert_eq!(self.client.runtime_api().identifier(parent)
				   .map_err(|e| consensus_pow::Error::Environment(
					   format!("Fetching identifier from runtime failed: {:?}", e))
				   )?,
				   kulupu_primitives::ALGORITHM_IDENTIFIER);

        let key_hash = key_hash(self.client.as_ref(), parent)?;

        let seal = match Seal::decode(&mut &seal[..]) {
            Ok(seal) => seal,
            Err(_) => return Ok(false),
        };

        if !is_valid_hash(&seal.work, difficulty) {
            return Ok(false);
        }

        let compute = Compute {
            key_hash,
            difficulty,
            pre_hash: *pre_hash,
            nonce: seal.nonce,
        };

        if compute.compute() != seal {
            return Ok(false);
        }

        Ok(true)
    }

    fn mine(
        &self,
        parent: &BlockId<B>,
        pre_hash: &H256,
        difficulty: Difficulty,
        round: u32,
    ) -> Result<Option<RawSeal>, Error<B>> {
        let key_hash = key_hash(self.client.as_ref(), parent)?;
        let params = MineParams {
            keyhash: key_hash,
            prehash: *pre_hash,
            difficulty: difficulty,
            round: round,
        };
        match serde_json::to_string(&params) {
            Ok(p) => {
                match self.tx1.lock().unwrap().send(p.clone()) {
                    Ok(_) => {
                        info!("mine-params :{:?}", p)
                    }
                    Err(e) => {
                        warn!("channel send: {}", e);
                        return Ok(None)
                    }
                }
            }
            Err(e) => {
                return Ok(None)
            }
        }
        loop {
            let result = self.rx2.lock().unwrap().recv_timeout(Duration::from_secs(1000));
            match result {
                Ok(res) => {
                    if let Ok(v) = hex::decode(res) {
                        if v.len() == 1 {
                            return Ok(None)
                        }
                        info!("recv seal {:?}", v.clone());
                        return Ok(Some(v))
                    } else {
                        return Ok(None)
                    }
                }
                Err(e) => {
                    warn!("recv seal {:?}", e);
                    return Ok(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kulupu_primitives::{H256, U256};

    #[test]
    fn randomx_len() {
        assert_eq!(randomx::HASH_SIZE, 32);
    }

    #[test]
    fn randomx_collision() {
        let mut compute = Compute {
            key_hash: H256::from([
                210, 164, 216, 149, 3, 68, 116, 1, 239, 110, 111, 48, 180, 102, 53, 180, 91, 84,
                242, 90, 101, 12, 71, 70, 75, 83, 17, 249, 214, 253, 71, 89,
            ]),
            pre_hash: H256::default(),
            difficulty: U256::default(),
            nonce: H256::default(),
        };
        let hash1 = compute.clone().compute();
        U256::one().to_big_endian(&mut compute.nonce[..]);
        let hash2 = compute.compute();
        assert!(hash1 != hash2);
    }
}
