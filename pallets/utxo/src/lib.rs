// Copyright (c) 2021 RBB S.r.l
// opensource@mintlayer.org
// SPDX-License-Identifier: MIT
// Licensed under the MIT License;
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://spdx.org/licenses/MIT
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Author(s): C. Yap, L. Kuklinek

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
mod script;
mod sign;
#[cfg(test)]
mod tests;
pub mod tokens;
pub mod weights;

use chainscript::Builder;
use codec::Encode;
use core::convert::TryInto;
use frame_support::{
    inherent::Vec,
    pallet_prelude::{DispatchError, DispatchResultWithPostInfo},
};
use sp_core::{crypto::UncheckedFrom, H256, H512};
use sp_runtime::sp_std::vec;
use utxo_api::UtxoApi;

#[frame_support::pallet]
pub mod pallet {
    pub use crate::script::{BlockTime, RawBlockTime};
    use crate::sign::{self, Scheme};
    use crate::tokens::{NftDataHash, OutputData, TokenId, Value};
    use bech32;
    use chainscript::Script;
    use codec::{Decode, Encode};
    use core::marker::PhantomData;
    use frame_support::{
        dispatch::{DispatchResultWithPostInfo, Vec},
        pallet_prelude::*,
        sp_io::crypto,
        sp_runtime::traits::{BlakeTwo256, Dispatchable, Hash, SaturatedConversion},
        traits::{IsSubType, UnixTime},
    };
    use frame_system::pallet_prelude::*;
    use hex_literal::hex;
    use pp_api::ProgrammablePoolApi;
    #[cfg(feature = "std")]
    use serde::{Deserialize, Serialize};
    use sp_core::{
        sp_std::collections::btree_map::BTreeMap,
        sp_std::{convert::TryInto, str, vec},
        sr25519,
        testing::SR25519,
        H256, H512,
    };

    #[pallet::error]
    pub enum Error<T> {
        /// Account balance must be greater than or equal to the transfer amount.
        BalanceLow,
        /// Balance should be non-zero.
        BalanceZero,
        /// The signing account has no permission to do the operation.
        NoPermission,
        /// The given asset ID is unknown.
        Unknown,
        /// The origin account is frozen.
        Frozen,
        /// The asset ID is already taken.
        InUse,
        /// Invalid witness data given.
        BadWitness,
        /// Minimum balance should be non-zero.
        MinBalanceZero,
        /// No provider reference exists to allow a non-zero balance of a non-self-sufficient asset.
        NoProvider,
        /// Invalid metadata given.
        BadMetadata,
        /// No approval exists that would allow the transfer.
        Unapproved,
        /// The source account would not survive the transfer and it needs to stay alive.
        WouldDie,
        /// Thrown when there is an attempt to mint a duplicate collection.
        NftCollectionExists,
    }

    #[pallet::pallet]
    #[pallet::generate_store(pub(super) trait Store)]
    pub struct Pallet<T>(PhantomData<T>);

    /// runtime configuration
    #[pallet::config]
    pub trait Config: frame_system::Config + pallet_timestamp::Config {
        type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

        /// The overarching call type.
        type Call: Dispatchable + From<Call<Self>> + IsSubType<Call<Self>> + Clone;

        type WeightInfo: WeightInfo;

        type ProgrammablePool: ProgrammablePoolApi<AccountId = Self::AccountId>;

        fn authorities() -> Vec<H256>;
    }

    pub trait WeightInfo {
        fn spend(u: u32) -> Weight;
        fn token_create(u: u32) -> Weight;
        fn send_to_address(u: u32) -> Weight;
    }

