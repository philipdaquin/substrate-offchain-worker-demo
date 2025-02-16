#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	//! A demonstration of an offchain worker that sends onchain callbacks
	use core::{convert::TryInto};
	use parity_scale_codec::{Decode, Encode};
	use frame_support::pallet_prelude::*;
	use frame_system::{
		pallet_prelude::*,
		offchain::{
			AppCrypto, CreateSignedTransaction, SendSignedTransaction, SendUnsignedTransaction,
			SignedPayload, Signer, SigningTypes, SubmitTransaction,
		},
	};
	use sp_core::{crypto::KeyTypeId};
	use sp_runtime::{
		offchain::{
			http,
			storage::StorageValueRef,
			storage_lock::{BlockAndTime, StorageLock},
			Duration,
		},
		traits::BlockNumberProvider,
		transaction_validity::{
			InvalidTransaction, TransactionSource, TransactionValidity, ValidTransaction,
		},
		RuntimeDebug,
	};
	use sp_std::{collections::vec_deque::VecDeque, prelude::*, str};

	use serde::{Deserialize, Deserializer};
	
	use serde_json::{json, Value};

	/// Defines application identifier for crypto keys of this module.
	///
	/// Every module that deals with signatures needs to declare its unique identifier for
	/// its crypto keys.
	/// When an offchain worker is signing transactions it's going to request keys from type
	/// `KeyTypeId` via the keystore to sign the transaction.
	/// The keys can be inserted manually via RPC (see `author_insertKey`).
	pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"demo");
	const NUM_VEC_LEN: usize = 10;
	/// The type to sign and send transactions.
	const UNSIGNED_TXS_PRIORITY: u64 = 100;
