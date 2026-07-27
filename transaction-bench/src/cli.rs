use {
    crate::priority_fee::{FeeDistribution, FeeTier, PremiumPlan, PriorityFeeMode},
    clap::{Args, Parser, Subcommand, crate_description, crate_name, crate_version, value_parser},
    solana_clap_v3_utils::{
        input_parsers::parse_url_or_moniker, input_validators::normalize_to_url_if_moniker,
    },
    solana_commitment_config::CommitmentConfig,
    solana_pubkey::Pubkey,
    solana_tpu_tools_common::cli::{
        AccountParams, DeleteAccounts, LeaderTracker, ReadAccounts, WriteAccounts,
    },
    std::{
        net::SocketAddr,
        num::{NonZeroU64, NonZeroUsize},
        path::PathBuf,
    },
    tokio::time::Duration,
};

fn parse_and_normalize_url(addr: &str) -> Result<String, String> {
    match parse_url_or_moniker(addr) {
        Ok(parsed) => Ok(normalize_to_url_if_moniker(&parsed)),
        Err(e) => Err(format!("Invalid URL or moniker: {e}")),
    }
}

// Not `Eq`: `PriorityFeeParams` carries `f64` shape parameters.
#[derive(Parser, Debug, PartialEq)]
#[clap(name = crate_name!(),
    version = crate_version!(),
    about = crate_description!(),
    rename_all = "kebab-case"
)]
pub struct ClientCliParameters {
    #[clap(
        long = "url",
        short = 'u',
        value_parser = parse_and_normalize_url,
        help = "URL for Solana's JSON RPC or moniker (or their first letter):\n\
        [mainnet-beta, testnet, devnet, localhost]"
    )]
    pub json_rpc_url: String,

    #[clap(
        long,
        default_value = "confirmed",
        value_parser = value_parser!(CommitmentConfig),
        help = "Block commitment config for getting latest blockhash.\n\
        [possible values: processed, confirmed, finalized]"
    )]
    pub commitment_config: CommitmentConfig,

    // Cannot use value_parser to read keypair file because Keypair is not Clone.
    #[clap(
        long,
        help = "Keypair file of authority. If not provided, create a new one.\nIf authority has \
                insufficient funds, client will try airdrop."
    )]
    pub authority: Option<PathBuf>,

    #[clap(
        long,
        help = "Validate the created accounts number, size, balance.\nMight be time consuming, so \
                recommended only for debugging purposes."
    )]
    pub validate_accounts: bool,

    #[clap(
        long,
        hide = true,
        help = "Use the internal mock RpcClient for local stress testing against a mock QUIC \
                server. Intended for testing only."
    )]
    pub mock_rpc: bool,

    #[clap(subcommand)]
    pub command: Command,
}

// Not `Eq`: contains `ExecutionParams`, which carries `f64` shape parameters.
#[derive(Subcommand, Debug, PartialEq)]
pub enum Command {
    #[clap(about = "Create accounts without saving them and run")]
    Run {
        #[clap(flatten)]
        account_params: AccountParams,

        #[clap(flatten)]
        execution_params: ExecutionParams,

        #[clap(flatten)]
        transaction_params: TransactionParams,
    },

    #[clap(about = "Read accounts from provided accounts file and run")]
    ReadAccountsRun {
        #[clap(flatten)]
        read_accounts: ReadAccounts,

        #[clap(flatten)]
        execution_params: ExecutionParams,

        #[clap(flatten)]
        transaction_params: TransactionParams,
    },

    #[clap(about = "Create accounts and save them to a file, skipping the execution")]
    WriteAccounts(WriteAccounts),

    #[clap(about = "Transfer all lamports from account-file payers to a recipient")]
    DeleteAccounts(DeleteAccounts),
}

