use std::{fs::File, time::{Duration, Instant}};

use anchor_lang::AccountDeserialize;
use clearing_house::{math::{collateral::calculate_updated_collateral, constants::{AMM_TO_QUOTE_PRECISION_RATIO_I128, MARGIN_PRECISION}, funding::calculate_funding_payment, position::calculate_base_asset_value_and_pnl}, state::{market::{Markets, AMM}, state::State, user::{User, UserPositions}}, error::ClearingHouseResult};
use config::{CLI_URL, KEYFILE_PATH};
use rayon::{iter::{IntoParallelRefMutIterator, ParallelIterator}, join};
use solana_client::{rpc_client::RpcClient};
use solana_sdk::{commitment_config::{CommitmentConfig}, instruction::{AccountMeta, Instruction}, pubkey::Pubkey, signature::Keypair, signer::Signer, transaction::Transaction};

mod config;

fn main() {
    let timeout = Duration::from_secs(45);
    let commitment_config = CommitmentConfig::processed();
    let client = RpcClient::new_with_timeout_and_commitment(
        CLI_URL.to_string(),
        timeout,
        commitment_config,
    );
    // fee payer and transaction signer keypair
    let payer: Keypair = solana_sdk::signer::keypair::read_keypair(&mut File::open(KEYFILE_PATH).unwrap()).unwrap();
    println!("liquidator account {}", bs58::encode(payer.pubkey().to_bytes()).into_string());
    let mut liquidator_drift_account = Pubkey::default();

    let now = Instant::now();
    let mut users: Vec<(Pubkey, User)> = vec![];
    let mut markets = (Pubkey::default(),  Markets::default());
    let mut state = (Pubkey::default(), State::default());

    let all_accounts = client.get_program_accounts(&clearing_house::id()).unwrap();

    for account in &all_accounts {
        // try deserializing into a user account
        let user_account = User::try_deserialize(&mut &*account.1.data);
        if !user_account.is_err() {
            let user_account = user_account.unwrap();

            if user_account.authority == payer.pubkey() {
                liquidator_drift_account = account.0;
                println!("liquidator drift account {}", bs58::encode(account.0.to_bytes()).into_string());
            }
            users.push((account.0, user_account));
            continue;
        }

        let markets_account = Markets::try_deserialize(&mut &*account.1.data);
        if !markets_account.is_err() {
            markets = (account.0, markets_account.unwrap());
            continue;
        }

        let state_account = State::try_deserialize(&mut &*account.1.data);
        if !state_account.is_err() {
            state = (account.0, state_account.unwrap());
            continue;
        }
    }

    let elapsed = now.elapsed();
    println!("loaded {} user accounts from a total of {} accounts in {:.2?}", users.len(), all_accounts.len(), elapsed);

    loop {
        // reload markets and funding payment history
        markets = (markets.0, Markets::try_deserialize(&mut &*client.get_account_data(&markets.0).unwrap()).unwrap());
        // loop over all users
        users.par_iter_mut().for_each(|mut user| {
            let (user_postitions_data, user_account_data) = join(|| client.get_account_data(&user.1.positions), || client.get_account_data(&user.0));

            if user_postitions_data.is_err() || user_account_data.is_err() {
                println!("failed to get account data for account {}", bs58::encode(user.0.to_bytes()).into_string());
                return;
            }
            let mut user_positions = UserPositions::try_deserialize(&mut &*user_postitions_data.unwrap()).unwrap();
            user.1 = User::try_deserialize(&mut &*user_account_data.unwrap()).unwrap();

            // Settle user's funding payments so that collateral is up to date
            settle_funding_payment(
                &mut user.1,
                &mut user_positions,
                &markets.1,
            ).unwrap();

            // Verify that the user is in liquidation territory
            let (_total_collateral, _unrealized_pnl, _base_asset_value, margin_ratio) =
                calculate_margin_ratio(&user.1, &mut user_positions, &markets.1).unwrap();
            // is liquidatable
            if margin_ratio <= state.1.margin_ratio_partial {
                let mut accounts = vec![
                    AccountMeta::new_readonly(state.0, false),
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new(liquidator_drift_account, false),
                    AccountMeta::new(user.0, false),
                    AccountMeta::new(state.1.collateral_vault, false),
                    AccountMeta::new_readonly(state.1.collateral_vault_authority, false),
                    AccountMeta::new(state.1.insurance_vault, false),
                    AccountMeta::new_readonly(state.1.insurance_vault_authority, false),
                    AccountMeta::new_readonly(spl_token::id(), false),
                    AccountMeta::new(state.1.markets, false),
                    AccountMeta::new(user.1.positions, false),
                    AccountMeta::new(state.1.trade_history, false),
                    AccountMeta::new(state.1.liquidation_history, false),
                    AccountMeta::new(state.1.funding_payment_history, false),
                ];

                for position in user_positions.positions {
                    if position.base_asset_amount != 0 {
                        let market = markets.1.markets[position.market_index as usize];
                        accounts.push(AccountMeta::new_readonly(market.amm.oracle, false));
                    }
                }

                let liquidate_instruction = Instruction {
                    program_id: clearing_house::id(),
                    accounts,
                    data: hex::decode("dfb3e27d302e274a").unwrap(),
                };

                let liquidate_transaction = Transaction::new_signed_with_payer(
                    &*vec![liquidate_instruction],
                    Some(&payer.pubkey()),
                    &vec![&payer],
                    client.get_recent_blockhash().unwrap().0,
                );
                // println!("tx size: {}", liquidate_transaction.message.serialize().len());
                client.send_transaction(&liquidate_transaction);

                println!("liquidated account {}", bs58::encode(user.0.to_bytes()).into_string());
                user.1 = User::try_deserialize(&mut &*client.get_account_data(&user.0).unwrap()).unwrap();
            }
        });
    }
}

