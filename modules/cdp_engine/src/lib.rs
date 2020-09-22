//! # CDP Engine Module
//!
//! ## Overview
//!
//! The core module of Honzon protocol. CDP engine is responsible for handle
//! internal processes about CDPs, including liquidation, settlement and risk
//! management.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	debug, decl_error, decl_event, decl_module, decl_storage, ensure,
	traits::{EnsureOrigin, Get},
	weights::{constants::WEIGHT_PER_MICROS, DispatchClass},
};
use frame_system::{
	self as system, ensure_none,
	offchain::{SendTransactionTypes, SubmitTransaction},
};
use loans::Position;
use orml_traits::Change;
use orml_utilities::{with_transaction_result, IterableStorageDoubleMapExtended, OffchainErr};
use primitives::{Amount, Balance, CurrencyId};
use sp_runtime::{
	offchain::{
		storage::StorageValueRef,
		storage_lock::{StorageLock, Time},
		Duration,
	},
	traits::{BlakeTwo256, Bounded, Convert, Hash, Saturating, UniqueSaturatedInto, Zero},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity, ValidTransaction,
	},
	DispatchResult, FixedPointNumber, RandomNumberGenerator, RuntimeDebug,
};
use sp_std::{marker, prelude::*};
use support::{
	CDPTreasury, CDPTreasuryExtended, DEXManager, EmergencyShutdown, ExchangeRate, Price, PriceProvider, Rate, Ratio,
	RiskManager,
};

mod debit_exchange_rate_convertor;
pub use debit_exchange_rate_convertor::DebitExchangeRateConvertor;

mod mock;
mod tests;

const OFFCHAIN_WORKER_DATA: &[u8] = b"acala/cdp-engine/data/";
const OFFCHAIN_WORKER_LOCK: &[u8] = b"acala/cdp-engine/lock/";
const LOCK_DURATION: u64 = 100;
const MAX_ITERATIONS: u32 = 10000;

pub type LoansOf<T> = loans::Module<T>;

pub trait Trait: SendTransactionTypes<Call<Self>> + system::Trait + loans::Trait {
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// The origin which may update risk management parameters. Root can always
	/// do this.
	type UpdateOrigin: EnsureOrigin<Self::Origin>;

	/// The list of valid collateral currency types
	type CollateralCurrencyIds: Get<Vec<CurrencyId>>;

	/// The default liquidation ratio for all collateral types of CDP
	type DefaultLiquidationRatio: Get<Ratio>;

	/// The default debit exchange rate for all collateral types
	type DefaultDebitExchangeRate: Get<ExchangeRate>;

	/// The default liquidation penalty rate when liquidate unsafe CDP
	type DefaultLiquidationPenalty: Get<Rate>;

	/// The minimum debit value to avoid debit dust
	type MinimumDebitValue: Get<Balance>;

	/// Stablecoin currency id
	type GetStableCurrencyId: Get<CurrencyId>;

	/// The max slippage allowed when liquidate an unsafe CDP by swap with DEX
	type MaxSlippageSwapWithDEX: Get<Ratio>;

	/// The CDP treasury to maintain bad debts and surplus generated by CDPs
	type CDPTreasury: CDPTreasuryExtended<Self::AccountId, Balance = Balance, CurrencyId = CurrencyId>;

	/// The price source of all types of currencies related to CDP
	type PriceSource: PriceProvider<CurrencyId>;

	/// The DEX participating in liquidation
	type DEX: DEXManager<Self::AccountId, CurrencyId, Balance>;

	/// A configuration for base priority of unsigned transactions.
	///
	/// This is exposed so that it can be tuned for particular runtime, when
	/// multiple modules send unsigned transactions.
	type UnsignedPriority: Get<TransactionPriority>;

	/// Emergency shutdown.
	type EmergencyShutdown: EmergencyShutdown;
}

/// Liquidation strategy available
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq)]
pub enum LiquidationStrategy {
	/// Liquidation CDP's collateral by create collateral auction
	Auction,
	/// Liquidation CDP's collateral by swap with DEX
	Exchange,
}

