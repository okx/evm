//! Gasless transaction fee hooks.

use alloy_evm::{Evm, EvmFactory};
use revm::context_interface::result::ResultAndState;

/// Previous fee-check cfg value saved while a gasless transaction executes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpFeeCheckState {
    /// Previous base-fee validation flag.
    pub disable_base_fee: bool,
}

/// Temporarily disables the base-fee check for a single gasless tx.
///
/// A gasless tx executes with `effective_gas_price == 0` (see `OpTransaction::is_gasless`), which
/// upstream `revm`'s `validate_tx_env` rejects with `GasPriceLessThanBasefee` unless
/// `cfg.disable_base_fee` is set. Rather than fork that upstream validation, this flips the flag
/// for the duration of the tx and restores it afterwards.
///
/// It operates on the concrete [`OpEvm`] (not inside the OP handler) because the handler is
/// generic over `OpContextTr`, which exposes `cfg` read-only — only the concrete EVM context
/// offers `modify_cfg`. Every other gasless fee decision (skipping the fee charge, suppressing
/// the gas refund, the caller reimbursement and the beneficiary reward) is made in the handler by
/// reading `tx.is_gasless()`, so `disable_base_fee` is the only flag this needs to touch: gasless
/// paths never reach `calculate_caller_fee`, so setting `disable_fee_charge` would be redundant.
pub trait GaslessFeeHook<E: Evm> {
    /// Disables the base-fee check if `disabled` is true, returning the previous cfg state.
    fn disable_gasless_fee_checks(evm: &mut E, disabled: bool) -> Option<OpFeeCheckState>;

    /// Restores a previous cfg state returned by [`GaslessFeeHook::disable_gasless_fee_checks`].
    fn restore_gasless_fee_checks(evm: &mut E, previous: Option<OpFeeCheckState>);

    /// Executes `tx` with the base-fee check disabled when `disabled` is true.
    fn transact_with_gasless_fee_checks(
        evm: &mut E,
        tx: E::Tx,
        disabled: bool,
    ) -> Result<ResultAndState<E::HaltReason>, E::Error> {
        let previous = Self::disable_gasless_fee_checks(evm, disabled);
        let result = evm.transact_raw(tx);
        Self::restore_gasless_fee_checks(evm, previous);
        result
    }
}

/// Marker trait satisfied by every [`EvmFactory`].
///
/// Previously this carried an associated `Hook` type threaded through [`OpBlockExecutorFactory`].
/// The hook type is now hardcoded to [`XLayerGaslessFeeHook`] in `create_executor`; this blanket
/// marker ensures code that references the bound still compiles.
pub trait XLayerGaslessFeeHookFactory: EvmFactory {}
impl<F: EvmFactory> XLayerGaslessFeeHookFactory for F {}

/// Gasless fee hook for the standard [`OpEvm`].
///
/// Implements [`GaslessFeeHook`] for ALL [`Evm`] types. For EVM types that expose a
/// `set_base_fee_check_disabled` method (see [`Evm::set_base_fee_check_disabled`]), the
/// base-fee validation check is disabled for the duration of the gasless transaction.
/// For other EVM types the hook is a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct XLayerGaslessFeeHook;

impl<E: Evm> GaslessFeeHook<E> for XLayerGaslessFeeHook {
    fn disable_gasless_fee_checks(evm: &mut E, disabled: bool) -> Option<OpFeeCheckState> {
        if !disabled {
            return None;
        }
        evm.set_base_fee_check_disabled(true)
            .map(|prev| OpFeeCheckState { disable_base_fee: prev })
    }

    fn restore_gasless_fee_checks(evm: &mut E, previous: Option<OpFeeCheckState>) {
        let Some(previous) = previous else {
            return;
        };
        evm.set_base_fee_check_disabled(previous.disable_base_fee);
    }
}

#[cfg(all(test, feature = "optional_no_base_fee"))]
mod xlayer_tests {
    use alloy_evm::{Evm, EvmEnv, EvmFactory};
    use op_revm::{OpSpecId, OpTransaction};
    use revm::{
        context::{BlockEnv, CfgEnv, TxEnv},
        database::EmptyDB,
    };

    use crate::OpEvmFactory;
    use super::*;

    #[test]
    fn test_gasless_fee_cfg_bypasses_fee_validation() {
        let env = EvmEnv::new(
            CfgEnv::new_with_spec(OpSpecId::REGOLITH),
            BlockEnv { basefee: 100, gas_limit: 30_000, ..Default::default() },
        );
        let tx = OpTransaction::builder()
            .base(TxEnv::builder().gas_limit(21_000).gas_price(0))
            .build_fill();

        let mut evm = OpEvmFactory::default().create_evm(EmptyDB::default(), env.clone());
        assert!(evm.transact_raw(tx.clone()).is_err());

        let mut evm = OpEvmFactory::default().create_evm(EmptyDB::default(), env);
        assert!(XLayerGaslessFeeHook::transact_with_gasless_fee_checks(
            &mut evm,
            OpTransaction { is_gasless: true, ..tx },
            true,
        )
        .is_ok());
        // Hook must restore the flag after the transaction.
        assert!(!evm.ctx().cfg.disable_base_fee);
    }

    #[test]
    fn test_gasless_fee_cfg_keeps_block_gas_limit_validation() {
        let mut evm = OpEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(
                CfgEnv::new_with_spec(OpSpecId::REGOLITH),
                BlockEnv { basefee: 100, gas_limit: 20_000, ..Default::default() },
            ),
        );
        let tx = OpTransaction::builder()
            .base(TxEnv::builder().gas_limit(21_000).gas_price(0))
            .gasless(true)
            .build_fill();

        assert!(XLayerGaslessFeeHook::transact_with_gasless_fee_checks(&mut evm, tx, true).is_err());
        assert!(!evm.ctx().cfg.disable_base_fee);
    }
}