// stripped down internal functions

/// Funding payments are settled lazily. The amm tracks its cumulative funding rate (for longs and shorts)
/// and the user's market position tracks how much funding the user been cumulatively paid for that market.
/// If the two values are not equal, the user owes/is owed funding.
fn settle_funding_payment(
    user: &mut User,
    user_positions: &mut UserPositions,
    markets: &Markets,
) -> ClearingHouseResult {
    let mut funding_payment: i128 = 0;
    for market_position in user_positions.positions.iter_mut() {
        if market_position.base_asset_amount == 0 {
            continue;
        }

        let market = &markets.markets[Markets::index_from_u64(market_position.market_index)];
        let amm: &AMM = &market.amm;

        let amm_cumulative_funding_rate = if market_position.base_asset_amount > 0 {
            amm.cumulative_funding_rate_long
        } else {
            amm.cumulative_funding_rate_short
        };

        if amm_cumulative_funding_rate != market_position.last_cumulative_funding_rate {
            let market_funding_rate_payment =
                calculate_funding_payment(amm_cumulative_funding_rate, market_position)?;

            funding_payment = funding_payment
                .checked_add(market_funding_rate_payment)
                .unwrap();

            market_position.last_cumulative_funding_rate = amm_cumulative_funding_rate;
            market_position.last_funding_rate_ts = amm.last_funding_rate_ts;
        }
    }

    let funding_payment_collateral = funding_payment
        .checked_div(AMM_TO_QUOTE_PRECISION_RATIO_I128)
        .unwrap();

    user.collateral = calculate_updated_collateral(user.collateral, funding_payment_collateral)?;

    Ok(())
}

fn calculate_margin_ratio(
    user: &User,
    user_positions: &mut UserPositions,
    markets: &Markets,
) -> ClearingHouseResult<(u128, i128, u128, u128)> {
    let mut base_asset_value: u128 = 0;
    let mut unrealized_pnl: i128 = 0;

    // loop 1 to calculate unrealized_pnl
    for market_position in user_positions.positions.iter() {
        if market_position.base_asset_amount == 0 {
            continue;
        }

        let amm = &markets.markets[Markets::index_from_u64(market_position.market_index)].amm;
        let (position_base_asset_value, position_unrealized_pnl) =
            calculate_base_asset_value_and_pnl(market_position, amm)?;

        base_asset_value = base_asset_value
            .checked_add(position_base_asset_value)
            .unwrap();
        unrealized_pnl = unrealized_pnl
            .checked_add(position_unrealized_pnl)
            .unwrap();
    }

    let total_collateral: u128;
    let margin_ratio: u128;
    if base_asset_value == 0 {
        total_collateral = u128::MAX;
        margin_ratio = u128::MAX;
    } else {
        total_collateral = calculate_updated_collateral(user.collateral, unrealized_pnl)?;
        margin_ratio = total_collateral
            .checked_mul(MARGIN_PRECISION)
            .unwrap()
            .checked_div(base_asset_value)
            .unwrap();
    }

    Ok((
        total_collateral,
        unrealized_pnl,
        base_asset_value,
        margin_ratio,
    ))
}