    /// Transaction input
    ///
    /// The input contains two pieces of information used to unlock the funds being spent. The
    /// first one is `lock` and is usually committed to in UTXO specifed by the `outpoint`. It
    /// contains data used to protect the funds. The second one is `witness` that contains a proof
    /// that redeemer is allowed to spend the funds. The `witness` field does not contribute to the
    /// transaction ID hash to emulate the behaviour of SegWit.
    ///
    /// Both `lock` and `witness` are raw byte arrays. The exact interpretation depends on the
    /// [Destination] kind of the UTXO being spent. A couple of examples:
    ///
    /// * `Destination::Pubkey(key)`
    ///   * `lock` has to be empty
    ///   * `witness` contains the signature for the transaction and given pubkey
    /// * `Destination::ScriptHash(script_hash)`
    ///   * `lock` is the script fully expanded out, hash of `lock` has to match `script_hash`
    ///   * `witness` is a script that generates the input to the `lock` script
    #[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
    #[derive(
        Clone, Encode, Decode, Eq, PartialEq, PartialOrd, Ord, RuntimeDebug, Hash, Default,
    )]
    pub struct TransactionInput {
        /// The output being spent
        pub(crate) outpoint: H256,
        /// The lock data
        pub(crate) lock: Vec<u8>,
        /// The witness data
        pub(crate) witness: Vec<u8>,
    }

    impl TransactionInput {
        /// New input with a signature in the `witness` field.
        pub fn new_with_signature(outpoint: H256, sig_script: H512) -> Self {
            Self {
                outpoint,
                lock: Vec::new(),
                witness: (&sig_script[..]).to_vec(),
            }
        }

        /// New input with empty `lock` and `witness` to be filled later.
        pub fn new_empty(outpoint: H256) -> Self {
            Self {
                outpoint,
                lock: Vec::new(),
                witness: Vec::new(),
            }
        }

        /// New input with lock script and witness script.
        pub fn new_script(outpoint: H256, lock: Script, witness: Script) -> Self {
            Self {
                outpoint,
                lock: lock.into_bytes(),
                witness: witness.into_bytes(),
            }
        }

        /// Get lock hash.
        pub fn lock_hash(&self) -> H256 {
            BlakeTwo256::hash(&self.lock)
        }
    }

    /// Destination specifies where a payment goes. Can be a pubkey hash, script, etc.
    #[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
    #[derive(Clone, Encode, Decode, Eq, PartialEq, PartialOrd, Ord, RuntimeDebug)]
    pub enum Destination<AccountId> {
        /// Plain pay-to-pubkey
        Pubkey(sr25519::Public),
        /// Pay to fund a new programmable pool. Takes code and data.
        CreatePP(Vec<u8>, Vec<u8>),
        /// Pay to an existing contract. Takes a destination account,
        /// whether the call funds the contract, and input data.
        CallPP(AccountId, bool, Vec<u8>),
        /// Pay to script hash
        ScriptHash(H256),
    }

    impl<AccountId> Destination<AccountId> {
        /// Hash of an empty byte array
        const EMPTY: H256 = H256(hex!(
            "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8"
        ));

        /// Calculate lock commitment for given destination.
        ///
        /// The `lock` field of the input spending the UTXO has to match this hash.
        pub fn lock_commitment(&self) -> &H256 {
            match self {
                Destination::ScriptHash(hash) => hash,
                _ => &Self::EMPTY,
            }
        }
    }

    /// Output of a transaction
    #[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
    #[derive(Clone, Encode, Decode, Eq, PartialEq, PartialOrd, Ord, RuntimeDebug)]
    pub struct TransactionOutput<AccountId> {
        pub(crate) value: Value,
        pub(crate) destination: Destination<AccountId>,
        pub(crate) data: Option<OutputData>,
    }

    impl<AccountId> TransactionOutput<AccountId> {
        /// By default the header is 0:
        /// token type for both the value and fee is MLT,
        /// and the signature method is BLS.
        /// functions are available in TXOutputHeaderImpls to update the header.
        pub fn new_pubkey(value: Value, pubkey: H256) -> Self {
            let pubkey = sp_core::sr25519::Public::from_h256(pubkey);
            Self {
                value,
                destination: Destination::Pubkey(pubkey.into()),
                data: None,
            }
        }

        /// Create a new output to create a smart contract.
        pub fn new_create_pp(value: Value, code: Vec<u8>, data: Vec<u8>) -> Self {
            Self {
                value,
                destination: Destination::CreatePP(code, data),
                data: None,
            }
        }

        /// Create a new output to call a smart contract routine.
        pub fn new_call_pp(
            value: Value,
            dest_account: AccountId,
            fund: bool,
            input: Vec<u8>,
        ) -> Self {
            Self {
                value,
                destination: Destination::CallPP(dest_account, fund, input),
                data: None,
            }
        }

        /// Create a new output to given script hash.
        pub fn new_script_hash(value: Value, hash: H256) -> Self {
            Self {
                value,
                destination: Destination::ScriptHash(hash),
                data: None,
            }
        }

        /// Create a new output with the data field. This is going to be paid to a public key.
        pub fn new_p2pk_with_data(value: Value, pubkey: H256, data: OutputData) -> Self {
            let pubkey = sp_core::sr25519::Public::from_h256(pubkey);
            Self {
                value,
                destination: Destination::Pubkey(pubkey.into()),
                data: Some(data),
            }
        }
    }

    #[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
    #[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, Default)]
    pub struct Transaction<AccountId> {
        pub(crate) inputs: Vec<TransactionInput>,
        pub(crate) outputs: Vec<TransactionOutput<AccountId>>,
        pub(crate) time_lock: RawBlockTime,
    }

    impl<AccountId: Encode> Transaction<AccountId> {
        /// Get hash of output at given index.
        pub fn outpoint(&self, index: u64) -> H256 {
            BlakeTwo256::hash_of(&(self, index)).into()
        }

        // A convenience method to sign a transaction. Only Schnorr supported for now.
        pub fn sign(
            mut self,
            utxos: &[TransactionOutput<AccountId>],
            index: usize,
            pk: &sr25519::Public,
        ) -> Option<Self> {
            let msg = crate::sign::TransactionSigMsg::construct(
                Default::default(),
                &self,
                utxos,
                index as u64,
                u32::MAX,
            );
            self.inputs[index].witness =
                crypto::sr25519_sign(SR25519, pk, &msg.encode())?.0.to_vec();
            Some(self)
        }

        pub fn check_time_lock<T: Config>(&self) -> bool {
            match self.time_lock.time() {
                BlockTime::Blocks(lock_block_num) => {
                    <frame_system::Pallet<T>>::block_number() >= lock_block_num.into()
                }
                BlockTime::Timestamp(lock_time) => {
                    <pallet_timestamp::Pallet<T> as UnixTime>::now() >= lock_time
                }
            }
        }
    }

    // Transaction output type associated with given Config.
    #[allow(type_alias_bounds)]
    pub type TransactionOutputFor<T: Config> = TransactionOutput<T::AccountId>;

    // Transaction type associated with given Config.
    #[allow(type_alias_bounds)]
    pub type TransactionFor<T: Config> = Transaction<T::AccountId>;

    #[pallet::storage]
    #[pallet::getter(fn reward_total)]
    pub(super) type RewardTotal<T> = StorageValue<_, Value, ValueQuery>;

    #[pallet::storage]
    #[pallet::getter(fn utxo_store)]
    pub(super) type UtxoStore<T: Config> = StorageMap<_, Identity, H256, TransactionOutputFor<T>>;

    #[pallet::storage]
    #[pallet::getter(fn pointer_to_issue_token)]
    pub(super) type TokenIssuanceTransactions<T: Config> =
        StorageMap<_, Identity, TokenId, /* UTXO */ H256, OptionQuery>;

    #[pallet::storage]
    #[pallet::getter(fn nft_unique_data_hash)]
    pub(super) type NftUniqueDataHash<T: Config> =
        StorageMap<_, Identity, NftDataHash, /* UTXO */ H256, OptionQuery>;

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    #[pallet::metadata(T::AccountId = "AccountId")]
    pub enum Event<T: Config> {
        TransactionSuccess(TransactionFor<T>),
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        fn on_finalize(block_num: T::BlockNumber) {
            disperse_reward::<T>(&T::authorities(), block_num)
        }
    }

    pub(crate) fn get_output_by_token_id<T: Config>(
        token_id: TokenId,
    ) -> Option<TransactionOutputFor<T>> {
        let utxo_id = TokenIssuanceTransactions::<T>::get(token_id)?;
        UtxoStore::<T>::get(utxo_id)
    }

    // Strips a transaction of its Signature fields by replacing value with ZERO-initialized fixed hash.
    pub fn get_simple_transaction<AccountId: Encode + Clone>(
        tx: &Transaction<AccountId>,
    ) -> Vec<u8> {
        let mut trx = tx.clone();
        for input in trx.inputs.iter_mut() {
            input.witness = Vec::new();
        }
        trx.encode()
    }

    fn disperse_reward<T: Config>(auths: &[H256], block_number: T::BlockNumber) {
        let reward = <RewardTotal<T>>::take();
        let share_value: Value =
            reward.checked_div(auths.len() as Value).ok_or("No authorities").unwrap();
        if share_value == 0 {
            //put reward back if it can't be split nicely
            <RewardTotal<T>>::put(reward as Value);
            return;
        }

        let remainder = reward
            .checked_sub(share_value * auths.len() as Value)
            .ok_or("Sub underflow")
            .unwrap();

        log::debug!("disperse_reward:: reward total: {:?}", remainder);
        <RewardTotal<T>>::put(remainder as Value);

        for authority in auths {
            // TODO: where do we get the header info?
            let utxo = TransactionOutput::new_pubkey(share_value, *authority);

            let hash = {
                let b_num = block_number.saturated_into::<u64>();
                BlakeTwo256::hash_of(&(&utxo, b_num))
            };

            if !<UtxoStore<T>>::contains_key(hash) {
                <UtxoStore<T>>::insert(hash, utxo);
            }
        }
    }

    pub fn create<T: Config>(
        caller: &T::AccountId,
        code: &Vec<u8>,
        utxo_hash: H256,
        utxo_value: u128,
        data: &Vec<u8>,
    ) {
        let weight: Weight = 6000000000;

        match T::ProgrammablePool::create(caller, weight, code, utxo_hash, utxo_value, data) {
            Ok(_) => log::info!("success!"),
            Err(e) => log::error!("failure: {:#?}", e),
        }
    }

    pub fn call<T: Config>(
        caller: &T::AccountId,
        dest: &T::AccountId,
        utxo_hash: H256,
        utxo_value: u128,
        fund_contract: bool,
        data: &Vec<u8>,
    ) {
        let weight: Weight = 6000000000;

        match T::ProgrammablePool::call(
            caller,
            dest,
            weight,
            utxo_hash,
            utxo_value,
            fund_contract,
            data,
        ) {
            Ok(_) => log::info!("success!"),
            Err(e) => log::error!("failure: {:#?}", e),
        }
    }

    pub fn validate_transaction<T: Config>(
        tx: &TransactionFor<T>,
    ) -> Result<ValidTransaction, &'static str> {
        //both inputs and outputs should contain at least 1 and at most u32::MAX - 1 entries

        //ensure rather than assert to avoid panic
        //both inputs and outputs should contain at least 1 and at most u32::MAX - 1 entries
        ensure!(!tx.inputs.is_empty(), "no inputs");
        ensure!(!tx.outputs.is_empty(), "no outputs");
        ensure!(tx.inputs.len() < (u32::MAX as usize), "too many inputs");
        ensure!(tx.outputs.len() < (u32::MAX as usize), "too many outputs");

        //ensure each input is used only a single time
        //maps each input into btree
        //if map.len() > num of inputs then fail
        //https://doc.rust-lang.org/std/collections/struct.BTreeMap.html
        //WARNING workshop code has a bug here
        //https://github.com/substrate-developer-hub/utxo-workshop/blob/workshop/runtime/src/utxo.rs
        //input_map.len() > transaction.inputs.len() //THIS IS WRONG
        {
            let input_map: BTreeMap<_, ()> =
                tx.inputs.iter().map(|input| (input.outpoint, ())).collect();
            //we want map size and input size to be equal to ensure each is used only once
            ensure!(
                input_map.len() == tx.inputs.len(),
                "each input should be used only once"
            );
        }
        //ensure each output is unique
        //map each output to btree to count unique elements
        //WARNING example code has a bug here
        //out_map.len() != transaction.outputs.len() //THIS IS WRONG
        {
            let out_map: BTreeMap<_, ()> = tx.outputs.iter().map(|output| (output, ())).collect();
            //check each output is defined only once
            ensure!(
                out_map.len() == tx.outputs.len(),
                "each output should be used once"
            );
        }

        // Verify absolute time lock
        ensure!(
            tx.check_time_lock::<T>(),
            "Time lock restrictions not satisfied"
        );

        // Resolve the transaction inputs by looking up UTXOs being spent by them.
        //
        // This will cointain one of the following:
        // * Ok(utxos): a vector of UTXOs each input spends.
        // * Err(missing): a vector of outputs missing from the store
        let input_utxos = {
            let mut missing = Vec::new();
            let mut resolved: Vec<TransactionOutputFor<T>> = Vec::new();

            for input in &tx.inputs {
                if let Some(input_utxo) = <UtxoStore<T>>::get(&input.outpoint) {
                    let lock_commitment = input_utxo.destination.lock_commitment();
                    ensure!(
                        input.lock_hash() == *lock_commitment,
                        "Lock hash does not match"
                    );
                    resolved.push(input_utxo);
                } else {
                    missing.push(input.outpoint.clone().as_fixed_bytes().to_vec());
                }
            }

            missing.is_empty().then(|| resolved).ok_or(missing)
        };

        let full_inputs: Vec<(TokenId, TransactionOutputFor<T>)> = tx
            .inputs
            .iter()
            .filter_map(|input| <UtxoStore<T>>::get(&input.outpoint))
            .filter_map(|output| match output.data {
                Some(ref data) => match data {
                    OutputData::TokenTransferV1 { token_id, .. }
                    | OutputData::TokenIssuanceV1 { token_id, .. }
                    | OutputData::NftMintV1 { token_id, .. } => Some((token_id.clone(), output)),
                    OutputData::TokenBurnV1 { .. } => None,
                },
                None => {
                    // We do not calculate MLT here
                    None
                }
            })
            .collect();

        //
        let mut total_value_of_input_tokens: BTreeMap<TokenId, Value> = BTreeMap::new();
        let mut mlt_amount_in_inputs: Value = 0;
        for input in &tx.inputs {
            let output = <UtxoStore<T>>::get(&input.outpoint).ok_or("missing inputs")?;
            match &output.data {
                Some(OutputData::TokenIssuanceV1 {
                    ref token_id,
                    token_ticker,
                    amount_to_issue,
                    number_of_decimals,
                    metadata_uri,
                }) => {
                    // We have to check is this token already issued?
                    ensure!(
                        TokenIssuanceTransactions::<T>::contains_key(token_id),
                        "token has never been issued"
                    );
                    ensure!(
                        token_ticker.is_ascii(),
                        "token ticker has none ascii characters"
                    );
                    ensure!(
                        metadata_uri.is_ascii(),
                        "metadata uri has none ascii characters"
                    );
                    ensure!(token_ticker.len() <= 5, "token ticker is too long");
                    ensure!(!token_ticker.is_empty(), "token ticker can't be empty");
                    ensure!(metadata_uri.len() <= 100, "token metadata uri is too long");
                    ensure!(amount_to_issue > &0u128, "output value must be nonzero");
                    ensure!(number_of_decimals <= &18, "too long decimals");
                    // If token has just created we can't meet another amount here.
                    total_value_of_input_tokens.insert(token_id.clone(), *amount_to_issue);
                    // But probably in this input we have a fee
                    mlt_amount_in_inputs = mlt_amount_in_inputs
                        .checked_add(output.value)
                        .ok_or("input value overflow")?;
                }
                Some(OutputData::TokenTransferV1 {
                    ref token_id,
                    amount,
                    ..
                }) => {
                    total_value_of_input_tokens.insert(
                        token_id.clone(),
                        total_value_of_input_tokens
                            .get(token_id)
                            .unwrap_or(&0)
                            .checked_add(*amount)
                            .ok_or("input value overflow")?,
                    );
                    // But probably in this input we have a fee
                    mlt_amount_in_inputs = mlt_amount_in_inputs
                        .checked_add(output.value)
                        .ok_or("input value overflow")?;
                }
                Some(OutputData::TokenBurnV1 { .. }) => {
                    // Nothing to do here because tokens no longer exist.
                }
                Some(OutputData::NftMintV1 {
                    ref token_id,
                    data_hash,
                    metadata_uri,
                }) => {
                    // We have to check is this token already issued?
                    ensure!(
                        TokenIssuanceTransactions::<T>::contains_key(token_id),
                        "unable to use an input where NFT has not minted yet"
                    );
                    // Check is this digital data unique?
                    ensure!(
                        NftUniqueDataHash::<T>::contains_key(data_hash),
                        "unable to use an input where NFT digital data was changed"
                    );
                    ensure!(
                        metadata_uri.is_ascii(),
                        "metadata uri has none ascii characters"
                    );
                    // If NFT has just created we can't meet another NFT part here.
                    total_value_of_input_tokens.insert(token_id.clone(), 1);
                }
                None => {
                    mlt_amount_in_inputs = mlt_amount_in_inputs
                        .checked_add(output.value)
                        .ok_or("input value overflow")?;
                }
            }
        }

        let mut total_value_of_output_tokens: BTreeMap<TokenId, Value> = BTreeMap::new();
        let mut mlt_amount_in_outputs: Value = 0;
        for output in &tx.outputs {
            match &output.data {
                Some(OutputData::TokenIssuanceV1 {
                    ref token_id,
                    token_ticker,
                    amount_to_issue,
                    number_of_decimals,
                    metadata_uri,
                }) => {
                    // We have to check is this token already issued?
                    ensure!(
                        !TokenIssuanceTransactions::<T>::contains_key(token_id),
                        "token has already been issued"
                    );
                    ensure!(
                        token_ticker.is_ascii(),
                        "token ticker has none ascii characters"
                    );
                    ensure!(
                        metadata_uri.is_ascii(),
                        "metadata uri has none ascii characters"
                    );
                    ensure!(token_ticker.len() <= 5, "token ticker is too long");
                    ensure!(!token_ticker.is_empty(), "token ticker can't be empty");
                    ensure!(metadata_uri.len() <= 100, "token metadata uri is too long");
                    ensure!(amount_to_issue > &0u128, "output value must be nonzero");
                    ensure!(number_of_decimals <= &18, "too long decimals");

                    // If token has just created we can't meet another amount here.
                    total_value_of_output_tokens.insert(token_id.clone(), *amount_to_issue);
                    // But probably in this input we have a fee
                    mlt_amount_in_outputs = mlt_amount_in_outputs
                        .checked_add(output.value)
                        .ok_or("input value overflow")?;
                }
                Some(OutputData::TokenTransferV1 {
                    ref token_id,
                    amount,
                    ..
                }) => {
                    total_value_of_output_tokens.insert(
                        token_id.clone(),
                        total_value_of_output_tokens
                            .get(token_id)
                            .unwrap_or(&0)
                            .checked_add(*amount)
                            .ok_or("output value overflow")?,
                    );
                    // But probably in this input we have a fee
                    mlt_amount_in_outputs = mlt_amount_in_outputs
                        .checked_add(output.value)
                        .ok_or("input value overflow")?;
                }
                Some(OutputData::TokenBurnV1 { .. }) => {
                    // Nothing to do here because tokens no longer exist.
                }
                Some(OutputData::NftMintV1 {
                    ref token_id,
                    data_hash,
                    metadata_uri,
                }) => {
                    // We have to check is this token already issued?
                    ensure!(
                        !TokenIssuanceTransactions::<T>::contains_key(token_id),
                        "token has already been issued"
                    );

                    // Check is this digital data unique?
                    ensure!(
                        !<NftUniqueDataHash<T>>::contains_key(data_hash),
                        "digital data has already been minted"
                    );
                    ensure!(
                        metadata_uri.is_ascii(),
                        "metadata uri has none ascii characters"
                    );
                    // If NFT has just created we can't meet another NFT part here.
                    total_value_of_output_tokens.insert(token_id.clone(), 1);
                }
                None => {
                    mlt_amount_in_outputs = mlt_amount_in_outputs
                        .checked_add(output.value)
                        .ok_or("output value overflow")?;
                }
            }
        }

        // Check for token creation
        for output in tx.outputs.iter() {
            let tid = match output.data {
                Some(OutputData::TokenTransferV1 { ref token_id, .. })
                | Some(OutputData::TokenIssuanceV1 { ref token_id, .. }) => token_id.clone(),
                Some(OutputData::NftMintV1 { .. })
                | Some(OutputData::TokenBurnV1 { .. })
                | None => continue,
            };
            // If we have input and output for the same token it's not a problem
            if full_inputs.iter().find(|&x| (x.0 == tid) && (x.1 != *output)).is_some() {
                continue;
            } else {
                // But when we don't have an input for token but token id exist
                ensure!(
                    !<TokenIssuanceTransactions<T>>::contains_key(tid),
                    "no inputs for the token id"
                );
            }
        }

        let mut new_utxos = Vec::new();
        let mut reward = 0;

        // Check that outputs are valid
        for (output_index, output) in tx.outputs.iter().enumerate() {
            match output.data {
                Some(OutputData::TokenIssuanceV1 {
                    amount_to_issue, ..
                }) => ensure!(amount_to_issue > 0, "output value must be nonzero"),
                Some(OutputData::TokenTransferV1 { amount, .. }) => {
                    ensure!(amount > 0, "output value must be nonzero")
                }
                Some(OutputData::TokenBurnV1 { amount_to_burn, .. }) => {
                    ensure!(amount_to_burn > 0, "output value must be nonzero")
                }
                Some(OutputData::NftMintV1 { .. }) => {
                    // Nothing to check
                }
                None => ensure!(output.value > 0, "output value must be nonzero"),
            }
            let hash = tx.outpoint(output_index as u64);
            ensure!(!<UtxoStore<T>>::contains_key(hash), "output already exists");
            new_utxos.push(hash.as_fixed_bytes().to_vec());

            match output.destination {
                Destination::Pubkey(_) | Destination::ScriptHash(_) => {}
                Destination::CreatePP(_, _) => {
                    log::info!("TODO validate OP_CREATE");
                }
                Destination::CallPP(_, _, _) => {
                    log::info!("TODO validate OP_CALL");
                }
            }
        }

        // if all spent UTXOs are available, check the math and signatures
        if let Ok(input_utxos) = &input_utxos {
            // We have to check sum of input tokens is less or equal to output tokens.
            ensure!(
                mlt_amount_in_outputs <= mlt_amount_in_inputs,
                "output value must not exceed input value"
            );

            let mut num_creations = 0;
            for output_token in &total_value_of_output_tokens {
                match total_value_of_input_tokens.get(&output_token.0) {
                    Some(input_value) => ensure!(
                        input_value >= &output_token.1,
                        "output value must not exceed input value"
                    ),
                    None => {
                        match &tx.outputs.iter().find(|x| match x.data {
                            Some(ref output_data) => {
                                output_data.id().as_ref() == Some(output_token.0)
                            }
                            None => false,
                        }) {
                            Some(output) => match output.data {
                                None
                                | Some(OutputData::TokenTransferV1 { .. })
                                | Some(OutputData::TokenBurnV1 { .. }) => {
                                    frame_support::fail!("input for the token not found")
                                }
                                Some(OutputData::NftMintV1 { .. })
                                | Some(OutputData::TokenIssuanceV1 { .. }) => {
                                    num_creations += 1;
                                    continue;
                                }
                            },
                            None => frame_support::fail!("corrupted output data"),
                        }
                    }
                }
            }
            ensure!(
                mlt_amount_in_inputs >= (num_creations * crate::tokens::Mlt(100).to_munit()),
                "insufficient fee"
            );

            for (index, (input, input_utxo)) in tx.inputs.iter().zip(input_utxos).enumerate() {
                match &input_utxo.destination {
                    Destination::Pubkey(pubkey) => {
                        let msg = sign::TransactionSigMsg::construct(
                            sign::SigHash::default(),
                            &tx,
                            &input_utxos,
                            index as u64,
                            u32::MAX,
                        );
                        let ok = pubkey
                            .parse_sig(&input.witness[..])
                            .ok_or("bad signature format")?
                            .verify(&msg);
                        ensure!(ok, "signature must be valid");
                    }
                    Destination::CreatePP(_, _) => {
                        log::info!("TODO validate spending of OP_CREATE");
                    }
                    Destination::CallPP(_, _, _) => {
                        let spend =
                            u16::from_le_bytes(input.witness[1..].try_into().or_else(|_| {
                                Err(DispatchError::Other(
                                    "Failed to convert witness to an opcode",
                                ))
                            })?);
                        ensure!(spend == 0x1337, "OP_SPEND not found");
                    }
                    Destination::ScriptHash(_hash) => {
                        let witness = input.witness.clone();
                        let lock = input.lock.clone();
                        crate::script::verify(&tx, &input_utxos, index as u64, witness, lock)
                            .map_err(|_| "script verification failed")?;
                    }
                }
            }

            // Reward at the moment only in MLT
            reward = mlt_amount_in_inputs
                .checked_sub(mlt_amount_in_outputs)
                .ok_or("reward underflow")?;
        }

        Ok(ValidTransaction {
            priority: reward as u64,
            requires: input_utxos.map_or_else(|x| x, |_| Vec::new()),
            provides: new_utxos,
            longevity: TransactionLongevity::MAX,
            propagate: true,
        })
    }

    /// Update storage to reflect changes made by transaction
    /// Where each utxo key is a hash of the entire transaction and its order in the TransactionOutputs vector
    pub fn update_storage<T: Config>(
        caller: &T::AccountId,
        tx: &TransactionFor<T>,
        reward: Value,
    ) -> DispatchResultWithPostInfo {
        // Calculate new reward total
        let new_total = <RewardTotal<T>>::get().checked_add(reward).ok_or("Reward overflow")?;

        log::debug!("update_storage:: reward total: {:?}", new_total);
        <RewardTotal<T>>::put(new_total);

        // Removing spent UTXOs
        for input in &tx.inputs {
            log::debug!("removing {:?} in UtxoStore.", input.outpoint);
            <UtxoStore<T>>::remove(input.outpoint);
        }

        for (index, output) in tx.outputs.iter().enumerate() {
            let hash = tx.outpoint(index as u64);
            log::debug!("inserting to UtxoStore {:?} as key {:?}", output, hash);
            <UtxoStore<T>>::insert(hash, output);

            match &output.destination {
                Destination::Pubkey(_) | Destination::ScriptHash(_) => {
                    let hash = tx.outpoint(index as u64);
                    log::debug!("inserting to UtxoStore {:?} as key {:?}", output, hash);
                    <UtxoStore<T>>::insert(hash, output);
                    match &output.data {
                        Some(OutputData::NftMintV1 {
                            token_id,
                            data_hash,
                            ..
                        }) => {
                            // We have to control that digital data of NFT is unique.
                            // Otherwise, anybody else might make a new NFT with exactly the same hash.
                            <NftUniqueDataHash<T>>::insert(data_hash, hash);
                            // Also, we should provide possibility of find an output that by token_id.
                            // This output is a place where token was created. It allow us to check that a token or
                            // a NFT have not created yet.
                            <TokenIssuanceTransactions<T>>::insert(token_id, hash);
                        }
                        Some(OutputData::TokenIssuanceV1 { token_id, .. }) => {
                            // For MLS-01 we save a relation between token_id and the output where
                            // token was created.
                            <TokenIssuanceTransactions<T>>::insert(token_id, hash);
                        }
                        // For the security reason we are implementing all cases
                        Some(OutputData::TokenBurnV1 { .. })
                        | Some(OutputData::TokenTransferV1 { .. })
                        | None => continue,
                    }
                }
                Destination::CreatePP(script, data) => {
                    create::<T>(caller, script, hash, output.value, &data);
                }
                Destination::CallPP(acct_id, fund, data) => {
                    call::<T>(caller, acct_id, hash, output.value, *fund, data);
                }
            }
        }

        Ok(().into())
    }

    pub fn spend<T: Config>(
        caller: &T::AccountId,
        tx: &TransactionFor<T>,
    ) -> DispatchResultWithPostInfo {
        let tx_validity = validate_transaction::<T>(tx)?;
        ensure!(tx_validity.requires.is_empty(), "missing inputs");
        update_storage::<T>(caller, tx, tx_validity.priority as Value)?;
        Ok(().into())
    }

    /// Pick the UTXOs of `caller` from UtxoStore that satisfy request `value`
    ///
    /// Return a list of UTXOs that satisfy the request
    /// Return empty vector if caller doesn't have enough UTXO
    ///
    // NOTE: limitation here is that this is only able to pick `Destination::Pubkey`
    // UTXOs because the ownership of those can be easily determined.
    // TODO: keep track of "our" UTXO separately?
    pub fn pick_utxo<T: Config>(
        caller: &T::AccountId,
        value: Value,
    ) -> (Value, Vec<H256>, Vec<TransactionOutputFor<T>>) {
        let mut utxos = Vec::new();
        let mut hashes = Vec::new();
        let mut total = 0;

        for (hash, utxo) in UtxoStore::<T>::iter() {
            match utxo.destination {
                Destination::Pubkey(pubkey) => {
                    if caller.encode() == pubkey.encode() {
                        total += utxo.value;
                        hashes.push(hash);
                        utxos.push(utxo);

                        if total >= value {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        (total, hashes, utxos)
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        #[pallet::weight(<T as Config>::WeightInfo::spend(tx.inputs.len().saturating_add(tx.outputs.len()) as u32))]
        pub fn spend(
            origin: OriginFor<T>,
            tx: Transaction<T::AccountId>,
        ) -> DispatchResultWithPostInfo {
            spend::<T>(&ensure_signed(origin)?, &tx)?;
            Self::deposit_event(Event::<T>::TransactionSuccess(tx));
            Ok(().into())
        }

        #[pallet::weight(<T as Config>::WeightInfo::send_to_address(16_u32.saturating_add(address.len() as u32)))]
        pub fn send_to_address(
            origin: OriginFor<T>,
            value: Value,
            address: Vec<u8>,
        ) -> DispatchResultWithPostInfo {
            let (_, data, _) = bech32::decode(&address).map_err(|e| match e {
                bech32::Error::InvalidLength => {
                    DispatchError::Other("Failed to decode address: invalid length")
                }
                bech32::Error::InvalidChar(_) => {
                    DispatchError::Other("Failed to decode address: invalid character")
                }
                bech32::Error::MixedCase => {
                    DispatchError::Other("Failed to decode address: mixed case")
                }
                bech32::Error::InvalidChecksum => {
                    DispatchError::Other("Failed to decode address: invalid checksum")
                }
                bech32::Error::InvalidHrp => {
                    DispatchError::Other("Failed to decode address: invalid HRP")
                }
                _ => DispatchError::Other("Failed to decode address"),
            })?;

            let dest: Destination<T::AccountId> = Destination::decode(&mut &data[..])
                .map_err(|_| DispatchError::Other("Failed to decode buffer into `Destination`"))?;
            ensure!(value > 0, "Value transferred must be larger than zero");

            let signer = ensure_signed(origin)?;
            let (total, hashes, utxos) = pick_utxo::<T>(&signer, value);

            ensure!(total >= value, "Caller doesn't have enough UTXOs");

            let mut inputs: Vec<TransactionInput> = Vec::new();
            for utxo in hashes.iter() {
                inputs.push(TransactionInput::new_empty(*utxo));
            }

            let pubkey_raw: [u8; 32] = signer
                .encode()
                .try_into()
                .map_err(|_| DispatchError::Other("Failed to get caller's public key"))?;

            let mut tx = Transaction {
                inputs,
                outputs: vec![
                    TransactionOutput {
                        value,
                        destination: dest,
                        // todo: We need to check what kind of token over here
                        data: None,
                    },
                    TransactionOutput::new_pubkey(total - value, H256::from(pubkey_raw)),
                ],
                time_lock: Default::default(),
            };

            for i in 0..tx.inputs.len() {
                tx = tx
                    .sign(&utxos, i, &sr25519::Public(pubkey_raw))
                    .ok_or(DispatchError::Other("Failed to sign the transaction"))?;
            }

            spend::<T>(&signer, &tx)
        }
    }

    #[pallet::genesis_config]
    pub struct GenesisConfig<T: Config> {
        pub genesis_utxos: Vec<TransactionOutputFor<T>>,
        pub _marker: PhantomData<T>,
    }

    #[cfg(feature = "std")]
    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            Self {
                genesis_utxos: vec![],
                _marker: Default::default(),
            }
        }
    }

    #[pallet::genesis_build]
    impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
        fn build(&self) {
            self.genesis_utxos.iter().cloned().for_each(|u| {
                UtxoStore::<T>::insert(BlakeTwo256::hash_of(&u), u);
            });
        }
    }
}

impl<T: Config> crate::Pallet<T> {
    pub fn send() -> u32 {
        1337
    }

    pub fn nft_read(
        nft_id: &core::primitive::str,
    ) -> Option<(/* Data url */ Vec<u8>, /* Data hash */ Vec<u8>)> {
        match crate::pallet::get_output_by_token_id::<T>(
            crate::tokens::TokenId::from_string(&nft_id).ok()?,
        )?
        .data
        {
            Some(crate::tokens::OutputData::NftMintV1 {
                data_hash,
                metadata_uri,
                ..
            }) => Some((metadata_uri, data_hash.encode())),
            _ => None,
        }
    }
}

fn coin_picker<T: Config>(outpoints: &Vec<H256>) -> Result<Vec<TransactionInput>, DispatchError> {
    let mut inputs: Vec<TransactionInput> = Vec::new();

    // consensus-critical sorting function...
    let mut outpoints = outpoints.clone();
    outpoints.sort();

    for outpoint in outpoints.iter() {
        let tx = <UtxoStore<T>>::get(&outpoint).ok_or("UTXO doesn't exist!")?;
        match tx.destination {
            Destination::CallPP(_, _, _) => {
                inputs.push(TransactionInput::new_script(
                    *outpoint,
                    Builder::new().into_script(),
                    Builder::new().push_int(0x1337).into_script(),
                ));
            }
            _ => {
                return Err(DispatchError::Other("Only CallPP vouts can be spent!"));
            }
        }
    }

    Ok(inputs)
}

impl<T: Config> UtxoApi for Pallet<T>
where
    T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
{
    type AccountId = T::AccountId;

    fn spend(
        caller: &T::AccountId,
        value: u128,
        address: H256,
        utxo: H256,
        sig: H512,
    ) -> DispatchResultWithPostInfo {
        spend::<T>(
            caller,
            &Transaction {
                inputs: vec![TransactionInput::new_with_signature(utxo, sig)],
                outputs: vec![TransactionOutputFor::<T>::new_pubkey(value, address)],
                time_lock: Default::default(),
            },
        )
    }

    fn send_conscrit_p2pk(
        caller: &T::AccountId,
        dest: &T::AccountId,
        value: u128,
        outpoints: &Vec<H256>,
    ) -> Result<(), DispatchError> {
        let pubkey_raw: [u8; 32] =
            dest.encode().try_into().map_err(|_| "Failed to get caller's public key")?;

        spend::<T>(
            caller,
            &Transaction {
                inputs: coin_picker::<T>(outpoints)?,
                outputs: vec![TransactionOutput::new_pubkey(value, H256::from(pubkey_raw))],
                time_lock: Default::default(),
            },
        )
        .map_err(|_| "Failed to spend the transaction!")?;
        Ok(())
    }

    fn send_conscrit_c2c(
        caller: &Self::AccountId,
        dest: &Self::AccountId,
        value: u128,
        data: &Vec<u8>,
        outpoints: &Vec<H256>,
    ) -> Result<(), DispatchError> {
        spend::<T>(
            caller,
            &Transaction {
                inputs: coin_picker::<T>(outpoints)?,
                outputs: vec![TransactionOutput::new_call_pp(
                    value,
                    dest.clone(),
                    true,
                    data.clone(),
                )],
                time_lock: Default::default(),
            },
        )
        .map_err(|_| "Failed to spend the transaction!")?;
        Ok(())
    }
}
