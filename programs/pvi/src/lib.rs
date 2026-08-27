use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount, TransferChecked};

declare_id!("7PKXznczrtwCSSYqMEhgFjvJqxpnpgGMZUX4RTy3XVgb");

const BPS: u128 = 10_000;
const TOKEN_DECIMALS: u8 = 6;

#[program]
pub mod pvi {
    use super::*;

    pub fn initialize_protocol(
        ctx: Context<InitializeProtocol>,
        risk: RiskParameters,
    ) -> Result<()> {
        risk.validate()?;
        let protocol = &mut ctx.accounts.protocol;
        protocol.authority = ctx.accounts.authority.key();
        protocol.pending_authority = Pubkey::default();
        protocol.pai_mint = Pubkey::default();
        protocol.collateral_mint = Pubkey::default();
        protocol.pvi_mint = Pubkey::default();
        protocol.collateral_vault = Pubkey::default();
        protocol.risk = risk;
        protocol.total_debt = 0;
        protocol.bad_debt = 0;
        protocol.paused = true;
        protocol.pai_bound = false;
        protocol.assets_initialized = false;
        protocol.bump = ctx.bumps.protocol;
        Ok(())
    }

    pub fn initialize_assets(ctx: Context<InitializeAssets>) -> Result<()> {
        let protocol = &mut ctx.accounts.protocol;
        require!(protocol.paused, PviError::MustBePaused);
        require!(
            !protocol.assets_initialized,
            PviError::AssetsAlreadyInitialized
        );
        require!(
            ctx.accounts.collateral_mint.decimals == TOKEN_DECIMALS,
            PviError::InvalidDecimals
        );
        protocol.collateral_mint = ctx.accounts.collateral_mint.key();
        protocol.pvi_mint = ctx.accounts.pvi_mint.key();
        protocol.collateral_vault = ctx.accounts.collateral_vault.key();
        protocol.assets_initialized = true;
        Ok(())
    }

    pub fn initialize_oracle(ctx: Context<InitializeOracle>, reporter: Pubkey) -> Result<()> {
        require!(reporter != Pubkey::default(), PviError::InvalidAuthority);
        let oracle = &mut ctx.accounts.oracle;
        oracle.reporter = reporter;
        oracle.status = OracleStatus::Active;
        oracle.sequence = 0;
        oracle.target_price_micros = 0;
        oracle.previous_target_price_micros = 0;
        oracle.latest_volume_usd_micros = 0;
        oracle.last_update_timestamp = 0;
        oracle.bump = ctx.bumps.oracle;
        Ok(())
    }

