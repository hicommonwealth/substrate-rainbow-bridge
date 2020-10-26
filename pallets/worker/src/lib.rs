#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::Parameter;
use sp_runtime::traits::AtLeast32Bit;
use sp_std::prelude::*;
use codec::{Encode, Decode};
use frame_system::{
	self as system, ensure_signed,
	offchain::{
		AppCrypto, CreateSignedTransaction,
	}
};
use frame_support::{
	debug, decl_module, decl_storage, decl_event, ensure,
	traits::Get,
};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
	transaction_validity::{
		ValidTransaction, TransactionValidity, TransactionSource,
		TransactionPriority,
	},
	offchain::{http},
};

use lite_json::json::JsonValue;

use ethereum_types::{H128, U256};
use sp_core::{H256};

// pub mod eth;

#[cfg(test)]
mod tests;

/// Defines application identifier for crypto keys of this module.
///
/// Every module that deals with signatures needs to declare its unique identifier for
/// its crypto keys.
/// When offchain worker is signing transactions it's going to request keys of type
/// `KeyTypeId` from the keystore and use the ones it finds to sign the transaction.
/// The keys can be inserted manually via RPC (see `author_insertKey`).
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"btc!");

/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrappers.
/// We can use from supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
/// the types with this pallet-specific identifier.
pub mod crypto {
	use super::KEY_TYPE;
	use sp_runtime::{
		app_crypto::{app_crypto, sr25519},
		traits::Verify,
	};
	use sp_core::sr25519::Signature as Sr25519Signature;
	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}

/// This pallet's configuration trait
pub trait Trait: CreateSignedTransaction<Call<Self>> {
	/// The identifier type for an offchain worker.
	type AuthorityId: AppCrypto<Self::Public, Self::Signature>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
	/// The overarching dispatch call type.
	type Call: From<Call<Self>>;

	// Configuration parameters

	/// A grace period after we send transaction.
	///
	/// To avoid sending too many transactions, we only attempt to send one
	/// every `GRACE_PERIOD` blocks. We use Local Storage to coordinate
	/// sending between distinct runs of this offchain worker.
	type GracePeriod: Get<Self::BlockNumber>;

	/// Number of blocks of cooldown after unsigned transaction is included.
	///
	/// This ensures that we only accept unsigned transactions once, every `UnsignedInterval` blocks.
	type UnsignedInterval: Get<Self::BlockNumber>;

	/// A configuration for base priority of unsigned transactions.
	///
	/// This is exposed so that it can be tuned for particular runtime, when
	/// multiple pallets send unsigned transactions.
	type UnsignedPriority: Get<TransactionPriority>;
	/// Threshold type for storage items
	type Threshold: Parameter + AtLeast32Bit + Default + Copy;
}

/// Minimal information about a header.
#[derive(Encode, Decode)]
pub struct HeaderInfo {
	pub total_difficulty: U256,
	pub parent_hash: H256,
	pub number: U256,
}

decl_storage! {
	trait Store for Module<T: Trait> as WorkerModule {
		/// The epoch from which the DAG merkle roots start.
		pub DAGsStartEpoch get(fn dags_start_epoch): Option<u64>;
		/// DAG merkle roots for the next several years.
		pub DAGsMerkleRoots get(fn dags_merkle_roots): Vec<H128>;
		/// Hash of the header that has the highest cumulative difficulty. The current head of the
		/// canonical chain.
		pub BestHeaderHash get(fn best_header_hash): H256;
		/// We store the hashes of the blocks for the past `hashes_gc_threshold` headers.
		/// Events that happen past this threshold cannot be verified by the client.
		/// It is desirable that this number is larger than 7 days worth of headers, which is roughly
		/// 40k Ethereum blocks. So this number should be 40k in production.
		pub HashesGCThreshold get(fn hashes_gc_threshold): Option<T::Threshold>;
		/// We store full information about the headers for the past `finalized_gc_threshold` blocks.
		/// This is required to be able to adjust the canonical chain when the fork switch happens.
		/// The commonly used number is 500 blocks, so this number should be 500 in production.
		pub FinalizedGCThreshold get(fn finalized_gc_threshold): Option<T::Threshold>;
		/// Number of confirmations that applications can use to consider the transaction safe.
		/// For most use cases 25 should be enough, for super safe cases it should be 500.
		pub NumConfirmations get(fn num_confirmations): Option<T::Threshold>;
		/// Hashes of the canonical chain mapped to their numbers. Stores up to `hashes_gc_threshold`
		/// entries.
		/// header number -> header hash
		pub CanonicalHeaderHashes get(fn canonical_header_hashes): map hasher(twox_64_concat) U256 => H256;
		/// All known header hashes. Stores up to `finalized_gc_threshold`.
		/// header number -> hashes of all headers with this number.
		pub AllHeaderHashes get(fn all_header_hashes): map hasher(twox_64_concat) U256 => Vec<H256>;
		/// Known headers. Stores up to `finalized_gc_threshold`.
		pub Headers get(fn headers): map hasher(twox_64_concat) H256 => Option<ethereum::Header>;
		/// Minimal information about the headers, like cumulative difficulty. Stores up to
		/// `finalized_gc_threshold`.
		pub Infos get(fn infos): map hasher(twox_64_concat) H256 => Option<HeaderInfo>;
		/// If set, block header added by trusted signer will skip validation and added by
		/// others will be immediately rejected, used in PoA testnets
		pub TrustedSigner get(fn trusted_signer): Option<T::AccountId>;
	}
}