// Not `Eq`: contains `PriorityFeeParams`, which carries `f64` shape parameters.
#[derive(Args, Clone, Debug, PartialEq)]
#[clap(rename_all = "kebab-case")]
pub struct ExecutionParams {
    // Cannot use value_parser to read keypair file because Keypair is not Clone.
    #[clap(
        long = "staked-identity-file",
        help = "Validator identity keypair file for staked connection. Spawns one tpu-client-next \
                instance per occurrence. Repeat the same file to get multiple connections under \
                one identity, or use different files for distinct stake allocations. Without this \
                flag a single unstaked instance is used."
    )]
    pub staked_identity_files: Vec<PathBuf>,

    /// Address to bind on, default will listen on all available interfaces, 0 that
    /// OS will choose the port.
    #[clap(long, help = "bind", default_value = "0.0.0.0:0")]
    pub bind: SocketAddr,

    #[clap(
        long,
        value_parser = parse_duration,
        help = "If specified, limits the benchmark execution to the specified duration. May be \
                combined with --num-transactions; whichever limit is reached first stops the run."
    )]
    pub duration: Option<Duration>,

    #[clap(
        long,
        value_parser = value_parser!(NonZeroU64),
        help = "If specified, limits the benchmark to sending this many transactions. May be \
                combined with --duration; whichever limit is reached first stops the run."
    )]
    pub num_transactions: Option<NonZeroU64>,

    #[clap(
        long,
        default_value_t = 0,
        help = "Sign transactions with a blockhash this many SECONDS old instead of the freshest \
                one. 0 (default) uses fresh blockhashes.\nThis uses only getLatestBlockhash (a \
                delay line of observed blockhashes), so it needs nothing special on the target \
                (no getBlock / --enable-rpc-transaction-history, whose RocksDB write \
                amplification can skew single-validator runs). The tool primes for this many \
                seconds before it starts sending, so every transaction is uniformly aged from the \
                first one. Set it just under the validity window (150 blocks x slot_time, e.g. \
                ~22s at ~150ms slots or ~60s at 400ms) so transactions expire shortly after being \
                buffered, stressing the scheduler's discard-on-age path. Tune by watching the \
                validator's num_dropped_on_clean (good) vs num_dropped_on_receive_age (too stale)."
    )]
    pub blockhash_stale_secs: u64,

    #[clap(
        long,
        value_parser = value_parser!(NonZeroU64),
        help = "Optional global target send rate in transactions per second. When set, \
                transaction-bench switches to paced sending."
    )]
    pub target_tps: Option<NonZeroU64>,

    #[clap(
        long,
        value_parser = value_parser!(NonZeroU64),
        help = "Override the QUIC initial congestion window (in bytes) passed to tpu-client-next. \
                Larger values skip TCP-style slow-start at connection startup so that \
                transactions can be sent as fast as possible immediately. Defaults to \
                tpu-client-next's built-in value (128 * PACKET_DATA_SIZE)."
    )]
    pub initial_congestion_window: Option<NonZeroU64>,

    #[clap(
        long,
        default_value_t = 0,
        help = "After the generator finishes, keep the scheduler channels open for up to this \
                many seconds so tpu-client-next's worker queues and quinn send buffers can flush \
                in-flight transactions before teardown. 0 (default) tears down immediately. \
                Recommended when using --num-transactions, which otherwise drops the last \
                in-flight batches."
    )]
    pub drain_seconds: u64,

    #[clap(
        long,
        default_value_t = 16,
        help = "Max number of connections to keep open."
    )]
    pub num_max_open_connections: usize,

    #[clap(
        long,
        default_value_t = 8,
        help = "Size of the workers pull, controls how many transactions batches are generated in \
                parallel."
    )]
    pub workers_pull_size: usize,

    #[clap(
        long,
        default_value_t = 1,
        help = "To how many future leaders the transactions should be sent. The connection fanout \
                is set send_fanout + 1."
    )]
    pub send_fanout: usize,

    #[clap(
        long,
        help = "Sets compute-unit-price (microlamports) for transactions."
    )]
    pub compute_unit_price: Option<u64>,

    #[clap(flatten)]
    pub priority_fee_params: PriorityFeeParams,

    #[clap(subcommand)]
    pub leader_tracker: LeaderTracker,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[clap(rename_all = "kebab-case")]
pub struct TransactionParams {
    #[clap(flatten)]
    pub simple_transfer_tx_params: SimpleTransferTxParams,

    #[clap(flatten)]
    pub padding_params: InstructionPaddingParams,

    #[clap(long, help = "Generate and send transfer transactions in V1 format.")]
    pub use_txv1: bool,
    //TODO(klykov): memo
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[clap(rename_all = "kebab-case")]
pub struct InstructionPaddingParams {
    #[clap(
        long,
        help = "If set, wraps all transfer instructions in the instruction padding program, with \
                the given amount of padding bytes in instruction data."
    )]
    pub instruction_padding_data_size: Option<u32>,

    #[clap(
        long,
        requires = "instruction_padding_data_size",
        help = "Optionally specify the instruction padding program id. Defaults to the SPL \
                instruction padding program."
    )]
    pub instruction_padding_program_id: Option<Pubkey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionPaddingConfig {
    pub program_id: Pubkey,
    pub data_size: u32,
    pub loaded_accounts_data_size_limit: u32,
}

// In case of plain transfer transaction, set loaded account data size to 30 KiB.
// It is large enough yet smaller than 32 KiB page size, so it would cost 0 extra CU.
pub(crate) const TRANSFER_TRANSACTION_LOADED_ACCOUNTS_DATA_SIZE: u32 = 30 * 1024;
// In case of padding program usage, we need to take into account program size too.
const PADDING_PROGRAM_ACCOUNT_DATA_SIZE: u32 = 28 * 1024;

fn get_padded_transaction_loaded_accounts_data_size() -> u32 {
    TRANSFER_TRANSACTION_LOADED_ACCOUNTS_DATA_SIZE + PADDING_PROGRAM_ACCOUNT_DATA_SIZE
}

impl TransactionParams {
    pub fn instruction_padding_config(&self) -> Option<InstructionPaddingConfig> {
        self.padding_params
            .instruction_padding_data_size
            .map(|data_size| InstructionPaddingConfig {
                program_id: self
                    .padding_params
                    .instruction_padding_program_id
                    .unwrap_or(spl_instruction_padding_interface::ID),
                data_size,
                loaded_accounts_data_size_limit: get_padded_transaction_loaded_accounts_data_size(),
            })
    }
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
#[clap(rename_all = "kebab-case")]
pub struct SimpleTransferTxParams {
    #[clap(
        long = "max-lamports-to-transfer",
        alias = "lamports-to-transfer",
        default_value_t = DEFAULT_MAX_LAMPORTS_TO_TRANSFER,
        value_parser = value_parser!(u64).range(513..),
        help = "Max lamports to transfer in a transfer instruction. For each transfer instruction \
                in a generated batch, select a unique random value from the range [1, this value]\n\
                to provide more entropy for transactions. Defaults to 65536.\n"
    )]
    pub max_lamports_to_transfer: u64,

    #[clap(long, default_value = "600", help = "Transfer transaction CU budget.")]
    pub transfer_tx_cu_budget: u32,

    #[clap(
        long,
        default_value = "1",
        help = "Number of send instructions per transaction."
    )]
    pub num_send_instructions_per_tx: usize,

    #[clap(
        long,
        value_parser = value_parser!(NonZeroUsize),
        help = "Number of transactions per batch. Required when using --num-conflict-groups."
    )]
    pub tx_batch_size: Option<NonZeroUsize>,

    #[clap(
        long,
        requires = "tx_batch_size",
        value_parser = value_parser!(NonZeroUsize),
        help = "Number of unique destination accounts per batch.\n\
                When set, destinations repeat to create account conflicts.\n\
                Lower value = more conflicts. Default: all destinations unique."
    )]
    pub num_conflict_groups: Option<NonZeroUsize>,
}