    pub fn update_oracle(ctx: Context<UpdateOracle>, update: OracleUpdate) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let oracle = &mut ctx.accounts.oracle;
        require!(
            oracle.status == OracleStatus::Active,
            PviError::OraclePaused
        );
        require!(update.sequence > oracle.sequence, PviError::Replay);
        require!(
            update.observed_at > oracle.last_update_timestamp && update.observed_at <= now,
            PviError::InvalidTimestamp
        );
        require!(
            now.checked_sub(update.observed_at)
                .ok_or(PviError::InvalidTimestamp)?
                <= ctx.accounts.protocol.risk.max_oracle_age_seconds,
            PviError::StaleOracle
        );
        require!(update.target_price_micros > 0, PviError::InvalidTarget);
        if oracle.target_price_micros > 0 {
            let delta = update
                .target_price_micros
                .abs_diff(oracle.target_price_micros) as u128;
            let change_bps = checked_mul_div(delta, BPS, oracle.target_price_micros as u128)?;
            require!(
                change_bps <= ctx.accounts.protocol.risk.max_update_bps as u128,
                PviError::UpdateTooLarge
            );
        }
        oracle.previous_target_price_micros = oracle.target_price_micros;
        oracle.target_price_micros = update.target_price_micros;
        oracle.latest_volume_usd_micros = update.eligible_volume_usd_micros;
        oracle.last_update_timestamp = update.observed_at;
        oracle.sequence = update.sequence;
        Ok(())
    }

    pub fn open_position(ctx: Context<OpenPosition>) -> Result<()> {
        let position = &mut ctx.accounts.position;
        position.owner = ctx.accounts.owner.key();
        position.collateral_micros = 0;
        position.debt_pvi_micros = 0;
        position.accrued_fees_micros = 0;
        position.bump = ctx.bumps.position;
        Ok(())
    }

    pub fn deposit_collateral(ctx: Context<DepositCollateral>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        token::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.owner_collateral.to_account_info(),
                    mint: ctx.accounts.collateral_mint.to_account_info(),
                    to: ctx.accounts.collateral_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            TOKEN_DECIMALS,
        )?;
        ctx.accounts.position.collateral_micros = ctx
            .accounts
            .position
            .collateral_micros
            .checked_add(amount)
            .ok_or(PviError::Overflow)?;
        Ok(())
    }

    pub fn mint_pvi(ctx: Context<MintPvi>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        ctx.accounts.assert_risk_actions_allowed()?;
        let fee = fee_amount(amount, ctx.accounts.protocol.risk.mint_fee_bps)?;
        let debt_increase = amount.checked_add(fee).ok_or(PviError::Overflow)?;
        let next_debt = ctx
            .accounts
            .position
            .debt_pvi_micros
            .checked_add(debt_increase)
            .ok_or(PviError::Overflow)?;
        require!(
            ctx.accounts.position.collateral_micros
                >= ctx.accounts.protocol.risk.minimum_collateral_micros,
            PviError::BelowMinimumCollateral
        );
        require!(
            healthy(
                ctx.accounts.position.collateral_micros,
                next_debt,
                ctx.accounts.oracle.target_price_micros,
                ctx.accounts.protocol.risk.min_collateral_ratio_bps
            )?,
            PviError::Unhealthy
        );
        require!(
            next_debt <= ctx.accounts.protocol.risk.wallet_debt_ceiling_pvi_micros,
            PviError::WalletDebtCeiling
        );
        let next_total = ctx
            .accounts
            .protocol
            .total_debt
            .checked_add(debt_increase)
            .ok_or(PviError::Overflow)?;
        require!(
            next_total <= ctx.accounts.protocol.risk.protocol_debt_ceiling_pvi_micros,
            PviError::ProtocolDebtCeiling
        );
        let bump = [ctx.accounts.protocol.bump];
        let signer: &[&[u8]] = &[b"protocol", &bump];
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.pvi_mint.to_account_info(),
                    to: ctx.accounts.owner_pvi.to_account_info(),
                    authority: ctx.accounts.protocol.to_account_info(),
                },
                &[signer],
            ),
            amount,
        )?;
        ctx.accounts.position.debt_pvi_micros = next_debt;
        ctx.accounts.position.accrued_fees_micros = ctx
            .accounts
            .position
            .accrued_fees_micros
            .checked_add(fee)
            .ok_or(PviError::Overflow)?;
        ctx.accounts.protocol.total_debt = next_total;
        emit!(PviMinted {
            owner: ctx.accounts.owner.key(),
            amount,
            fee
        });
        Ok(())
    }

    pub fn repay_debt(ctx: Context<RepayDebt>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        let paid = amount.min(ctx.accounts.position.debt_pvi_micros);
        require!(paid > 0, PviError::NoDebt);
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.pvi_mint.to_account_info(),
                    from: ctx.accounts.owner_pvi.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            paid,
        )?;
        ctx.accounts.position.debt_pvi_micros = ctx
            .accounts
            .position
            .debt_pvi_micros
            .checked_sub(paid)
            .ok_or(PviError::Overflow)?;
        ctx.accounts.position.accrued_fees_micros = ctx
            .accounts
            .position
            .accrued_fees_micros
            .saturating_sub(paid);
        ctx.accounts.protocol.total_debt = ctx
            .accounts
            .protocol
            .total_debt
            .checked_sub(paid)
            .ok_or(PviError::Overflow)?;
        emit!(DebtRepaid {
            owner: ctx.accounts.owner.key(),
            amount: paid
        });
        Ok(())
    }

    pub fn withdraw_collateral(ctx: Context<WithdrawCollateral>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        ctx.accounts.assert_risk_actions_allowed()?;
        let remaining = ctx
            .accounts
            .position
            .collateral_micros
            .checked_sub(amount)
            .ok_or(PviError::InsufficientCollateral)?;
        require!(
            healthy(
                remaining,
                ctx.accounts.position.debt_pvi_micros,
                ctx.accounts.oracle.target_price_micros,
                ctx.accounts.protocol.risk.min_collateral_ratio_bps
            )?,
            PviError::Unhealthy
        );
        transfer_from_vault(
            &ctx.accounts.protocol,
            &ctx.accounts.collateral_vault,
            &ctx.accounts.owner_collateral,
            &ctx.accounts.collateral_mint,
            &ctx.accounts.token_program,
            amount,
        )?;
        ctx.accounts.position.collateral_micros = remaining;
        Ok(())
    }

    pub fn liquidate(ctx: Context<Liquidate>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        assert_oracle_fresh(&ctx.accounts.protocol, &ctx.accounts.oracle)?;
        require!(
            !healthy(
                ctx.accounts.position.collateral_micros,
                ctx.accounts.position.debt_pvi_micros,
                ctx.accounts.oracle.target_price_micros,
                ctx.accounts.protocol.risk.liquidation_ratio_bps
            )?,
            PviError::NotLiquidatable
        );
        let repaid = amount.min(ctx.accounts.position.debt_pvi_micros);
        let debt_value = usd_value(repaid, ctx.accounts.oracle.target_price_micros)?;
        let seize = checked_mul_div(
            debt_value as u128,
            BPS + ctx.accounts.protocol.risk.liquidation_penalty_bps as u128,
            BPS,
        )? as u64;
        require!(
            seize <= ctx.accounts.position.collateral_micros,
            PviError::LiquidationTooLarge
        );
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.pvi_mint.to_account_info(),
                    from: ctx.accounts.liquidator_pvi.to_account_info(),
                    authority: ctx.accounts.liquidator.to_account_info(),
                },
            ),
            repaid,
        )?;
        transfer_from_vault(
            &ctx.accounts.protocol,
            &ctx.accounts.collateral_vault,
            &ctx.accounts.liquidator_collateral,
            &ctx.accounts.collateral_mint,
            &ctx.accounts.token_program,
            seize,
        )?;
        ctx.accounts.position.debt_pvi_micros = ctx
            .accounts
            .position
            .debt_pvi_micros
            .checked_sub(repaid)
            .ok_or(PviError::Overflow)?;
        ctx.accounts.position.collateral_micros = ctx
            .accounts
            .position
            .collateral_micros
            .checked_sub(seize)
            .ok_or(PviError::Overflow)?;
        ctx.accounts.protocol.total_debt = ctx
            .accounts
            .protocol
            .total_debt
            .checked_sub(repaid)
            .ok_or(PviError::Overflow)?;
        if ctx.accounts.position.collateral_micros == 0 && ctx.accounts.position.debt_pvi_micros > 0
        {
            let bad = ctx.accounts.position.debt_pvi_micros;
            ctx.accounts.position.debt_pvi_micros = 0;
            ctx.accounts.protocol.total_debt = ctx
                .accounts
                .protocol
                .total_debt
                .checked_sub(bad)
                .ok_or(PviError::Overflow)?;
            ctx.accounts.protocol.bad_debt = ctx
                .accounts
                .protocol
                .bad_debt
                .checked_add(bad)
                .ok_or(PviError::Overflow)?;
        }
        emit!(PositionLiquidated {
            owner: ctx.accounts.position.owner,
            liquidator: ctx.accounts.liquidator.key(),
            repaid,
            collateral_seized: seize
        });
        Ok(())
    }

    pub fn settle_bad_debt(ctx: Context<SettleBadDebt>, amount: u64) -> Result<()> {
        require!(amount > 0, PviError::AmountZero);
        let settled = amount.min(ctx.accounts.protocol.bad_debt);
        require!(settled > 0, PviError::NoBadDebt);
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.pvi_mint.to_account_info(),
                    from: ctx.accounts.payer_pvi.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            settled,
        )?;
        ctx.accounts.protocol.bad_debt = ctx
            .accounts
            .protocol
            .bad_debt
            .checked_sub(settled)
            .ok_or(PviError::Overflow)?;
        emit!(BadDebtSettled {
            payer: ctx.accounts.payer.key(),
            amount: settled,
        });
        Ok(())
    }

    pub fn update_risk_parameters(ctx: Context<Admin>, risk: RiskParameters) -> Result<()> {
        require!(ctx.accounts.protocol.paused, PviError::MustBePaused);
        risk.validate()?;
        ctx.accounts.protocol.risk = risk;
        Ok(())
    }

    pub fn bind_pai_mint(ctx: Context<Admin>, pai_mint: Pubkey) -> Result<()> {
        require!(ctx.accounts.protocol.paused, PviError::MustBePaused);
        require!(!ctx.accounts.protocol.pai_bound, PviError::PaiAlreadyBound);
        require!(pai_mint != Pubkey::default(), PviError::InvalidMint);
        ctx.accounts.protocol.pai_mint = pai_mint;
        ctx.accounts.protocol.pai_bound = true;
        Ok(())
    }

    pub fn pause_protocol(ctx: Context<Admin>) -> Result<()> {
        ctx.accounts.protocol.paused = true;
        Ok(())
    }
    pub fn resume_protocol(ctx: Context<Admin>) -> Result<()> {
        require!(ctx.accounts.protocol.pai_bound, PviError::PaiNotBound);
        require!(
            ctx.accounts.protocol.assets_initialized,
            PviError::AssetsNotInitialized
        );
        require!(
            ctx.accounts.protocol.bad_debt == 0,
            PviError::OutstandingBadDebt
        );
        ctx.accounts.protocol.paused = false;
        Ok(())
    }
    pub fn set_reporter(ctx: Context<AdminOracle>, reporter: Pubkey) -> Result<()> {
        require!(reporter != Pubkey::default(), PviError::InvalidAuthority);
        ctx.accounts.oracle.reporter = reporter;
        Ok(())
    }
    pub fn transfer_authority(ctx: Context<Admin>, next: Pubkey) -> Result<()> {
        require!(next != Pubkey::default(), PviError::InvalidAuthority);
        ctx.accounts.protocol.pending_authority = next;
        Ok(())
    }
    pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
        ctx.accounts.protocol.authority = ctx.accounts.pending_authority.key();
        ctx.accounts.protocol.pending_authority = Pubkey::default();
        Ok(())
    }
}