/// Risk management params
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq, Default)]
pub struct RiskManagementParams {
	/// Maximum total debit value generated from it, when reach the hard cap,
	/// CDP's owner cannot issue more stablecoin under the collateral type.
	pub maximum_total_debit_value: Balance,

	/// Extra stability fee rate, `None` value means not set
	pub stability_fee: Option<Rate>,

	/// Liquidation ratio, when the collateral ratio of
	/// CDP under this collateral type is below the liquidation ratio, this CDP
	/// is unsafe and can be liquidated. `None` value means not set
	pub liquidation_ratio: Option<Ratio>,

	/// Liquidation penalty rate, when liquidation occurs,
	/// CDP will be deducted an additional penalty base on the product of
	/// penalty rate and debit value. `None` value means not set
	pub liquidation_penalty: Option<Rate>,

	/// Required collateral ratio, if it's set, cannot adjust the position of
	/// CDP so that the current collateral ratio is lower than the required
	/// collateral ratio. `None` value means not set
	pub required_collateral_ratio: Option<Ratio>,
}

// typedef to help polkadot.js disambiguate Change with different generic
// parameters
type ChangeOptionRate = Change<Option<Rate>>;
type ChangeOptionRatio = Change<Option<Ratio>>;
type ChangeBalance = Change<Balance>;

decl_event!(
	pub enum Event<T>
	where
		<T as system::Trait>::AccountId,
		CurrencyId = CurrencyId,
		Balance = Balance,
	{
		/// Liquidate the unsafe CDP. [collateral_type, owner, collateral_amount, bad_debt_value, liquidation_strategy]
		LiquidateUnsafeCDP(CurrencyId, AccountId, Balance, Balance, LiquidationStrategy),
		/// Settle the CDP has debit. [collateral_type, owner]
		SettleCDPInDebit(CurrencyId, AccountId),
		/// The stability fee for specific collateral type updated. [collateral_type, new_stability_fee]
		StabilityFeeUpdated(CurrencyId, Option<Rate>),
		/// The liquidation fee for specific collateral type updated. [collateral_type, new_liquidation_ratio]
		LiquidationRatioUpdated(CurrencyId, Option<Ratio>),
		/// The liquidation penalty rate for specific collateral type updated. [collateral_type, new_liquidation_panelty]
		LiquidationPenaltyUpdated(CurrencyId, Option<Rate>),
		/// The required collateral penalty rate for specific collateral type updated. [collateral_type, new_required_collateral_ratio]
		RequiredCollateralRatioUpdated(CurrencyId, Option<Ratio>),
		/// The hard cap of total debit value for specific collateral type updated. [collateral_type, new_total_debit_value]
		MaximumTotalDebitValueUpdated(CurrencyId, Balance),
		/// The global stability fee for all types of collateral updated. [new_global_stability_fee]
		GlobalStabilityFeeUpdated(Rate),
	}
);