const DEFAULT_MAX_LAMPORTS_TO_TRANSFER: u64 = 65_536;

fn parse_duration(s: &str) -> Result<Duration, &'static str> {
    s.parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| "failed to parse duration")
}

pub fn build_cli_parameters() -> ClientCliParameters {
    ClientCliParameters::parse()
}

/// Default power-law shape (`beta`) when `--priority-fee-shape` is omitted.
const DEFAULT_POWER_LAW_BETA: f64 = 2.0;
/// Default Pareto shape (`alpha`) when `--priority-fee-shape` is omitted.
const DEFAULT_PARETO_ALPHA: f64 = 1.0;

/// Selects the shape of the random additional priority fee. Maps to a
/// [`FeeDistribution`] in [`PriorityFeeMode`].
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeDistributionKind {
    /// Flat draw over `[0, max]` (original behavior).
    Uniform,
    /// Bounded power law: most txs cheap, high fees rarer as `beta` grows.
    PowerLaw,
    /// Bounded Pareto on `[1, max]`: heavy right tail, models real fee markets.
    Pareto,
    /// Weighted discrete fees from `--priority-fee-tiers`.
    Tiered,
    /// Exact, reproducible high-fee subset derived from `--num-transactions`
    /// (see `--high-fee-*`). Not a random draw.
    Premium,
}

/// CLI flags controlling the additional priority fee component. Flattened into [`ExecutionParams`];
/// convert to a runtime [`PriorityFeeMode`] via [`TryFrom`].
///
/// Not `Eq`: `--priority-fee-shape` is an `f64`.
#[derive(Args, Clone, Debug, PartialEq)]
#[clap(rename_all = "kebab-case")]
pub struct PriorityFeeParams {
    #[clap(
        long,
        default_value_t = 0,
        help = "Top of the additional priority-fee range (microlamports) on top of \
                --compute-unit-price.\nRandom mode (default): each tx draws from \
                --priority-fee-distribution over [0, N] (uniform by default).\nScheduled mode \
                (with --priority-fee-schedule-period-ms): fee cycles 0..=N,\nadvancing one step \
                every period. 0 = no additional component (tiered mode ignores N).\nWhen > 0 and \
                --compute-unit-price is unset, base defaults to 1."
    )]
    pub random_compute_unit_price_max: u64,

    #[clap(
        long,
        default_value = "uniform",
        help = "Shape of the random additional fee over [0, --random-compute-unit-price-max]:\n\
                uniform (default): flat; every fee equally likely (original behavior).\n\
                power-law: fee = max*(1 - u^(1/beta)); most txs cheap, high fees rarer.\n\
                pareto: bounded Pareto on [1, max]; heavy right tail (models real fee markets).\n\
                tiered: weighted discrete fees from --priority-fee-tiers.\n\
                premium: exact, reproducible high-fee subset of --num-transactions (see \
                --high-fee-*).\n\
                Tune power-law/pareto with --priority-fee-shape."
    )]
    pub priority_fee_distribution: FeeDistributionKind,

    #[clap(
        long,
        help = "Shape parameter for --priority-fee-distribution:\n\
                power-law: beta > 0 (default 2.0); beta=1 is uniform, larger beta = rarer highs.\n\
                pareto: alpha > 0 (default 1.0); smaller alpha = heavier high-fee tail.\n\
                Must be unset for uniform and tiered."
    )]
    pub priority_fee_shape: Option<f64>,

    #[clap(
        long,
        help = "Weighted fee:weight tiers for --priority-fee-distribution tiered, e.g.\n\
                '0:90,500:9,1000:1' => 90% pay +0, 9% pay +500, 1% pay +1000 (weights are\n\
                relative; they need not sum to 100). Fees are additional microlamports on top of\n\
                the base --compute-unit-price; a bare 'FEE' defaults its weight to 1. Required for\n\
                and only valid with tiered mode."
    )]
    pub priority_fee_tiers: Option<String>,

    #[clap(
        long,
        help = "Additional priority fee (microlamports) charged by the premium subset in \
                --priority-fee-distribution premium. Non-premium transactions add nothing on top \
                of the base --compute-unit-price. Only valid in premium mode."
    )]
    pub high_fee_microlamports: Option<u64>,

    #[clap(
        long,
        help = "Premium share of --num-transactions for --priority-fee-distribution premium, in \
                (0.0, 1.0]. Premium count = round(fraction * num-transactions). Mutually exclusive \
                with --high-fee-count."
    )]
    pub high_fee_fraction: Option<f64>,

    #[clap(
        long,
        help = "Exact number of premium transactions for --priority-fee-distribution premium \
                (must be <= --num-transactions). Mutually exclusive with --high-fee-fraction."
    )]
    pub high_fee_count: Option<u64>,

    #[clap(
        long,
        value_parser = value_parser!(NonZeroU64),
        help = "Switch priority fee from random to deterministic scheduled mode.\n\
                The fee ramps linearly from 0 to --random-compute-unit-price-max\n\
                over N milliseconds, then resets and repeats (sawtooth). Both\n\
                endpoints (0 and max) are observed exactly. Requires\n\
                --random-compute-unit-price-max > 0 (no effect otherwise).\n\
                N = 1 is degenerate (no resolvable ramp in a 1 ms window)."
    )]
    pub priority_fee_schedule_period_ms: Option<NonZeroU64>,
}