fn checked_mul_div(value: u128, multiplier: u128, divisor: u128) -> Result<u128> {
    value
        .checked_mul(multiplier)
        .ok_or(PviError::Overflow)?
        .checked_div(divisor)
        .ok_or_else(|| error!(PviError::Overflow))
}
fn fee_amount(amount: u64, bps: u16) -> Result<u64> {
    checked_mul_div(amount as u128, bps as u128, BPS)?
        .try_into()
        .map_err(|_| error!(PviError::Overflow))
}
fn usd_value(pvi: u64, target: u64) -> Result<u64> {
    checked_mul_div(pvi as u128, target as u128, 1_000_000)?
        .try_into()
        .map_err(|_| error!(PviError::Overflow))
}
fn healthy(collateral: u64, debt: u64, target: u64, ratio: u16) -> Result<bool> {
    if debt == 0 {
        return Ok(true);
    }
    let value = usd_value(debt, target)? as u128;
    Ok((collateral as u128) * BPS >= value * ratio as u128)
}
fn assert_oracle_fresh(protocol: &Protocol, oracle: &Oracle) -> Result<()> {
    require!(
        oracle.status == OracleStatus::Active,
        PviError::OraclePaused
    );
    require!(oracle.target_price_micros > 0, PviError::InvalidTarget);
    let age = Clock::get()?
        .unix_timestamp
        .checked_sub(oracle.last_update_timestamp)
        .ok_or(PviError::InvalidTimestamp)?;
    require!(
        age <= protocol.risk.max_oracle_age_seconds,
        PviError::StaleOracle
    );
    Ok(())
}