decl_error! {
	/// Error for cdp engine module.
	pub enum Error for Module<T: Trait> {
		/// The total debit value of specific collateral type already exceed the hard cap
		ExceedDebitValueHardCap,
		/// The collateral ratio below the required collateral ratio
		BelowRequiredCollateralRatio,
		/// The collateral ratio below the liquidation ratio
		BelowLiquidationRatio,
		/// The CDP must be unsafe to be liquidated
		MustBeUnsafe,
		/// Invalid collateral type
		InvalidCollateralType,
		/// Remain debit value in CDP below the dust amount
		RemainDebitValueTooSmall,
		/// Feed price is invalid
		InvalidFeedPrice,
		/// No debit value in CDP so that it cannot be settled
		NoDebitValue,
		/// System has already been shutdown
		AlreadyShutdown,
		/// Must after system shutdown
		MustAfterShutdown,
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as CDPEngine {
		/// Mapping from collateral type to its exchange rate of debit units and debit value
		pub DebitExchangeRate get(fn debit_exchange_rate): map hasher(twox_64_concat) CurrencyId => Option<ExchangeRate>;

		/// Global stability fee rate for all types of collateral
		pub GlobalStabilityFee get(fn global_stability_fee) config(): Rate;

		/// Mapping from collateral type to its risk management params
		pub CollateralParams get(fn collateral_params): map hasher(twox_64_concat) CurrencyId => RiskManagementParams;
	}

	add_extra_genesis {
		#[allow(clippy::type_complexity)] // it's reasonable to use this one-off complex params config type
		config(collaterals_params): Vec<(CurrencyId, Option<Rate>, Option<Ratio>, Option<Rate>, Option<Ratio>, Balance)>;
		build(|config: &GenesisConfig| {
			config.collaterals_params.iter().for_each(|(
				currency_id,
				stability_fee,
				liquidation_ratio,
				liquidation_penalty,
				required_collateral_ratio,
				maximum_total_debit_value,
			)| {
				CollateralParams::insert(currency_id, RiskManagementParams {
					maximum_total_debit_value: *maximum_total_debit_value,
					stability_fee: *stability_fee,
					liquidation_ratio: *liquidation_ratio,
					liquidation_penalty: *liquidation_penalty,
					required_collateral_ratio: *required_collateral_ratio,
				});
			});
		});
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;
		fn deposit_event() = default;

		/// The list of valid collateral currency types
		const CollateralCurrencyIds: Vec<CurrencyId> = T::CollateralCurrencyIds::get();

		/// The minimum debit value allowed exists in CDP which has debit amount to avoid dust
		const MinimumDebitValue: Balance = T::MinimumDebitValue::get();

		/// The stable currency id
		const GetStableCurrencyId: CurrencyId = T::GetStableCurrencyId::get();

		/// The max slippage allowed when liquidate an unsafe CDP by swap with DEX
		const MaxSlippageSwapWithDEX: Ratio = T::MaxSlippageSwapWithDEX::get();

		/// The default liquidation ratio for all collateral types of CDP,
		/// if the liquidation ratio for specific collateral is `None`, it works.
		const DefaultLiquidationRatio: Ratio = T::DefaultLiquidationRatio::get();

		/// The default debit exchange rate for all collateral types,
		/// if the debit exchange rate for specific collateral is `None`, it works.
		const DefaultDebitExchangeRate: ExchangeRate = T::DefaultDebitExchangeRate::get();

		/// The default liquidation penalty rate when liquidate unsafe CDP,
		/// if the liquidation penalty rate for specific collateral is `None`, it works.
		const DefaultLiquidationPenalty: Rate = T::DefaultLiquidationPenalty::get();

		/// Liquidate unsafe CDP
		///
		/// The dispatch origin of this call must be _None_.
		///
		/// - `currency_id`: CDP's collateral type.
		/// - `who`: CDP's owner.
		///
		/// # <weight>
		/// - Preconditions:
		/// 	- T::CDPTreasury is module_cdp_treasury
		/// 	- T::DEX is module_dex
		/// - Complexity: `O(1)`
		/// - Db reads:
		///		- liquidate by auction: 19
		///		- liquidate by dex: 19
		/// - Db writes:
		///		- liquidate by auction: 14
		///		- liquidate by dex: 14
		/// -------------------
		/// Base Weight:
		///		- liquidate by auction: 200.1 µs
		///		- liquidate by dex: 325.3 µs
		/// # </weight>
		#[weight = (325 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(19, 14), DispatchClass::Operational)]
		pub fn liquidate(
			origin,
			currency_id: CurrencyId,
			who: T::AccountId,
		) {
			with_transaction_result(|| {
				ensure_none(origin)?;
				ensure!(!T::EmergencyShutdown::is_shutdown(), Error::<T>::AlreadyShutdown);
				Self::liquidate_unsafe_cdp(who, currency_id)?;
				Ok(())
			})?;
		}

		/// Settle CDP has debit after system shutdown
		///
		/// The dispatch origin of this call must be _None_.
		///
		/// - `currency_id`: CDP's collateral type.
		/// - `who`: CDP's owner.
		///
		/// # <weight>
		/// - Preconditions:
		/// 	- T::CDPTreasury is module_cdp_treasury
		/// 	- T::DEX is module_dex
		/// - Complexity: `O(1)`
		/// - Db reads: 10
		/// - Db writes: 6
		/// -------------------
		/// Base Weight: 161.5 µs
		/// # </weight>
		#[weight = (162 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(10, 6), DispatchClass::Operational)]
		pub fn settle(
			origin,
			currency_id: CurrencyId,
			who: T::AccountId,
		) {
			with_transaction_result(|| {
				ensure_none(origin)?;
				ensure!(T::EmergencyShutdown::is_shutdown(), Error::<T>::MustAfterShutdown);
				Self::settle_cdp_has_debit(who, currency_id)?;
				Ok(())
			})?;
		}

		/// Update global parameters related to risk management of CDP
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `global_stability_fee`: global stability fee rate.
		///
		/// # <weight>
		/// - Complexity: `O(1)`
		/// - Db reads: 0
		/// - Db writes: 1
		/// -------------------
		/// Base Weight: 24.16 µs
		/// # </weight>
		#[weight = (24 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(0, 1), DispatchClass::Operational)]
		pub fn set_global_params(
			origin,
			global_stability_fee: Rate,
		) {
			with_transaction_result(|| {
				T::UpdateOrigin::ensure_origin(origin)?;
				GlobalStabilityFee::put(global_stability_fee);
				Self::deposit_event(RawEvent::GlobalStabilityFeeUpdated(global_stability_fee));
				Ok(())
			})?;
		}

		/// Update parameters related to risk management of CDP under specific collateral type
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `currency_id`: collateral type.
		/// - `stability_fee`: extra stability fee rate, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `liquidation_ratio`: liquidation ratio, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `liquidation_penalty`: liquidation penalty, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `required_collateral_ratio`: required collateral ratio, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `maximum_total_debit_value`: maximum total debit value.
		///
		/// # <weight>
		/// - Complexity: `O(1)`
		/// - Db reads:	1
		/// - Db writes: 1
		/// -------------------
		/// Base Weight: 76.08 µs
		/// # </weight>
		#[weight = (76 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(1, 1), DispatchClass::Operational)]
		pub fn set_collateral_params(
			origin,
			currency_id: CurrencyId,
			stability_fee: ChangeOptionRate,
			liquidation_ratio: ChangeOptionRatio,
			liquidation_penalty: ChangeOptionRate,
			required_collateral_ratio: ChangeOptionRatio,
			maximum_total_debit_value: ChangeBalance,
		) {
			with_transaction_result(|| {
				T::UpdateOrigin::ensure_origin(origin)?;
				ensure!(
					T::CollateralCurrencyIds::get().contains(&currency_id),
					Error::<T>::InvalidCollateralType,
				);

				let mut collateral_params = Self::collateral_params(currency_id);
				if let Change::NewValue(update) = stability_fee {
					collateral_params.stability_fee = update;
					Self::deposit_event(RawEvent::StabilityFeeUpdated(currency_id, update));
				}
				if let Change::NewValue(update) = liquidation_ratio {
					collateral_params.liquidation_ratio = update;
					Self::deposit_event(RawEvent::LiquidationRatioUpdated(currency_id, update));
				}
				if let Change::NewValue(update) = liquidation_penalty {
					collateral_params.liquidation_penalty = update;
					Self::deposit_event(RawEvent::LiquidationPenaltyUpdated(currency_id, update));
				}
				if let Change::NewValue(update) = required_collateral_ratio {
					collateral_params.required_collateral_ratio = update;
					Self::deposit_event(RawEvent::RequiredCollateralRatioUpdated(currency_id, update));
				}
				if let Change::NewValue(val) = maximum_total_debit_value {
					collateral_params.maximum_total_debit_value = val;
					Self::deposit_event(RawEvent::MaximumTotalDebitValueUpdated(currency_id, val));
				}
				CollateralParams::insert(currency_id, collateral_params);
				Ok(())
			})?;
		}

		/// Issue interest in stable currency for all types of collateral has debit when block end,
		/// and update their debit exchange rate
		fn on_finalize(_now: T::BlockNumber) {
			// collect stability fee for all types of collateral
			if !T::EmergencyShutdown::is_shutdown() {
				for currency_id in T::CollateralCurrencyIds::get() {
					let debit_exchange_rate = Self::get_debit_exchange_rate(currency_id);
					let stability_fee_rate = Self::get_stability_fee(currency_id);
					let total_debits = <LoansOf<T>>::total_positions(currency_id).debit;
					if !stability_fee_rate.is_zero() && !total_debits.is_zero() {
						let debit_exchange_rate_increment = debit_exchange_rate.saturating_mul(stability_fee_rate);
						let total_debit_value = Self::get_debit_value(currency_id, total_debits);
						let issued_stable_coin_balance = debit_exchange_rate_increment.saturating_mul_int(total_debit_value);

						// issue stablecoin to surplus pool
						if <T as Trait>::CDPTreasury::on_system_surplus(issued_stable_coin_balance).is_ok() {
							// update exchange rate when issue success
							let new_debit_exchange_rate = debit_exchange_rate.saturating_add(debit_exchange_rate_increment);
							DebitExchangeRate::insert(currency_id, new_debit_exchange_rate);
						}
					}
				}
			}
		}

		/// Runs after every block. Start offchain worker to check CDP and
		/// submit unsigned tx to trigger liquidation or settlement.
		fn offchain_worker(now: T::BlockNumber) {
			if let Err(e) = Self::_offchain_worker() {
				debug::info!(
					target: "cdp-engine offchain worker",
					"cannot run offchain worker at {:?}: {:?}",
					now,
					e,
				);
			} else {
				debug::debug!(
					target: "cdp-engine offchain worker",
					"offchain worker start at block: {:?} already done!",
					now,
				);
			}
		}
	}
}

impl<T: Trait> Module<T> {
	fn submit_unsigned_liquidation_tx(currency_id: CurrencyId, who: T::AccountId) {
		let call = Call::<T>::liquidate(currency_id, who.clone());
		if SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into()).is_err() {
			debug::info!(
				target: "cdp-engine offchain worker",
				"submit unsigned liquidation tx for \nCDP - AccountId {:?} CurrencyId {:?} \nfailed!",
				who, currency_id,
			);
		}
	}

	fn submit_unsigned_settlement_tx(currency_id: CurrencyId, who: T::AccountId) {
		let call = Call::<T>::settle(currency_id, who.clone());
		if SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into()).is_err() {
			debug::info!(
				target: "cdp-engine offchain worker",
				"submit unsigned settlement tx for \nCDP - AccountId {:?} CurrencyId {:?} \nfailed!",
				who, currency_id,
			);
		}
	}

	fn _offchain_worker() -> Result<(), OffchainErr> {
		let collateral_currency_ids = T::CollateralCurrencyIds::get();
		if collateral_currency_ids.len().is_zero() {
			return Ok(());
		}

		// check if we are a potential validator
		if !sp_io::offchain::is_validator() {
			return Err(OffchainErr::NotValidator);
		}

		// acquire offchain worker lock
		let lock_expiration = Duration::from_millis(LOCK_DURATION);
		let mut lock = StorageLock::<'_, Time>::with_deadline(&OFFCHAIN_WORKER_LOCK, lock_expiration);
		let mut guard = lock.try_lock().map_err(|_| OffchainErr::OffchainLock)?;

		let collateral_currency_ids = T::CollateralCurrencyIds::get();
		let to_be_continue = StorageValueRef::persistent(&OFFCHAIN_WORKER_DATA);

		// get to_be_continue record
		let (collateral_position, start_key) =
			if let Some(Some((last_collateral_position, maybe_last_iterator_previous_key))) =
				to_be_continue.get::<(u32, Option<Vec<u8>>)>()
			{
				(last_collateral_position, maybe_last_iterator_previous_key)
			} else {
				let random_seed = sp_io::offchain::random_seed();
				let mut rng = RandomNumberGenerator::<BlakeTwo256>::new(BlakeTwo256::hash(&random_seed[..]));
				(
					rng.pick_u32(collateral_currency_ids.len().saturating_sub(1) as u32),
					None,
				)
			};

		let currency_id = collateral_currency_ids[(collateral_position as usize)];
		let is_shutdown = T::EmergencyShutdown::is_shutdown();

		let mut map_iterator = <loans::Positions<T> as IterableStorageDoubleMapExtended<_, _, _>>::iter_prefix(
			currency_id,
			Some(MAX_ITERATIONS),
			start_key,
		);
		while let Some((who, Position { collateral, debit })) = map_iterator.next() {
			if !is_shutdown && Self::is_cdp_unsafe(currency_id, collateral, debit) {
				// liquidate unsafe CDPs before emergency shutdown occurs
				Self::submit_unsigned_liquidation_tx(currency_id, who);
			} else if is_shutdown && !debit.is_zero() {
				// settle CDPs with debit after emergency shutdown occurs.
				Self::submit_unsigned_settlement_tx(currency_id, who);
			}

			// extend offchain worker lock
			guard.extend_lock().map_err(|_| OffchainErr::OffchainLock)?;
		}

		// if iteration for map storage finished, clear to be continue record
		// otherwise, update to be continue record
		if map_iterator.finished {
			let next_collateral_position =
				if collateral_position < collateral_currency_ids.len().saturating_sub(1) as u32 {
					collateral_position + 1
				} else {
					0
				};
			to_be_continue.set(&(next_collateral_position, Option::<Vec<u8>>::None));
		} else {
			to_be_continue.set(&(collateral_position, Some(map_iterator.map_iterator.previous_key)));
		}

		// Consume the guard but **do not** unlock the underlying lock.
		guard.forget();

		Ok(())
	}

	pub fn is_cdp_unsafe(currency_id: CurrencyId, collateral: Balance, debit: Balance) -> bool {
		let stable_currency_id = T::GetStableCurrencyId::get();

		if let Some(feed_price) = T::PriceSource::get_relative_price(currency_id, stable_currency_id) {
			let collateral_ratio = Self::calculate_collateral_ratio(currency_id, collateral, debit, feed_price);
			collateral_ratio < Self::get_liquidation_ratio(currency_id)
		} else {
			false
		}
	}

	pub fn maximum_total_debit_value(currency_id: CurrencyId) -> Balance {
		Self::collateral_params(currency_id).maximum_total_debit_value
	}

	pub fn required_collateral_ratio(currency_id: CurrencyId) -> Option<Ratio> {
		Self::collateral_params(currency_id).required_collateral_ratio
	}

	pub fn get_stability_fee(currency_id: CurrencyId) -> Rate {
		Self::collateral_params(currency_id)
			.stability_fee
			.unwrap_or_default()
			.saturating_add(Self::global_stability_fee())
	}

	pub fn get_liquidation_ratio(currency_id: CurrencyId) -> Ratio {
		Self::collateral_params(currency_id)
			.liquidation_ratio
			.unwrap_or_else(T::DefaultLiquidationRatio::get)
	}

	pub fn get_liquidation_penalty(currency_id: CurrencyId) -> Rate {
		Self::collateral_params(currency_id)
			.liquidation_penalty
			.unwrap_or_else(T::DefaultLiquidationPenalty::get)
	}

	pub fn get_debit_exchange_rate(currency_id: CurrencyId) -> ExchangeRate {
		Self::debit_exchange_rate(currency_id).unwrap_or_else(T::DefaultDebitExchangeRate::get)
	}

	pub fn get_debit_value(currency_id: CurrencyId, debit_balance: Balance) -> Balance {
		DebitExchangeRateConvertor::<T>::convert((currency_id, debit_balance))
	}

	pub fn calculate_collateral_ratio(
		currency_id: CurrencyId,
		collateral_balance: Balance,
		debit_balance: Balance,
		price: Price,
	) -> Ratio {
		let locked_collateral_value = price.saturating_mul_int(collateral_balance);
		let debit_value = Self::get_debit_value(currency_id, debit_balance);

		Ratio::checked_from_rational(locked_collateral_value, debit_value).unwrap_or_else(Rate::max_value)
	}

	pub fn adjust_position(
		who: &T::AccountId,
		currency_id: CurrencyId,
		collateral_adjustment: Amount,
		debit_adjustment: Amount,
	) -> DispatchResult {
		ensure!(
			T::CollateralCurrencyIds::get().contains(&currency_id),
			Error::<T>::InvalidCollateralType,
		);
		<LoansOf<T>>::adjust_position(who, currency_id, collateral_adjustment, debit_adjustment)?;
		Ok(())
	}

	// settle cdp has debit when emergency shutdown
	pub fn settle_cdp_has_debit(who: T::AccountId, currency_id: CurrencyId) -> DispatchResult {
		let Position { collateral, debit } = <LoansOf<T>>::positions(currency_id, &who);
		ensure!(!debit.is_zero(), Error::<T>::NoDebitValue);

		// confiscate collateral in cdp to cdp treasury
		// and decrease CDP's debit to zero
		let settle_price: Price = T::PriceSource::get_relative_price(T::GetStableCurrencyId::get(), currency_id)
			.ok_or(Error::<T>::InvalidFeedPrice)?;
		let bad_debt_value = Self::get_debit_value(currency_id, debit);
		let confiscate_collateral_amount =
			sp_std::cmp::min(settle_price.saturating_mul_int(bad_debt_value), collateral);

		// confiscate collateral and all debit
		<LoansOf<T>>::confiscate_collateral_and_debit(&who, currency_id, confiscate_collateral_amount, debit)?;

		Self::deposit_event(RawEvent::SettleCDPInDebit(currency_id, who));
		Ok(())
	}

	// liquidate unsafe cdp
	pub fn liquidate_unsafe_cdp(who: T::AccountId, currency_id: CurrencyId) -> DispatchResult {
		let Position { collateral, debit } = <LoansOf<T>>::positions(currency_id, &who);
		let stable_currency_id = T::GetStableCurrencyId::get();

		// ensure the cdp is unsafe
		ensure!(
			Self::is_cdp_unsafe(currency_id, collateral, debit),
			Error::<T>::MustBeUnsafe
		);

		// confiscate all collateral and debit of unsafe cdp to cdp treasury
		<LoansOf<T>>::confiscate_collateral_and_debit(&who, currency_id, collateral, debit)?;

		let bad_debt_value = Self::get_debit_value(currency_id, debit);
		let target_stable_amount = Self::get_liquidation_penalty(currency_id).saturating_mul_acc_int(bad_debt_value);
		let supply_collateral_amount = T::DEX::get_supply_amount(currency_id, stable_currency_id, target_stable_amount);

		// if collateral can swap enough native token in DEX and exchange
		// slippage is below the limit, directly exchange with DEX, otherwise create
		// collateral auctions.
		let liquidation_strategy: LiquidationStrategy = if !supply_collateral_amount.is_zero() 	// supply_collateral_amount must not be zero
			&& collateral >= supply_collateral_amount									// ensure have sufficient collateral
			&& T::DEX::get_exchange_slippage(currency_id, stable_currency_id, supply_collateral_amount).map_or(false, |s| s <= T::MaxSlippageSwapWithDEX::get())
		// slippage is acceptable
		{
			LiquidationStrategy::Exchange
		} else {
			LiquidationStrategy::Auction
		};

		match liquidation_strategy {
			LiquidationStrategy::Exchange => {
				<T as Trait>::CDPTreasury::swap_collateral_to_stable(
					currency_id,
					supply_collateral_amount,
					target_stable_amount,
				)?;

				// refund remain collateral to CDP owner
				let refund_collateral_amount = collateral
					.checked_sub(supply_collateral_amount)
					.expect("ensured collateral >= supply_collateral_amount on exchange; qed");
				<T as Trait>::CDPTreasury::withdraw_collateral(&who, currency_id, refund_collateral_amount)?;
			}
			LiquidationStrategy::Auction => {
				// create collateral auctions by cdp treasury
				<T as Trait>::CDPTreasury::create_collateral_auctions(
					currency_id,
					collateral,
					target_stable_amount,
					who.clone(),
					true,
				)?;
			}
		}

		Self::deposit_event(RawEvent::LiquidateUnsafeCDP(
			currency_id,
			who,
			collateral,
			bad_debt_value,
			liquidation_strategy,
		));
		Ok(())
	}
}