/// Parse a `--priority-fee-tiers` spec: comma-separated `fee[:weight]` entries,
/// e.g. `0:90,500:9,1000:1`. A bare `fee` defaults its weight to `1.0`. Weights
/// are relative and need not sum to anything in particular; each must be
/// positive. Empty entries (e.g. a trailing comma) are skipped.
fn parse_fee_tiers(spec: &str) -> Result<Vec<FeeTier>, String> {
    let mut tiers = Vec::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (fee_str, weight) = match entry.split_once(':') {
            Some((fee_str, weight_str)) => {
                let weight = weight_str.trim().parse::<f64>().map_err(|_| {
                    format!("invalid weight in --priority-fee-tiers entry '{entry}'")
                })?;
                (fee_str.trim(), weight)
            }
            None => (entry, 1.0),
        };
        let fee = fee_str
            .parse::<u64>()
            .map_err(|_| format!("invalid fee in --priority-fee-tiers entry '{entry}'"))?;
        if weight <= 0.0 || !weight.is_finite() {
            return Err(format!(
                "weight must be a positive, finite number in --priority-fee-tiers entry '{entry}'"
            ));
        }
        tiers.push(FeeTier { fee, weight });
    }
    if tiers.is_empty() {
        return Err("--priority-fee-tiers must list at least one fee:weight tier".to_string());
    }
    Ok(tiers)
}

/// Resolve the parsed CLI args to a [`PriorityFeeMode`].
///
/// Takes `num_transactions` because premium mode derives its premium count from
/// it. Rejects combinations that would silently ignore a flag: scheduled mode
/// only pairs with the default uniform range; `--priority-fee-shape` only
/// applies to power-law/pareto; `--priority-fee-tiers` only applies to tiered;
/// `--high-fee-*` only apply to premium; and the continuous distributions are
/// no-ops when `--random-compute-unit-price-max` is 0 (as is scheduling, per
/// PR #56).
impl PriorityFeeParams {
    pub fn to_mode(&self, num_transactions: Option<NonZeroU64>) -> Result<PriorityFeeMode, String> {
        let max = self.random_compute_unit_price_max;
        let distribution = self.priority_fee_distribution;
        let shape = self.priority_fee_shape;
        let tiers = self.priority_fee_tiers.as_deref();

        // Premium is its own mode: an exact, reproducible high-fee subset
        // derived from --num-transactions, incompatible with the random /
        // scheduled knobs.
        if distribution == FeeDistributionKind::Premium {
            return self.premium_mode(num_transactions);
        }

        // The --high-fee-* knobs only mean something in premium mode.
        if self.high_fee_microlamports.is_some()
            || self.high_fee_fraction.is_some()
            || self.high_fee_count.is_some()
        {
            return Err(
                "--high-fee-microlamports / --high-fee-fraction / --high-fee-count \
                        require --priority-fee-distribution premium"
                    .to_string(),
            );
        }

        // Scheduled is a deterministic sawtooth, orthogonal to the random
        // distribution shapes; it only combines with the default uniform range.
        if let Some(period) = self.priority_fee_schedule_period_ms {
            if max == 0 {
                return Err("--priority-fee-schedule-period-ms has no effect when \
                            --random-compute-unit-price-max is 0; set it to a positive value"
                    .to_string());
            }
            if distribution != FeeDistributionKind::Uniform || shape.is_some() || tiers.is_some() {
                return Err(
                    "--priority-fee-schedule-period-ms (scheduled mode) cannot be combined \
                            with --priority-fee-distribution / --priority-fee-shape / \
                            --priority-fee-tiers"
                        .to_string(),
                );
            }
            return Ok(PriorityFeeMode::Scheduled {
                max,
                period_ms: period.get(),
            });
        }

        // Tiered draws from an explicit fee:weight table and ignores `max`.
        if distribution == FeeDistributionKind::Tiered {
            if shape.is_some() {
                return Err(
                    "--priority-fee-shape has no effect for --priority-fee-distribution tiered"
                        .to_string(),
                );
            }
            let spec = tiers.ok_or_else(|| {
                "--priority-fee-distribution tiered requires --priority-fee-tiers".to_string()
            })?;
            return Ok(PriorityFeeMode::Random(FeeDistribution::Tiered {
                tiers: parse_fee_tiers(spec)?,
            }));
        }

        // Outside tiered mode, a tier table is meaningless.
        if tiers.is_some() {
            return Err(
                "--priority-fee-tiers requires --priority-fee-distribution tiered".to_string(),
            );
        }

        // Continuous distributions (uniform/power-law/pareto) live on [0, max];
        // max == 0 disables the additional component entirely.
        if max == 0 {
            if distribution != FeeDistributionKind::Uniform || shape.is_some() {
                return Err(
                    "--priority-fee-distribution / --priority-fee-shape have no effect when \
                            --random-compute-unit-price-max is 0; set it to a positive value"
                        .to_string(),
                );
            }
            return Ok(PriorityFeeMode::None);
        }

        let distribution = match distribution {
            FeeDistributionKind::Uniform => {
                if shape.is_some() {
                    return Err(
                        "--priority-fee-shape has no effect for --priority-fee-distribution uniform"
                            .to_string(),
                    );
                }
                FeeDistribution::Uniform { max }
            }
            FeeDistributionKind::PowerLaw => {
                let beta = shape.unwrap_or(DEFAULT_POWER_LAW_BETA);
                if beta <= 0.0 || !beta.is_finite() {
                    return Err(
                        "--priority-fee-shape (power-law beta) must be a positive, finite number"
                            .to_string(),
                    );
                }
                FeeDistribution::PowerLaw { max, beta }
            }
            FeeDistributionKind::Pareto => {
                let alpha = shape.unwrap_or(DEFAULT_PARETO_ALPHA);
                if alpha <= 0.0 || !alpha.is_finite() {
                    return Err(
                        "--priority-fee-shape (pareto alpha) must be a positive, finite number"
                            .to_string(),
                    );
                }
                FeeDistribution::Pareto { max, alpha }
            }
            FeeDistributionKind::Tiered => unreachable!("tiered handled above"),
            FeeDistributionKind::Premium => unreachable!("premium handled above"),
        };
        Ok(PriorityFeeMode::Random(distribution))
    }