fn transfer_from_vault<'info>(
    protocol: &Account<'info, Protocol>,
    vault: &Account<'info, TokenAccount>,
    destination: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let bump = [protocol.bump];
    let signer: &[&[u8]] = &[b"protocol", &bump];
    token::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: vault.to_account_info(),
                mint: mint.to_account_info(),
                to: destination.to_account_info(),
                authority: protocol.to_account_info(),
            },
            &[signer],
        ),
        amount,
        TOKEN_DECIMALS,
    )
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct RiskParameters {
    pub min_collateral_ratio_bps: u16,
    pub liquidation_ratio_bps: u16,
    pub liquidation_penalty_bps: u16,
    pub mint_fee_bps: u16,
    pub redemption_fee_bps: u16,
    pub max_update_bps: u16,
    pub max_oracle_age_seconds: i64,
    pub minimum_collateral_micros: u64,
    pub protocol_debt_ceiling_pvi_micros: u64,
    pub wallet_debt_ceiling_pvi_micros: u64,
}
impl RiskParameters {
    fn validate(&self) -> Result<()> {
        require!(
            self.min_collateral_ratio_bps > self.liquidation_ratio_bps
                && self.liquidation_ratio_bps >= BPS as u16,
            PviError::InvalidRiskParameters
        );
        require!(
            self.liquidation_penalty_bps < BPS as u16
                && self.mint_fee_bps < BPS as u16
                && self.redemption_fee_bps < BPS as u16,
            PviError::InvalidRiskParameters
        );
        require!(
            self.max_oracle_age_seconds > 0 && self.max_update_bps > 0,
            PviError::InvalidRiskParameters
        );
        require!(
            self.protocol_debt_ceiling_pvi_micros >= self.wallet_debt_ceiling_pvi_micros
                && self.wallet_debt_ceiling_pvi_micros > 0,
            PviError::InvalidRiskParameters
        );
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct OracleUpdate {
    pub sequence: u64,
    pub eligible_volume_usd_micros: u64,
    pub target_price_micros: u64,
    pub observed_at: i64,
}

#[account]
#[derive(InitSpace)]
pub struct Protocol {
    pub authority: Pubkey,
    pub pending_authority: Pubkey,
    pub pai_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub pvi_mint: Pubkey,
    pub collateral_vault: Pubkey,
    pub risk: RiskParameters,
    pub total_debt: u64,
    pub bad_debt: u64,
    pub paused: bool,
    pub pai_bound: bool,
    pub assets_initialized: bool,
    pub bump: u8,
}
#[account]
#[derive(InitSpace)]
pub struct Oracle {
    pub reporter: Pubkey,
    pub latest_volume_usd_micros: u64,
    pub target_price_micros: u64,
    pub previous_target_price_micros: u64,
    pub last_update_timestamp: i64,
    pub sequence: u64,
    pub status: OracleStatus,
    pub bump: u8,
}
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub owner: Pubkey,
    pub collateral_micros: u64,
    pub debt_pvi_micros: u64,
    pub accrued_fees_micros: u64,
    pub bump: u8,
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum OracleStatus {
    Active,
    Paused,
}

#[derive(Accounts)]
pub struct InitializeProtocol<'info> {
    #[account(init,payer=authority,space=8+Protocol::INIT_SPACE,seeds=[b"protocol"],bump)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct InitializeAssets<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=authority)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,
    #[account(init,payer=authority,mint::decimals=TOKEN_DECIMALS,mint::authority=protocol,mint::freeze_authority=protocol,seeds=[b"pvi_mint"],bump)]
    pub pvi_mint: Account<'info, Mint>,
    #[account(init,payer=authority,token::mint=collateral_mint,token::authority=protocol,seeds=[b"collateral_vault"],bump)]
    pub collateral_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}
#[derive(Accounts)]
pub struct InitializeOracle<'info> {
    #[account(seeds=[b"protocol"],bump=protocol.bump,has_one=authority)]
    pub protocol: Account<'info, Protocol>,
    #[account(init,payer=authority,space=8+Oracle::INIT_SPACE,seeds=[b"oracle"],bump)]
    pub oracle: Account<'info, Oracle>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct UpdateOracle<'info> {
    #[account(seeds=[b"protocol"],bump=protocol.bump)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut,seeds=[b"oracle"],bump=oracle.bump,has_one=reporter)]
    pub oracle: Account<'info, Oracle>,
    pub reporter: Signer<'info>,
}
#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(init,payer=owner,space=8+Position::INIT_SPACE,seeds=[b"position",owner.key().as_ref()],bump)]
    pub position: Account<'info, Position>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DepositCollateral<'info> {
    #[account(seeds=[b"protocol"],bump=protocol.bump,has_one=collateral_mint,has_one=collateral_vault)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut,seeds=[b"position",owner.key().as_ref()],bump=position.bump,has_one=owner)]
    pub position: Account<'info, Position>,
    #[account(mut,token::mint=collateral_mint,token::authority=owner)]
    pub owner_collateral: Account<'info, TokenAccount>,
    #[account(mut,address=protocol.collateral_vault,token::mint=collateral_mint,token::authority=protocol)]
    pub collateral_vault: Account<'info, TokenAccount>,
    #[account(address=protocol.collateral_mint)]
    pub collateral_mint: Account<'info, Mint>,
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct MintPvi<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=pvi_mint)]
    pub protocol: Account<'info, Protocol>,
    #[account(seeds=[b"oracle"],bump=oracle.bump)]
    pub oracle: Account<'info, Oracle>,
    #[account(mut,seeds=[b"position",owner.key().as_ref()],bump=position.bump,has_one=owner)]
    pub position: Account<'info, Position>,
    #[account(mut,address=protocol.pvi_mint,mint::authority=protocol)]
    pub pvi_mint: Account<'info, Mint>,
    #[account(mut,token::mint=pvi_mint,token::authority=owner)]
    pub owner_pvi: Account<'info, TokenAccount>,
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}
impl<'info> MintPvi<'info> {
    fn assert_risk_actions_allowed(&self) -> Result<()> {
        require!(!self.protocol.paused, PviError::Paused);
        require!(self.protocol.pai_bound, PviError::PaiNotBound);
        require!(
            self.protocol.assets_initialized,
            PviError::AssetsNotInitialized
        );
        require!(self.protocol.bad_debt == 0, PviError::OutstandingBadDebt);
        assert_oracle_fresh(&self.protocol, &self.oracle)
    }
}