impl<T: Trait> RiskManager<T::AccountId, CurrencyId, Balance, Balance> for Module<T> {
	fn get_bad_debt_value(currency_id: CurrencyId, debit_balance: Balance) -> Balance {
		Self::get_debit_value(currency_id, debit_balance)
	}

	fn check_position_valid(
		currency_id: CurrencyId,
		collateral_balance: Balance,
		debit_balance: Balance,
	) -> DispatchResult {
		if !debit_balance.is_zero() {
			let debit_value = Self::get_debit_value(currency_id, debit_balance);
			let feed_price = <T as Trait>::PriceSource::get_relative_price(currency_id, T::GetStableCurrencyId::get())
				.ok_or(Error::<T>::InvalidFeedPrice)?;
			let collateral_ratio =
				Self::calculate_collateral_ratio(currency_id, collateral_balance, debit_balance, feed_price);

			// check the required collateral ratio
			if let Some(required_collateral_ratio) = Self::required_collateral_ratio(currency_id) {
				ensure!(
					collateral_ratio >= required_collateral_ratio,
					Error::<T>::BelowRequiredCollateralRatio
				);
			}

			// check the liquidation ratio
			ensure!(
				collateral_ratio >= Self::get_liquidation_ratio(currency_id),
				Error::<T>::BelowLiquidationRatio
			);

			// check the minimum_debit_value
			ensure!(
				debit_value >= T::MinimumDebitValue::get(),
				Error::<T>::RemainDebitValueTooSmall,
			);
		}

		Ok(())
	}