    /// Build a [`PriorityFeeMode::Premium`] plan. `total` comes from
    /// `--num-transactions`; the premium count comes from exactly one of
    /// `--high-fee-fraction` / `--high-fee-count`. Rejects the random / scheduled
    /// knobs, which have no meaning here.
    #[allow(clippy::arithmetic_side_effects)]
    fn premium_mode(
        &self,
        num_transactions: Option<NonZeroU64>,
    ) -> Result<PriorityFeeMode, String> {
        if self.random_compute_unit_price_max != 0
            || self.priority_fee_shape.is_some()
            || self.priority_fee_tiers.is_some()
            || self.priority_fee_schedule_period_ms.is_some()
        {
            return Err("--priority-fee-distribution premium does not use \
                        --random-compute-unit-price-max / --priority-fee-shape / \
                        --priority-fee-tiers / --priority-fee-schedule-period-ms"
                .to_string());
        }

        let total = num_transactions
            .ok_or_else(|| {
                "--priority-fee-distribution premium requires --num-transactions (the premium \
                 count is derived from it)"
                    .to_string()
            })?
            .get();

        let premium_fee = self.high_fee_microlamports.ok_or_else(|| {
            "--priority-fee-distribution premium requires --high-fee-microlamports".to_string()
        })?;

        let premium_count = match (self.high_fee_fraction, self.high_fee_count) {
            (Some(_), Some(_)) => {
                return Err("set only one of --high-fee-fraction or --high-fee-count".to_string());
            }
            (None, None) => {
                return Err(
                    "--priority-fee-distribution premium requires --high-fee-fraction or \
                            --high-fee-count"
                        .to_string(),
                );
            }
            (Some(fraction), None) => {
                if fraction <= 0.0 || !fraction.is_finite() || fraction > 1.0 {
                    return Err("--high-fee-fraction must be in (0.0, 1.0]".to_string());
                }
                // round() is nearest; min() keeps it within total after rounding.
                (fraction * total as f64).round().min(total as f64) as u64
            }
            (None, Some(count)) => {
                if count > total {
                    return Err(format!(
                        "--high-fee-count ({count}) cannot exceed --num-transactions ({total})"
                    ));
                }
                count
            }
        };

        Ok(PriorityFeeMode::Premium(PremiumPlan {
            total,
            premium_count,
            premium_fee,
        }))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        clap::Parser,
        solana_native_token::LAMPORTS_PER_SOL,
        std::net::{IpAddr, Ipv4Addr},
    };

    fn get_common_account_params() -> (Vec<&'static str>, AccountParams) {
        (
            vec!["--num-payers", "256", "--payer-account-balance", "1"],
            AccountParams {
                num_payers: 256,
                payer_account_balance: LAMPORTS_PER_SOL,
            },
        )
    }