decl_event!(
	pub enum Event<T> where AccountId = <T as frame_system::Trait>::AccountId {
		NewHeader(u32, AccountId),
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		// // Errors must be initialized if they are used by the pallet.
		// type Error = Error<T>;

		// Events must be initialized if they are used by the pallet.
		fn deposit_event() = default;

		#[weight = 0]
		fn init(
			origin,
			dags_start_epoch: u64,
			dags_merkle_roots: Vec<H128>,
			first_header: Vec<u8>,
			hashes_gc_threshold: T::Threshold,
			finalized_gc_threshold: T::Threshold,
			num_confirmations: T::Threshold,
			trusted_signer: Option<T::AccountId>,
		) {
			let _signer = ensure_signed(origin)?;
			ensure!(Self::dags_start_epoch().is_none(), "Already initialized");
			ensure!(Self::hashes_gc_threshold().is_none(), "Already initialized");
			ensure!(Self::finalized_gc_threshold().is_none(), "Already initialized");

			<DAGsStartEpoch>::set(Some(dags_start_epoch));
			<DAGsMerkleRoots>::set(dags_merkle_roots);
			<HashesGCThreshold<T>>::set(Some(hashes_gc_threshold));
			<FinalizedGCThreshold<T>>::set(Some(finalized_gc_threshold));
			<NumConfirmations<T>>::set(Some(num_confirmations));
			<TrustedSigner<T>>::set(trusted_signer);

			let header: ethereum::Header = rlp::decode(first_header.as_slice()).unwrap();
			let header_hash = header.hash();
			let header_number = header.number;

			<BestHeaderHash>::set(header_hash.clone());
			<AllHeaderHashes>::insert(header_number, vec![header_hash]);
			<CanonicalHeaderHashes>::insert(header_number, header_hash);
			<Headers>::insert(header_hash, header.clone());
			<Infos>::insert(header_hash, HeaderInfo {
				total_difficulty: header.difficulty,
				parent_hash: header.parent_hash,
				number: header.number,
			});
		}

		/// Offchain Worker entry point.
		///
		/// By implementing `fn offchain_worker` within `decl_module!` you declare a new offchain
		/// worker.
		/// This function will be called when the node is fully synced and a new best block is
		/// succesfuly imported.
		/// Note that it's not guaranteed for offchain workers to run on EVERY block, there might
		/// be cases where some blocks are skipped, or for some the worker runs twice (re-orgs),
		/// so the code should be able to handle that.
		/// You can use `Local Storage` API to coordinate runs of the worker.
		fn offchain_worker(block_number: T::BlockNumber) {
			// It's a good idea to add logs to your offchain workers.
			// Using the `frame_support::debug` module you have access to the same API exposed by
			// the `log` crate.
			// Note that having logs compiled to WASM may cause the size of the blob to increase
			// significantly. You can use `RuntimeDebug` custom derive to hide details of the types
			// in WASM or use `debug::native` namespace to produce logs only when the worker is
			// running natively.
			debug::native::info!("Hello World from offchain workers!");

			// Since off-chain workers are just part of the runtime code, they have direct access
			// to the storage and other included pallets.
			//
			// We can easily import `frame_system` and retrieve a block hash of the parent block.
			let parent_hash = <system::Module<T>>::block_hash(block_number - 1.into());
			debug::debug!("Current block: {:?} (parent hash: {:?})", block_number, parent_hash);
			let number = Self::fetch_block().unwrap();
			debug::info!("{:?}", number);
		}
	}
}