#[derive(Accounts)]
pub struct RepayDebt<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=pvi_mint)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut,seeds=[b"position",owner.key().as_ref()],bump=position.bump,has_one=owner)]
    pub position: Account<'info, Position>,
    #[account(mut,address=protocol.pvi_mint)]
    pub pvi_mint: Account<'info, Mint>,
    #[account(mut,token::mint=pvi_mint,token::authority=owner)]
    pub owner_pvi: Account<'info, TokenAccount>,
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct WithdrawCollateral<'info> {
    #[account(seeds=[b"protocol"],bump=protocol.bump,has_one=collateral_mint,has_one=collateral_vault)]
    pub protocol: Account<'info, Protocol>,
    #[account(seeds=[b"oracle"],bump=oracle.bump)]
    pub oracle: Account<'info, Oracle>,
    #[account(mut,seeds=[b"position",owner.key().as_ref()],bump=position.bump,has_one=owner)]
    pub position: Account<'info, Position>,
    #[account(mut,address=protocol.collateral_vault,token::mint=collateral_mint,token::authority=protocol)]
    pub collateral_vault: Account<'info, TokenAccount>,
    #[account(mut,token::mint=collateral_mint,token::authority=owner)]
    pub owner_collateral: Account<'info, TokenAccount>,
    #[account(address=protocol.collateral_mint)]
    pub collateral_mint: Account<'info, Mint>,
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}
impl<'info> WithdrawCollateral<'info> {
    fn assert_risk_actions_allowed(&self) -> Result<()> {
        require!(!self.protocol.paused, PviError::Paused);
        require!(
            self.protocol.assets_initialized,
            PviError::AssetsNotInitialized
        );
        assert_oracle_fresh(&self.protocol, &self.oracle)
    }
}