    fn get_common_execution_params(keypair_file_name: &str) -> (Vec<&str>, ExecutionParams) {
        (
            vec![
                "--staked-identity-file",
                keypair_file_name,
                "--duration",
                "120",
                "--send-fanout",
                "2",
                "--compute-unit-price",
                "1000",
                "pinned-leader-tracker",
                "127.0.0.1:8009",
            ],
            ExecutionParams {
                staked_identity_files: vec![PathBuf::from(&keypair_file_name)],
                bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0),
                duration: Some(Duration::from_secs(120)),
                num_transactions: None,
                blockhash_stale_secs: 0,
                target_tps: None,
                initial_congestion_window: None,
                drain_seconds: 0,
                leader_tracker: LeaderTracker::PinnedLeaderTracker {
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8009),
                },
                num_max_open_connections: 16,
                workers_pull_size: 8,
                send_fanout: 2,
                compute_unit_price: Some(1000),
                priority_fee_params: PriorityFeeParams {
                    random_compute_unit_price_max: 0,
                    priority_fee_distribution: FeeDistributionKind::Uniform,
                    priority_fee_shape: None,
                    priority_fee_tiers: None,
                    high_fee_microlamports: None,
                    high_fee_fraction: None,
                    high_fee_count: None,
                    priority_fee_schedule_period_ms: None,
                },
            },
        )
    }

    #[test]
    fn test_run_command() {
        let keypair_file_name = "/home/testUser/masterKey.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "run",
            "--max-lamports-to-transfer",
            "1000",
            "--transfer-tx-cu-budget",
            "600",
        ];
        let (account_args, account_params) = get_common_account_params();
        args.extend(account_args.iter());
        let (exec_args, execution_params) = get_common_execution_params(keypair_file_name);
        args.extend(exec_args.iter());

        let expected_parameters = ClientCliParameters {
            json_rpc_url: "http://localhost:8899".to_string(),
            commitment_config: CommitmentConfig::confirmed(),
            command: Command::Run {
                transaction_params: TransactionParams {
                    simple_transfer_tx_params: SimpleTransferTxParams {
                        max_lamports_to_transfer: 1000,
                        transfer_tx_cu_budget: 600,
                        num_send_instructions_per_tx: 1,
                        tx_batch_size: None,
                        num_conflict_groups: None,
                    },
                    padding_params: InstructionPaddingParams {
                        instruction_padding_data_size: None,
                        instruction_padding_program_id: None,
                    },
                    use_txv1: false,
                },
                account_params,
                execution_params,
            },
            authority: Some(PathBuf::from(&keypair_file_name)),
            validate_accounts: false,
            mock_rpc: false,
        };
        let actual = ClientCliParameters::try_parse_from(args).unwrap();

        assert_eq!(actual, expected_parameters);
    }

    #[test]
    fn test_read_accounts_run_command() {
        let keypair_file_name = "/home/testUser/masterKey.json";
        let accounts_file_name = "/home/testUser/accountsFile.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "read-accounts-run",
            "--accounts-file",
            accounts_file_name,
            "--transfer-tx-cu-budget",
            "1000",
            "--num-send-instructions-per-tx",
            "2",
        ];
        let (exec_args, execution_params) = get_common_execution_params(keypair_file_name);
        args.extend(exec_args.iter());

        let expected_parameters = ClientCliParameters {
            json_rpc_url: "http://localhost:8899".to_string(),
            commitment_config: CommitmentConfig::confirmed(),
            command: Command::ReadAccountsRun {
                read_accounts: ReadAccounts {
                    accounts_file: accounts_file_name.into(),
                },

                transaction_params: TransactionParams {
                    simple_transfer_tx_params: SimpleTransferTxParams {
                        max_lamports_to_transfer: DEFAULT_MAX_LAMPORTS_TO_TRANSFER,
                        transfer_tx_cu_budget: 1000,
                        num_send_instructions_per_tx: 2,
                        tx_batch_size: None,
                        num_conflict_groups: None,
                    },
                    padding_params: InstructionPaddingParams {
                        instruction_padding_data_size: None,
                        instruction_padding_program_id: None,
                    },
                    use_txv1: false,
                },
                execution_params,
            },
            authority: Some(PathBuf::from(&keypair_file_name)),
            validate_accounts: false,
            mock_rpc: false,
        };
        let cli = ClientCliParameters::try_parse_from(args);
        assert!(cli.is_ok(), "Unexpected error {:?}", cli.err());
        let actual = cli.unwrap();

        assert_eq!(actual, expected_parameters);
    }

    #[test]
    fn test_instruction_padding_config_defaults_program_id() {
        let params = TransactionParams {
            simple_transfer_tx_params: SimpleTransferTxParams {
                max_lamports_to_transfer: DEFAULT_MAX_LAMPORTS_TO_TRANSFER,
                transfer_tx_cu_budget: 600,
                num_send_instructions_per_tx: 1,
                tx_batch_size: None,
                num_conflict_groups: None,
            },
            padding_params: InstructionPaddingParams {
                instruction_padding_data_size: Some(128),
                instruction_padding_program_id: None,
            },
            use_txv1: false,
        };

        let padding_config = params.instruction_padding_config().unwrap();

        assert_eq!(
            padding_config.program_id,
            spl_instruction_padding_interface::ID
        );
        assert_eq!(padding_config.data_size, 128);
        assert_eq!(padding_config.loaded_accounts_data_size_limit, 58 * 1024);
    }

    #[test]
    fn test_target_tps_execution_param() {
        let keypair_file_name = "/home/testUser/masterKey.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "run",
            "--max-lamports-to-transfer",
            "1000",
            "--transfer-tx-cu-budget",
            "600",
            "--target-tps",
            "100",
        ];
        let (account_args, account_params) = get_common_account_params();
        args.extend(account_args.iter());
        let (exec_args, mut execution_params) = get_common_execution_params(keypair_file_name);
        args.extend(exec_args.iter());
        execution_params.target_tps = Some(NonZeroU64::new(100).unwrap());

        let actual = ClientCliParameters::try_parse_from(args).unwrap();
        let Command::Run {
            account_params: actual_account_params,
            execution_params: actual_execution_params,
            ..
        } = actual.command
        else {
            panic!("expected run command");
        };

        assert_eq!(actual_account_params, account_params);
        assert_eq!(actual_execution_params, execution_params);
    }

    #[test]
    fn test_use_txv1_transaction_param() {
        let keypair_file_name = "/home/testUser/masterKey.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "run",
            "--use-txv1",
        ];
        let (account_args, _account_params) = get_common_account_params();
        args.extend(account_args.iter());
        let (exec_args, _execution_params) = get_common_execution_params(keypair_file_name);
        args.extend(exec_args.iter());

        let actual = ClientCliParameters::try_parse_from(args).unwrap();
        let Command::Run {
            transaction_params, ..
        } = actual.command
        else {
            panic!("expected run command");
        };

        assert!(transaction_params.use_txv1);
    }

    #[test]
    fn test_write_accounts_command() {
        let keypair_file_name = "/home/testUser/masterKey.json";
        let accounts_file_name = "/home/testUser/accountsFile.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "write-accounts",
            "--accounts-file",
            accounts_file_name,
        ];

        let (account_args, account_params) = get_common_account_params();
        args.extend(account_args.iter());

        let expected_parameters = ClientCliParameters {
            json_rpc_url: "http://localhost:8899".to_string(),
            commitment_config: CommitmentConfig::confirmed(),
            command: Command::WriteAccounts(WriteAccounts {
                accounts_file: accounts_file_name.into(),
                account_params,
            }),
            authority: Some(PathBuf::from(&keypair_file_name)),
            validate_accounts: false,
            mock_rpc: false,
        };
        let cli = ClientCliParameters::try_parse_from(args);
        assert!(cli.is_ok(), "Unexpected error {:?}", cli.err());
        let actual = cli.unwrap();

        assert_eq!(actual, expected_parameters);
    }

    #[test]
    fn test_delete_accounts_command() {
        let keypair_file_name = "/home/testUser/masterKey.json";
        let accounts_file_name = "/home/testUser/accountsFile.json";
        let recipient = Pubkey::new_unique();
        let recipient_string = recipient.to_string();

        let args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "delete-accounts",
            "--accounts-file",
            accounts_file_name,
            "--recipient",
            &recipient_string,
        ];

        const MAX_RPC_SEND_TX_BATCH: usize = 60;
        let expected_parameters = ClientCliParameters {
            json_rpc_url: "http://localhost:8899".to_string(),
            commitment_config: CommitmentConfig::confirmed(),
            command: Command::DeleteAccounts(DeleteAccounts {
                accounts_file: accounts_file_name.into(),
                recipient: recipient_string.clone(),
                txn_batch_size: MAX_RPC_SEND_TX_BATCH,
            }),
            authority: Some(PathBuf::from(&keypair_file_name)),
            validate_accounts: false,
            mock_rpc: false,
        };
        let cli = ClientCliParameters::try_parse_from(args);
        assert!(cli.is_ok(), "Unexpected error {:?}", cli.err());
        let actual = cli.unwrap();

        assert_eq!(actual, expected_parameters);
    }

    #[test]
    fn test_conflict_groups_requires_tx_batch_size() {
        let keypair_file_name = "/home/testUser/masterKey.json";
        let (account_args, _account_params) = get_common_account_params();
        let (exec_args, _execution_params) = get_common_execution_params(keypair_file_name);

        let mut base_args = vec!["test", "-ul", "--authority", keypair_file_name, "run"];
        base_args.extend(account_args.iter());

        // ok: both flags present
        let mut args = base_args.clone();
        args.extend(["--tx-batch-size", "64", "--num-conflict-groups", "4"]);
        args.extend(exec_args.iter());
        assert!(ClientCliParameters::try_parse_from(args).is_ok());

        // err: num-conflict-groups without tx-batch-size
        let mut args = base_args.clone();
        args.extend(["--num-conflict-groups", "4"]);
        args.extend(exec_args.iter());
        assert!(ClientCliParameters::try_parse_from(args).is_err());

        // err: num-conflict-groups = 0
        let mut args = base_args.clone();
        args.extend(["--tx-batch-size", "64", "--num-conflict-groups", "0"]);
        args.extend(exec_args.iter());
        assert!(ClientCliParameters::try_parse_from(args).is_err());
    }

    #[test]
    fn test_priority_fee_params_try_into_mode() {
        use FeeDistributionKind::*;

        fn params(
            max: u64,
            period: Option<u64>,
            distribution: FeeDistributionKind,
            shape: Option<f64>,
            tiers: Option<&str>,
        ) -> PriorityFeeParams {
            PriorityFeeParams {
                random_compute_unit_price_max: max,
                priority_fee_distribution: distribution,
                priority_fee_shape: shape,
                priority_fee_tiers: tiers.map(String::from),
                high_fee_microlamports: None,
                high_fee_fraction: None,
                high_fee_count: None,
                priority_fee_schedule_period_ms: period.map(|p| NonZeroU64::new(p).unwrap()),
            }
        }

        // max = 0 with defaults => None.
        assert_eq!(
            params(0, None, Uniform, None, None).to_mode(None).unwrap(),
            PriorityFeeMode::None
        );

        // max > 0, uniform (default) => Random(Uniform).
        assert_eq!(
            params(100, None, Uniform, None, None)
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::Uniform { max: 100 })
        );

        // max > 0 with schedule => Scheduled.
        assert_eq!(
            params(10, Some(5), Uniform, None, None)
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Scheduled {
                max: 10,
                period_ms: 5
            }
        );

        // Scheduling with max = 0 must be rejected: silently no-op was the bug,
        // see PR #56 review.
        assert!(
            params(0, Some(5), Uniform, None, None)
                .to_mode(None)
                .is_err()
        );

        // Scheduled cannot combine with a non-uniform distribution.
        assert!(
            params(10, Some(5), Pareto, None, None)
                .to_mode(None)
                .is_err()
        );

        // power-law: --priority-fee-shape becomes beta; default applies when unset.
        assert_eq!(
            params(1000, None, PowerLaw, Some(3.0), None)
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::PowerLaw {
                max: 1000,
                beta: 3.0
            })
        );
        assert_eq!(
            params(1000, None, PowerLaw, None, None)
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::PowerLaw {
                max: 1000,
                beta: DEFAULT_POWER_LAW_BETA
            })
        );

        // pareto: --priority-fee-shape becomes alpha; default applies when unset.
        assert_eq!(
            params(1000, None, Pareto, None, None)
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::Pareto {
                max: 1000,
                alpha: DEFAULT_PARETO_ALPHA
            })
        );

        // Non-positive shape is rejected.
        assert!(
            params(1000, None, PowerLaw, Some(0.0), None)
                .to_mode(None)
                .is_err()
        );

        // Shape has no effect for uniform.
        assert!(
            params(1000, None, Uniform, Some(2.0), None)
                .to_mode(None)
                .is_err()
        );

        // A non-uniform distribution is a no-op when max = 0.
        assert!(params(0, None, Pareto, None, None).to_mode(None).is_err());

        // tiered parses the table and ignores max.
        assert_eq!(
            params(0, None, Tiered, None, Some("0:90,500:9,1000:1"))
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::Tiered {
                tiers: vec![
                    FeeTier {
                        fee: 0,
                        weight: 90.0
                    },
                    FeeTier {
                        fee: 500,
                        weight: 9.0
                    },
                    FeeTier {
                        fee: 1000,
                        weight: 1.0
                    },
                ]
            })
        );

        // A bare fee defaults its weight to 1.
        assert_eq!(
            params(0, None, Tiered, None, Some("0,1000"))
                .to_mode(None)
                .unwrap(),
            PriorityFeeMode::Random(FeeDistribution::Tiered {
                tiers: vec![
                    FeeTier {
                        fee: 0,
                        weight: 1.0
                    },
                    FeeTier {
                        fee: 1000,
                        weight: 1.0
                    },
                ]
            })
        );

        // tiered without a table is rejected; a table outside tiered is rejected.
        assert!(params(0, None, Tiered, None, None).to_mode(None).is_err());
        assert!(
            params(1000, None, Uniform, None, Some("0:1"))
                .to_mode(None)
                .is_err()
        );
        // Malformed tier specs are rejected.
        assert!(
            params(0, None, Tiered, None, Some("0:0"))
                .to_mode(None)
                .is_err(),
            "zero weight"
        );
        assert!(
            params(0, None, Tiered, None, Some("abc:1"))
                .to_mode(None)
                .is_err(),
            "non-numeric fee"
        );

        // Premium mode: exact high-fee subset derived from --num-transactions.
        fn premium(
            microlamports: Option<u64>,
            fraction: Option<f64>,
            count: Option<u64>,
        ) -> PriorityFeeParams {
            PriorityFeeParams {
                random_compute_unit_price_max: 0,
                priority_fee_distribution: Premium,
                priority_fee_shape: None,
                priority_fee_tiers: None,
                high_fee_microlamports: microlamports,
                high_fee_fraction: fraction,
                high_fee_count: count,
                priority_fee_schedule_period_ms: None,
            }
        }
        let n = NonZeroU64::new;

        // fraction => premium count = round(fraction * N).
        assert_eq!(
            premium(Some(1000), Some(0.01), None)
                .to_mode(n(10_000))
                .unwrap(),
            PriorityFeeMode::Premium(PremiumPlan {
                total: 10_000,
                premium_count: 100,
                premium_fee: 1000
            })
        );
        // explicit count is taken verbatim.
        assert_eq!(
            premium(Some(1000), None, Some(42))
                .to_mode(n(10_000))
                .unwrap(),
            PriorityFeeMode::Premium(PremiumPlan {
                total: 10_000,
                premium_count: 42,
                premium_fee: 1000
            })
        );
        // premium requires --num-transactions, --high-fee-microlamports, and
        // exactly one of fraction/count.
        assert!(premium(Some(1000), Some(0.01), None).to_mode(None).is_err());
        assert!(premium(None, Some(0.01), None).to_mode(n(100)).is_err());
        assert!(premium(Some(1000), None, None).to_mode(n(100)).is_err());
        assert!(
            premium(Some(1000), Some(0.01), Some(5))
                .to_mode(n(100))
                .is_err()
        );
        // fraction must be in (0.0, 1.0]; count must be <= N.
        assert!(
            premium(Some(1000), Some(0.0), None)
                .to_mode(n(100))
                .is_err()
        );
        assert!(
            premium(Some(1000), Some(1.5), None)
                .to_mode(n(100))
                .is_err()
        );
        assert!(
            premium(Some(1000), None, Some(101))
                .to_mode(n(100))
                .is_err()
        );

        // --high-fee-* outside premium mode is rejected.
        let mut high_fee_on_uniform = params(1000, None, Uniform, None, None);
        high_fee_on_uniform.high_fee_microlamports = Some(1000);
        assert!(high_fee_on_uniform.to_mode(n(100)).is_err());

        // Premium rejects the random/scheduled knobs.
        let mut premium_with_max = premium(Some(1000), Some(0.01), None);
        premium_with_max.random_compute_unit_price_max = 500;
        assert!(premium_with_max.to_mode(n(100)).is_err());
    }

    #[test]
    fn test_mock_rpc_flag() {
        let keypair_file_name = "/home/testUser/masterKey.json";

        let mut args = vec![
            "test",
            "--mock-rpc",
            "-ul",
            "--authority",
            keypair_file_name,
            "run",
        ];
        let (account_args, _account_params) = get_common_account_params();
        args.extend(account_args.iter());
        let (exec_args, _execution_params) = get_common_execution_params(keypair_file_name);
        args.extend(exec_args.iter());

        let actual = ClientCliParameters::try_parse_from(args).unwrap();
        assert!(actual.mock_rpc);
    }

    /// Check that cannot use `write` subcommand together with parameters from `TransactionParams`
    #[test]
    fn test_write_accounts_file_conflict() {
        let keypair_file_name = "/home/testUser/masterKey.json";
        let accounts_file_name = "/home/testUser/accountsFile.json";

        let mut args = vec![
            "test",
            "-ul",
            "--authority",
            keypair_file_name,
            "write-accounts",
            "--num-accounts-per-tx",
            "100",
            "--accounts-file",
            accounts_file_name,
        ];

        let (account_args, _account_params) = get_common_account_params();
        args.extend(account_args.iter());

        let cli = ClientCliParameters::try_parse_from(args);
        assert!(cli.is_err());
    }
}