	fn check_debit_cap(currency_id: CurrencyId, total_debit_balance: Balance) -> DispatchResult {
		let hard_cap = Self::maximum_total_debit_value(currency_id);
		let total_debit_value = Self::get_debit_value(currency_id, total_debit_balance);

		ensure!(total_debit_value <= hard_cap, Error::<T>::ExceedDebitValueHardCap,);

		Ok(())
	}
}

#[allow(deprecated)]
impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
		match call {
			Call::liquidate(currency_id, who) => {
				let Position { collateral, debit } = <LoansOf<T>>::positions(currency_id, &who);
				if !Self::is_cdp_unsafe(*currency_id, collateral, debit) || T::EmergencyShutdown::is_shutdown() {
					return InvalidTransaction::Stale.into();
				}

				ValidTransaction::with_tag_prefix("CDPEngineOffchainWorker")
					.priority(T::UnsignedPriority::get())
					.and_provides((<system::Module<T>>::block_number(), currency_id, who))
					.longevity(64_u64)
					.propagate(true)
					.build()
			}
			Call::settle(currency_id, who) => {
				let Position { debit, .. } = <LoansOf<T>>::positions(currency_id, who);
				if debit.is_zero() || !T::EmergencyShutdown::is_shutdown() {
					return InvalidTransaction::Stale.into();
				}

				ValidTransaction::with_tag_prefix("CDPEngineOffchainWorker")
					.priority(T::UnsignedPriority::get())
					.and_provides((currency_id, who))
					.longevity(64_u64)
					.propagate(true)
					.build()
			}
			_ => InvalidTransaction::Call.into(),
		}
	}
}