fn hex_to_bytes(v: &[char]) -> Result<Vec<u8>, hex::FromHexError> {
	let v_no_prefix = if v.len() >= 2 && v[0] == '0' && v[1] == 'x' {
		&v[2..]
	} else {
		&v[..]
	};
	let v_u8 = v_no_prefix.iter().map(|c| *c as u8).collect::<Vec<u8>>();
	hex::decode(&v_u8[..])
}

impl<T: Trait> Module<T> {
    pub fn initialized() -> bool {
		Self::dags_start_epoch().is_some()
    }

    pub fn dag_merkle_root(epoch: u64) -> H128 {
    	match Self::dags_start_epoch() {
    		Some(ep) => Self::dags_merkle_roots()[(epoch - ep) as usize],
    		None => H128::zero(),
    	}
    	
    }

    pub fn last_block_number(&self) -> U256 {
    	match Self::infos(Self::best_header_hash()) {
    		Some(header) => header.number,
    		None => U256::zero(),
    	}
    }

    // /// Returns the block hash from the canonical chain.
    // pub fn block_hash(index: u64) -> Option<H256> {
    //     self.canonical_header_hashes.get(&index)
    // }

    // /// Returns all hashes known for that height.
    // pub fn known_hashes(index: u64) -> Vec<H256> {
    //     self.all_header_hashes.get(&index).unwrap_or_default()
    // }

    // /// Returns block hash and the number of confirmations.
    // pub fn block_hash_safe(&self, #[serializer(borsh)] index: u64) -> Option<H256> {
    //     let header_hash = self.block_hash(index)?;
    //     let last_block_number = self.last_block_number();
    //     if index + self.num_confirmations > last_block_number {
    //         None
    //     } else {
    //         Some(header_hash)
    //     }
    // }

	fn fetch_block() -> Result<u32, http::Error> {
		// Make a post request to an eth chain
		let body = br#"{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["latest", false],"id":1}"#;
		let request: http::Request = http::Request::post(
			"http://localhost:8545",
			[ &body[..] ].to_vec(),
		);
		let pending = request.send().unwrap();

		// wait indefinitely for response (TODO: timeout)
		let mut response = pending.wait().unwrap();
		let headers = response.headers().into_iter();
		assert_eq!(headers.current(), None);

		// and collect the body
		let body = response.body().collect::<Vec<u8>>();
		let body_str = sp_std::str::from_utf8(&body).map_err(|_| {
			debug::warn!("No UTF8 body");
			http::Error::Unknown
		}).unwrap();
		// decode JSON into object
		// println!("{:?}", body_str);
		let val = lite_json::parse_json(&body_str).unwrap();

		// get { "result": VAL }
		let block = match val {
			JsonValue::Object(obj) => {
				obj.into_iter()
					.find(|(k, _)| k.iter().map(|c| *c as u8).collect::<Vec<u8>>() == b"result".to_vec())
					.and_then(|v| {
						match v.1 {
							JsonValue::Object(block) => Some(block),
							_ => None,
						}
					})
			},
			_ => None
		};

		// get { "number": VAL } and convert from hex string -> decimal
		let number_hex: Vec<char> = block.unwrap().into_iter()
			.find(|(k, _)| k.iter().map(|c| *c as u8).collect::<Vec<u8>>() == b"number")
			.and_then(|v| match v.1 {
				JsonValue::String(n) => Some(n),
				_ => None,
			})
			.unwrap();

		let decoded_vec = hex_to_bytes(&number_hex[..]).unwrap();
		Ok(U256::from_big_endian(&decoded_vec[..]).low_u32())
	}
}

#[allow(deprecated)] // ValidateUnsigned
impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	/// Validate unsigned call to this module.
	///
	/// By default unsigned transactions are disallowed, but implementing the validator
	/// here we make sure that some particular calls (the ones produced by offchain worker)
	/// are being whitelisted and marked as valid.
	fn validate_unsigned(
		_source: TransactionSource,
		_call: &Self::Call,
	) -> TransactionValidity {
		ValidTransaction::with_tag_prefix("ExampleOffchainWorker")
		// We set base priority to 2**20 and hope it's included before any other
		// transactions in the pool. Next we tweak the priority depending on how much
		// it differs from the current average. (the more it differs the more priority it
		// has).
		.priority(T::UnsignedPriority::get())
		// The transaction is only valid for next 5 blocks. After that it's
		// going to be revalidated by the pool.
		.longevity(5)
		// It's fine to propagate that transaction to other peers, which means it can be
		// created even by nodes that don't produce blocks.
		// Note that sometimes it's better to keep it for yourself (if you are the block
		// producer), since for instance in some schemes others may copy your solution and
		// claim a reward.
		.propagate(true)
		.build()
	}
}