#[derive(Accounts)]
pub struct Liquidate<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=collateral_mint,has_one=collateral_vault,has_one=pvi_mint)]
    pub protocol: Account<'info, Protocol>,
    #[account(seeds=[b"oracle"],bump=oracle.bump)]
    pub oracle: Account<'info, Oracle>,
    #[account(mut,seeds=[b"position",position.owner.as_ref()],bump=position.bump)]
    pub position: Account<'info, Position>,
    #[account(mut,address=protocol.collateral_vault,token::mint=collateral_mint,token::authority=protocol)]
    pub collateral_vault: Account<'info, TokenAccount>,
    #[account(address=protocol.collateral_mint)]
    pub collateral_mint: Account<'info, Mint>,
    #[account(mut,address=protocol.pvi_mint)]
    pub pvi_mint: Account<'info, Mint>,
    #[account(mut,token::mint=pvi_mint,token::authority=liquidator)]
    pub liquidator_pvi: Account<'info, TokenAccount>,
    #[account(mut,token::mint=collateral_mint,token::authority=liquidator)]
    pub liquidator_collateral: Account<'info, TokenAccount>,
    pub liquidator: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SettleBadDebt<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=pvi_mint)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut,address=protocol.pvi_mint)]
    pub pvi_mint: Account<'info, Mint>,
    #[account(mut,token::mint=pvi_mint,token::authority=payer)]
    pub payer_pvi: Account<'info, TokenAccount>,
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Admin<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=authority)]
    pub protocol: Account<'info, Protocol>,
    pub authority: Signer<'info>,
}
#[derive(Accounts)]
pub struct AdminOracle<'info> {
    #[account(seeds=[b"protocol"],bump=protocol.bump,has_one=authority)]
    pub protocol: Account<'info, Protocol>,
    #[account(mut,seeds=[b"oracle"],bump=oracle.bump)]
    pub oracle: Account<'info, Oracle>,
    pub authority: Signer<'info>,
}
#[derive(Accounts)]
pub struct AcceptAuthority<'info> {
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump,has_one=pending_authority)]
    pub protocol: Account<'info, Protocol>,
    pub pending_authority: Signer<'info>,
}

