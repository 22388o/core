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
// Author(s): C. Yap

use crate::Perbill;

use codec::Decode;
use frame_support::dispatch::{DispatchResultWithPostInfo, Vec};
use frame_support::fail;
use frame_system::{Config as SysConfig, RawOrigin};
use pallet_staking::{BalanceOf, Pallet as StakingPallet};
use pallet_utxo::staking::StakingHelper;
use sp_core::{sp_std::vec, H256};
use sp_runtime::traits::StaticLookup;

type StakeAccountId<T> = <T as SysConfig>::AccountId;
type LookupSourceOf<T> = <<T as SysConfig>::Lookup as StaticLookup>::Source;

pub struct StakeOps<T>(sp_core::sp_std::marker::PhantomData<T>);
impl<T: pallet_staking::Config + pallet_utxo::Config + pallet_session::Config>
    StakingHelper<T::AccountId> for StakeOps<T>
where
    StakeAccountId<T>: From<[u8; 32]>,
    BalanceOf<T>: From<u128>,
{
    fn get_account_id(pub_key: &H256) -> StakeAccountId<T> {
        pub_key.0.into()
    }

    fn lock_for_staking(
        stash_account: &StakeAccountId<T>,
        controller_account: &StakeAccountId<T>,
        session_key: &Vec<u8>,
        value: u128,
    ) -> DispatchResultWithPostInfo {
        let controller_lookup: LookupSourceOf<T> = T::Lookup::unlookup(controller_account.clone());
        let reward_destination = pallet_staking::RewardDestination::Staked;

        // bond the funds
        StakingPallet::<T>::bond(
            RawOrigin::Signed(stash_account.clone()).into(),
            controller_lookup,
            value.into(),
            reward_destination,
        )?;

        let rotate_keys = sp_core::Bytes::from(session_key.to_vec());
        // session keys
        let sesh_key = <T as pallet_session::Config>::Keys::decode(&mut &rotate_keys[..])
            .expect("SessionKeys decoded successfully");
        pallet_session::Pallet::<T>::set_keys(
            RawOrigin::Signed(controller_account.clone()).into(),
            sesh_key,
            vec![],
        )?;

        let validator_prefs = pallet_staking::ValidatorPrefs {
            commission: Perbill::from_percent(0),
            ..Default::default()
        };

        // applying for the role of "validator".
        StakingPallet::<T>::validate(
            RawOrigin::Signed(controller_account.clone()).into(),
            validator_prefs,
        )?;

        Ok(().into())
    }

    fn lock_extra_for_staking(
        stash_account: &StakeAccountId<T>,
        controller_account: &StakeAccountId<T>,
        value: u128,
    ) -> DispatchResultWithPostInfo {
        // get the stash account first
        if let Some(stake_ledger) = <StakingPallet<T>>::ledger(controller_account.clone()) {
            if stash_account != &stake_ledger.stash {
                log::error!(
                    "stash account {:?} has no permission to stake.",
                    stash_account
                );
                return Err(pallet_utxo::Error::<T>::NoPermission)?;
            }

            StakingPallet::<T>::bond_extra(
                RawOrigin::Signed(stake_ledger.stash).into(),
                value.into(),
            )?;
            return Ok(().into());
        }

        log::error!("check sync with pallet-staking.");
        return Err(pallet_utxo::Error::<T>::InvalidOperation)?;
    }

    fn unlock_request_for_withdrawal(
        stash_account: &StakeAccountId<T>,
    ) -> DispatchResultWithPostInfo {
        // get the controller account, given the stash_account.
        let controller_account = <StakingPallet<T>>::bonded(stash_account.clone())
            .ok_or(pallet_utxo::Error::<T>::StashAccountNotFound)?;

        // stop validating / block producing
        StakingPallet::<T>::chill(RawOrigin::Signed(controller_account.clone()).into())?;

        // get the total balance to free up
        let stake_ledger = <StakingPallet<T>>::ledger(controller_account.clone())
            .ok_or(pallet_utxo::Error::<T>::ControllerAccountNotFound)?;

        // unbond
        StakingPallet::<T>::unbond(
            RawOrigin::Signed(controller_account).into(),
            stake_ledger.total,
        )?;

        Ok(().into())
    }

    fn withdraw(stash_account: &StakeAccountId<T>) -> DispatchResultWithPostInfo {
        // get the controller account, given the stash_account.
        let controller_account = <StakingPallet<T>>::bonded(stash_account.clone())
            .ok_or(pallet_utxo::Error::<T>::StashAccountNotFound)?;

        let stake_ledger = <StakingPallet<T>>::ledger(controller_account.clone())
            .ok_or(pallet_utxo::Error::<T>::ControllerAccountNotFound)?;
        if stake_ledger.unlocking.is_empty() {
            log::error!("No unlocked funds found to withdraw.");
            fail!(pallet_utxo::Error::<T>::InvalidOperation);
        } else if stake_ledger.unlocking.len() > 1 {
            log::error!("Pallet-staking ledger's unlocking field should only contain ONE element.");
            fail!(pallet_utxo::Error::<T>::InvalidOperation)
        }

        let res = StakingPallet::<T>::withdraw_unbonded(
            RawOrigin::Signed(controller_account.clone()).into(),
            0,
        )?;

        // if the staking still exists, withdrawal was unsuccessful.
        if <StakingPallet<T>>::ledger(controller_account).is_some() {
            log::error!("no withdrawal was done.");
            fail!(pallet_utxo::Error::<T>::InvalidOperation)
        }

        Ok(res)
    }
}