//
	// We are fetching information from Hacker News public API
	const HTTP_REMOTE_REQUEST: &str = "https://api.coincap.io/v2/assets/polkadot";
	const FETCH_TIMEOUT_PERIOD: u64 = 4000; // in milli-seconds
	const LOCK_TIMEOUT_EXPIRATION: u64 = FETCH_TIMEOUT_PERIOD + 1000; // in milli-seconds
	const LOCK_BLOCK_EXPIRATION: u32 = 3; // in block number

	/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrapper.
	/// We can utilize the supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
	/// them with the pallet-specific identifier.
	pub mod crypto {
		use crate::KEY_TYPE;
		use sp_core::sr25519::Signature as Sr25519Signature;
		use sp_std::prelude::*;
		use sp_runtime::{
			app_crypto::{app_crypto, sr25519},
			traits::Verify, MultiSignature, MultiSigner
		};

		app_crypto!(sr25519, KEY_TYPE);

		pub struct TestAuthId;
		// implemented for runtime
		impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for TestAuthId {
			type RuntimeAppPublic = Public;
			type GenericSignature = sp_core::sr25519::Signature;
			type GenericPublic = sp_core::sr25519::Public;
		}

		// implemented for mock runtime in test
		impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature>
		for TestAuthId
		{
			type RuntimeAppPublic = Public;
			type GenericSignature = sp_core::sr25519::Signature;
			type GenericPublic = sp_core::sr25519::Public;
		}
	}

	#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, scale_info::TypeInfo)]
	pub struct Payload<Public> {
		price: u32,
		public: Public,
	}

	#[derive(Deserialize, Encode, Decode, Default)]
	pub struct Price { 
		#[serde(deserialize_with = "de_string_to_bytes")]
		price: Vec<u8>,	
	}

	impl<T: SigningTypes> SignedPayload<T> for Payload<T::Public> {
		fn public(&self) -> T::Public {
			self.public.clone()
		}
	}

	

	//	Get information from hackernews 
	// ref: https://serde.rs/container-attrs.html#crate
	#[derive(Deserialize, Encode, Decode, Default, RuntimeDebug, scale_info::TypeInfo)]
	struct HackerNewsInfo {
		// Specify our own deserializing function to convert JSON string to vector of bytes
		#[serde(deserialize_with = "de_string_to_bytes")]
		by: Vec<u8>,
		#[serde(deserialize_with = "de_string_to_bytes")]
		title: Vec<u8>,
		//#[serde(deserialize_with = "de_string_to_bytes")]
		url: Vec<u8>,
		descendants: u32,
	}

	//* Implement Deserializer */
	#[derive(Debug, Deserialize, Encode, Decode, Default)]
	struct IndexingData(Vec<u8>, u64);

	pub fn de_string_to_bytes<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
	where
	D: Deserializer<'de>,
	{
		let s: &str = Deserialize::deserialize(de)?;
		Ok(s.as_bytes().to_vec())
	}

	//* Pallet Types  */
	#[pallet::config]
	pub trait Config: frame_system::Config + CreateSignedTransaction<Call<Self>> {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// The overarching dispatch call type.
		type Call: From<Call<Self>>;
		/// The identifier type for an offchain worker.
		type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	// The pallet's runtime storage items.
	#[pallet::storage]
	#[pallet::getter(fn numbers)]
	pub type Numbers<T> = StorageValue<_, VecDeque<u32>, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		NewNumber(Option<T::AccountId>, u32),
	}

	// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		// Error returned when not sure which ocw function to executed
		UnknownOffchainMux,

		// Error returned when making signed transactions in off-chain worker
		NoLocalAcctForSigning,
		OffchainSignedTxError,

		// Error returned when making unsigned transactions in off-chain worker
		OffchainUnsignedTxError,

		// Error returned when making unsigned transactions with signed payloads in off-chain worker
		OffchainUnsignedTxSignedPayloadError,

		// Error returned when fetching github info
		HttpFetchingError,
		DeserializeToObjError,
		DeserializeToStrError,
		StringToJSONerr,
		ParseError,
	}
	//* Implement Offchain Workers */
	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		/// Offchain Worker entry point.
		///
		/// By implementing `fn offchain_worker` you declare a new offchain worker.
		/// This function will be called when the node is fully synced and a new best block is
		/// succesfuly imported.
		/// Note that it's not guaranteed for offchain workers to run on EVERY block, there might
		/// be cases where some blocks are skipped, or for some the worker runs twice (re-orgs),
		/// so the code should be able to handle that.
		/// You can use `Local Storage` API to coordinate runs of the worker.
		fn offchain_worker(block_number: T::BlockNumber) {

			log::info!("Hello from Offchain Worker!");

			//*  Here we are showcasing various techniques used when running off-chain workers (ocw) */
			//*  1. Sending signed transaction from ocw */
			//*  2. Sending unsigned transaction from ocw */
			//*  3. Sending unsigned transactions with signed payloads from ocw */
			//*  4. Fetching JSON via http requests in ocw           */
			// const TX_TYPES: u32 = 4;
			// let modu = block_number.try_into().map_or(TX_TYPES, |bn: usize| (bn as u32) % TX_TYPES);
			
			
			// let result = match modu {
			// 	0 => Self::offchain_signed_tx(block_number),
			// 	1 => Self::offchain_unsigned_tx(block_number),
			// 	2 => Self::offchain_unsigned_tx_signed_payload(block_number),
			// 	3 => Self::fetch_remote_info(),
			// 	_ => Err(Error::<T>::UnknownOffchainMux),
			// };

			
			let result = Self::offchain_unsigned_tx_signed_payload(block_number);

			if let Err(e) = result {
				log::error!("offchain_worker error: {:?}", e);
			}
		}
	}


	//* Validate Unsigned Calls To this Pallet */
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		/// Validate unsigned call to this module.
		///
		/// By default unsigned transactions are disallowed, but implementing the validator
		/// here we make sure that some particular calls (the ones produced by offchain worker)
		/// are being whitelisted and marked as valid.
		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			let valid_tx = |provide| ValidTransaction::with_tag_prefix("ocw-demo")
				.priority(UNSIGNED_TXS_PRIORITY)
				.and_provides([&provide])
				.longevity(3)
				.propagate(true)
				.build();

			match call {
				Call::submit_number_unsigned { number: _number } => valid_tx(b"submit_number_unsigned".to_vec()),
				Call::submit_number_unsigned_with_signed_payload {
					ref payload,
					ref signature
				} => {
					if !SignedPayload::<T>::verify::<T::AuthorityId>(payload, signature.clone()) {
						return InvalidTransaction::BadProof.into();
					}
					valid_tx(b"submit_number_unsigned_with_signed_payload".to_vec())
				},
				_ => InvalidTransaction::Call.into(),
			}
		}
	}

	//* Pallet Calls */
	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(10000)]
		pub fn submit_number_signed(origin: OriginFor<T>, number: u32) -> DispatchResult {
			let who = ensure_signed(origin)?;
			log::info!("submit_number_signed: ({}, {:?})", number, who);
			//	FIFO values inside the VecDeque
			Self::append_or_replace_number(number);

			Self::deposit_event(Event::NewNumber(Some(who), number));
			Ok(())
		}

		#[pallet::weight(10000)]
		pub fn submit_number_unsigned(origin: OriginFor<T>, number: u32) -> DispatchResult {
			let _ = ensure_none(origin)?;
			log::info!("submit_number_unsigned: {}", number);
			Self::append_or_replace_number(number);

			Self::deposit_event(Event::NewNumber(None, number));
			Ok(())
		}

		#[pallet::weight(10000)]
		#[allow(unused_variables)]
		pub fn submit_number_unsigned_with_signed_payload(
			origin: OriginFor<T>,
			payload: Payload<T::Public>,
			signature: T::Signature) -> DispatchResult
		{
			let _ = ensure_none(origin)?;
			// we don't need to verify the signature here because it has been verified in
			//   `validate_unsigned` function when sending out the unsigned tx.
			let Payload { price , public } = payload;
			log::info!("submit_number_unsigned_with_signed_payload: ({}, {:?})", price, public);
			Self::append_or_replace_number(price);

			Self::deposit_event(Event::NewNumber(None, price));
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Append a new number to the tail of the list, removing an element from the head if reaching
		///   the bounded length.
		fn append_or_replace_number(number: u32) {
			Numbers::<T>::mutate(|numbers| {
				if numbers.len() == NUM_VEC_LEN {
					let _ = numbers.pop_front();
				}
				numbers.push_back(number);
				log::info!("Number vector: {:?}", numbers);
			});
		}

		/// Check if we have fetched the data before. If yes, we can use the cached version
		///   stored in off-chain worker storage `storage`. If not, we fetch the remote info and
		///   write the info into the storage for future retrieval.
		fn fetch_remote_info() -> Result<(), Error<T>> {
			// Create a reference to Local Storage value.
			// Since the local storage is common for all offchain workers, it's a good practice
			// to prepend our entry wifth the pallet name.
			let s_info = StorageValueRef::persistent(b"offchain-demo::hn-info");

			// Local storage is persisted and shared between runs of the offchain workers,
			// offchain workers may run concurrently. We can use the `mutate` function to
			// write a storage entry in an atomic fashion.
			//
			// With a similar API as `StorageValue` with the variables `get`, `set`, `mutate`.
			// We will likely want to use `mutate` to access
			// the storage comprehensively.
			//
			if let Ok(Some(info)) = s_info.get::<HackerNewsInfo>() {
				// hn-info has already been fetched. Return early.
				log::info!("cached hn-info: {:?}", info);
				return Ok(());
			}

			// Since off-chain storage can be accessed by off-chain workers from multiple runs, it is important to lock
			//   it before doing heavy computations or write operations.
			//
			// There are four ways of defining a lock:
			//   1) `new` - lock with default time and block exipration
			//   2) `with_deadline` - lock with default block but custom time expiration
			//   3) `with_block_deadline` - lock with default time but custom block expiration
			//   4) `with_block_and_time_deadline` - lock with custom time and block expiration
			// Here we choose the most custom one for demonstration purpose.
			let mut lock = StorageLock::<BlockAndTime<Self>>::with_block_and_time_deadline(
				b"offchain-demo::lock", LOCK_BLOCK_EXPIRATION,
				Duration::from_millis(LOCK_TIMEOUT_EXPIRATION)
				);

			// We try to acquire the lock here. If failed, we know the `fetch_n_parse` part inside is being
			//   executed by previous run of ocw, so the function just returns.
			if let Ok(_guard) = lock.try_lock() {
				match Self::fetch_n_parse() {
					Ok(info) => { s_info.set(&info); }
					Err(err) => { return Err(err); }
				}
			}
			Ok(())
		}

		//	Fetch and Parse Price Struct 
		fn parse_get_price() -> Result<Price, Error<T>> { 
			let resp_bytes = Self::fetch_from_remote().map_err(|e| { 
				log::error!("Parse Error: {:?}", e);
					<Error<T>>::HttpFetchingError
			})?;
			//	BYTES TO JSON String  
			let resp_str = str::from_utf8(&resp_bytes).map_err(|_| <Error<T>>::DeserializeToStrError)?;
			// Print out our fetched JSON string
			log::info!("Price Struct Function: {}", resp_str);
			
			//	Deserialising JSON to struct using serde_json 
			let resp_json: Value = serde_json::from_str(&resp_str).map_err(|e| { 
				log::error!("String to json error {:?}", e);
				<Error<T>>::StringToJSONerr
			})?;
			//	Byte to Strung 
			let byte_string = serde_json::to_string(&resp_json["byte"]).map_err(|e| { 
				log::error!("String to json error {:?}", e);
				<Error<T>>::StringToJSONerr
				
			})?;
			let dot_price: Price = serde_json::from_str(&byte_string).map_err(|e| { 
				log::error!("String to json error {:?}", e);
				<Error<T>>::StringToJSONerr
			})?;

			Ok(dot_price)
		}

		/// Fetch from remote and deserialize the JSON to a struct
		fn fetch_n_parse() -> Result<HackerNewsInfo, Error<T>> {
			let resp_bytes = Self::fetch_from_remote().map_err(|e| {
				log::error!("fetch_from_remote error: {:?}", e);
				<Error<T>>::HttpFetchingError
			})?;

			let resp_str = str::from_utf8(&resp_bytes).map_err(|_| <Error<T>>::DeserializeToStrError)?;
			// Print out our fetched JSON string
			log::info!("fetch_n_parse: {}", resp_str);

			// Deserializing JSON to struct, thanks to `serde` and `serde_derive`
			let info: HackerNewsInfo =
			serde_json::from_str(&resp_str).map_err(|_| <Error<T>>::DeserializeToObjError)?;
			Ok(info)
		}

		/// This function uses the `offchain::http` API to query the remote endpoint information,
		///   and returns the JSON response as vector of bytes.
		fn fetch_from_remote() -> Result<Vec<u8>, Error<T>> {
			// Initiate an external HTTP GET request. This is using high-level wrappers from `sp_runtime`.
			let request = http::Request::get(HTTP_REMOTE_REQUEST);

			// Keeping the offchain worker execution time reasonable, so limiting the call to be within 3s.
			let timeout = sp_io::offchain::timestamp()
				.add(Duration::from_millis(FETCH_TIMEOUT_PERIOD));

			let pending = request
				.deadline(timeout) // Setting the timeout time
				.send() // Sending the request out by the host
				.map_err(|e| {
					log::error!("{:?}", e);
					<Error<T>>::HttpFetchingError
				})?;

			// By default, the http request is async from the runtime perspective. So we are asking the
			//   runtime to wait here
			// The returning value here is a `Result` of `Result`, so we are unwrapping it twice by two `?`
			//   ref: https://docs.substrate.io/rustdocs/latest/sp_runtime/offchain/http/struct.PendingRequest.html#method.try_wait
			let response = pending
				.try_wait(timeout)
				.map_err(|e| {
					log::error!("{:?}", e);
					<Error<T>>::HttpFetchingError
				})?
				.map_err(|e| {
					log::error!("{:?}", e);
					<Error<T>>::HttpFetchingError
				})?;

			if response.code != 200 {
				log::error!("Unexpected http request status code: {}", response.code);
				return Err(<Error<T>>::HttpFetchingError);
			}

			// Next we fully read the response body and collect it to a vector of bytes.
			Ok(response.body().collect::<Vec<u8>>())
		}

		fn offchain_signed_tx(block_number: T::BlockNumber) -> Result<(), Error<T>> {
			// We retrieve a signer and check if it is valid.
			//   Since this pallet only has one key in the keystore. We use `any_account()1 to
			//   retrieve it. If there are multiple keys and we want to pinpoint it, `with_filter()` can be chained,
			let signer = Signer::<T, T::AuthorityId>::any_account();

			// Translating the current block number to number and submit it on-chain
			let number = block_number.try_into().unwrap_or(0);

			// `result` is in the type of `Option<(Account<T>, Result<(), ()>)>`. It is:
			//   - `None`: no account is available for sending transaction
			//   - `Some((account, Ok(())))`: transaction is successfully sent
			//   - `Some((account, Err(())))`: error occured when sending the transaction
			let result = signer.send_signed_transaction(|_acct|
				// This is the on-chain function
				Call::submit_number_signed { number }
			);

			// Display error if the signed tx fails.
			if let Some((acc, res)) = result {
				if res.is_err() {
					log::error!("failure: offchain_signed_tx: tx sent: {:?}", acc.id);
					return Err(<Error<T>>::OffchainSignedTxError);
				}
				// Transaction is sent successfully
				return Ok(());
			}

			// The case of `None`: no account is available for sending
			log::error!("No local account available");
			Err(<Error<T>>::NoLocalAcctForSigning)
		}

		fn offchain_unsigned_tx(block_number: T::BlockNumber) -> Result<(), Error<T>> {
			let number = block_number.try_into().unwrap_or(0);
			let call = Call::submit_number_unsigned { number };

			// `submit_unsigned_transaction` returns a type of `Result<(), ()>`
			//   ref: https://substrate.dev/rustdocs/v2.0.0/frame_system/offchain/struct.SubmitTransaction.html#method.submit_unsigned_transaction
			SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into())
			.map_err(|_| {
				log::error!("Failed in offchain_unsigned_tx");
				<Error<T>>::OffchainUnsignedTxError
			})
		}

		fn offchain_unsigned_tx_signed_payload(block_number: T::BlockNumber) -> Result<(), Error<T>> {
			// Retrieve the signer to sign the payload
			let signer = Signer::<T, T::AuthorityId>::any_account();
			let price = Self::parse_get_price()
				.map_err(|e| { 
					log::error!("Parse Error: {:?}", e);
					<Error<T>>::HttpFetchingError
				})?;

				//	Convert Price u8 to u32 
			let priceu32 = Self::vec_conversion(price.price).map_err(|e| { 
				log::error!("Parse Error: {:?}", e);
				<Error<T>>::ParseError
			})?;

			Self::append_or_replace_number(priceu32);
			
			// `send_unsigned_transaction` is returning a type of `Option<(Account<T>, Result<(), ()>)>`.
			//   Similar to `send_signed_transaction`, they account for:
			//   - `None`: no account is available for sending transaction
			//   - `Some((account, Ok(())))`: transaction is successfully sent
			//   - `Some((account, Err(())))`: error occured when sending the transaction
			if let Some((_, res)) = signer.send_unsigned_transaction(
				|acct| Payload { 
					price: priceu32, public: acct.public.clone()
				 },
				|payload, signature| Call::submit_number_unsigned_with_signed_payload {
					payload, signature
				},
			) {
				return res.map_err(|_| {
					log::error!("Failed in offchain_unsigned_tx_signed_payload");
					<Error<T>>::OffchainUnsignedTxSignedPayloadError
				});
			}

			// The case of `None`: no account is available for sending
			log::error!("No local account available");
			Err(<Error<T>>::NoLocalAcctForSigning)
		}
		fn vec_conversion(val: Vec<u8>) -> Result<u32, Error<T>> { 
			let new_val: f32 = core::str::from_utf8(&val).map_err(|_| Error::<T>::ParseError)?
				//	Parse Vecu8 to f32
				.parse::<f32>().map_err(|_| Error::<T>::ParseError)?;
			Ok((new_val * 10000.) as u32)
		}
	}

	impl<T: Config> BlockNumberProvider for Pallet<T> {
		type BlockNumber = T::BlockNumber;

		fn current_block_number() -> Self::BlockNumber {
			<frame_system::Pallet<T>>::block_number()
		}
	}
}