#[event]
pub struct PviMinted {
    pub owner: Pubkey,
    pub amount: u64,
    pub fee: u64,
}
#[event]
pub struct DebtRepaid {
    pub owner: Pubkey,
    pub amount: u64,
}
#[event]
pub struct PositionLiquidated {
    pub owner: Pubkey,
    pub liquidator: Pubkey,
    pub repaid: u64,
    pub collateral_seized: u64,
}
#[event]
pub struct BadDebtSettled {
    pub payer: Pubkey,
    pub amount: u64,
}

#[error_code]
pub enum PviError {
    #[msg("protocol must be paused")]
    MustBePaused,
    #[msg("PAI mint is already permanently bound")]
    PaiAlreadyBound,
    #[msg("PAI mint has not been bound")]
    PaiNotBound,
    #[msg("assets already initialized")]
    AssetsAlreadyInitialized,
    #[msg("assets are not initialized")]
    AssetsNotInitialized,
    #[msg("invalid mint")]
    InvalidMint,
    #[msg("invalid token decimals")]
    InvalidDecimals,
    #[msg("overflow")]
    Overflow,
    #[msg("protocol paused")]
    Paused,
    #[msg("oracle paused")]
    OraclePaused,
    #[msg("oracle stale")]
    StaleOracle,
    #[msg("replayed observation")]
    Replay,
    #[msg("invalid timestamp")]
    InvalidTimestamp,
    #[msg("invalid target")]
    InvalidTarget,
    #[msg("update exceeds safety bound")]
    UpdateTooLarge,
    #[msg("position unhealthy")]
    Unhealthy,
    #[msg("insufficient collateral")]
    InsufficientCollateral,
    #[msg("amount is zero")]
    AmountZero,
    #[msg("position has no debt")]
    NoDebt,
    #[msg("wallet debt ceiling")]
    WalletDebtCeiling,
    #[msg("protocol debt ceiling")]
    ProtocolDebtCeiling,
    #[msg("invalid risk parameters")]
    InvalidRiskParameters,
    #[msg("invalid authority")]
    InvalidAuthority,
    #[msg("position is not liquidatable")]
    NotLiquidatable,
    #[msg("liquidation amount exceeds available collateral")]
    LiquidationTooLarge,
    #[msg("protocol has outstanding bad debt")]
    OutstandingBadDebt,
    #[msg("collateral is below the minimum")]
    BelowMinimumCollateral,
    #[msg("protocol has no bad debt")]
    NoBadDebt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collateral_health_uses_six_decimal_units() {
        assert!(healthy(150_000_000, 100_000_000, 1_000_000, 15_000).unwrap());
        assert!(!healthy(149_999_999, 100_000_000, 1_000_000, 15_000).unwrap());
        assert!(healthy(0, 0, 1_000_000, 15_000).unwrap());
    }

    #[test]
    fn fees_are_calculated_in_basis_points() {
        assert_eq!(fee_amount(1_000_000, 50).unwrap(), 5_000);
        assert_eq!(fee_amount(1, 50).unwrap(), 0);
    }

    #[test]
    fn target_price_converts_pvi_to_micro_usd() {
        assert_eq!(usd_value(2_000_000, 1_500_000).unwrap(), 3_000_000);
    }

    #[test]
    fn conservative_risk_parameters_validate() {
        let risk = RiskParameters {
            min_collateral_ratio_bps: 15_000,
            liquidation_ratio_bps: 12_500,
            liquidation_penalty_bps: 500,
            mint_fee_bps: 50,
            redemption_fee_bps: 0,
            max_update_bps: 2_000,
            max_oracle_age_seconds: 300,
            minimum_collateral_micros: 10_000_000,
            protocol_debt_ceiling_pvi_micros: 1_000_000_000,
            wallet_debt_ceiling_pvi_micros: 100_000_000,
        };
        assert!(risk.validate().is_ok());
    }
}
