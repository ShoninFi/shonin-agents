//! What the strategy wants to do, worked out from what it can see.
//!
//! Everything here is arithmetic on an [`AccountState`]: no host call, no
//! clock, nothing sent. The program around it reads the state, asks what to
//! do, and does it. Keeping the decision apart is what lets the tests below
//! pin down when the strategy spends gas and when it doesn't.
//!
//! Every transaction costs the vault's tank, so the rules are written to send
//! only what moves the carry meaningfully towards its target:
//!
//! - the money the venue already holds is counted before any more is moved,
//!   and the venue is funded for the whole carry in one move rather than an
//!   order's worth at a time;
//! - nothing is moved unless an order can follow it;
//! - a carry that has drifted less than [`config::REBALANCE_BAND`] from its
//!   target is left alone;
//! - the short leg is sized to what the spot actually bought, so an order
//!   that didn't fill leaves nothing to repair and nothing to resend; and
//!   spot left without its short is hedged before any more is bought.

use strategy_sdk::{
    AccountEvent, AccountState, Decimal, Limits, Order, Side, decimal, from_payload, size_for,
};

use crate::config;
use crate::per_share::{Held, PerShare};

/// The settings the strategy decides with, as numbers.
pub struct Settings {
    pub spot: String,
    pub perp: String,
    /// Funding, as a fraction of notional per day, worth carrying.
    pub entry_funding: Decimal,
    /// And below which an open carry is unwound.
    pub exit_funding: Decimal,
    /// How much of the vault to aim to have working.
    pub target_deployment: Decimal,
    /// Below this, an order is not worth the spread it would pay.
    pub minimum_notional: Decimal,
    /// How far past the quoted price an order may fill.
    pub slippage: Decimal,
    /// How far short of its target the carry may be before more is added.
    pub rebalance_band: Decimal,
}

impl Settings {
    /// The numbers in `config.rs`.
    pub fn shipped() -> Self {
        Self {
            spot: config::SPOT.to_string(),
            perp: config::PERP.to_string(),
            entry_funding: decimal(config::ENTRY_FUNDING),
            exit_funding: decimal(config::EXIT_FUNDING),
            target_deployment: decimal(config::TARGET_DEPLOYMENT),
            minimum_notional: decimal(config::MINIMUM_NOTIONAL),
            slippage: decimal(config::SLIPPAGE),
            rebalance_band: decimal(config::REBALANCE_BAND),
        }
    }
}

/// The limits in `config.rs`, as numbers.
pub fn limits() -> Limits {
    Limits {
        max_gross_leverage: decimal(config::MAX_GROSS_LEVERAGE),
        max_order_fraction: decimal(config::MAX_ORDER_FRACTION),
        idle_buffer: decimal(config::IDLE_BUFFER),
        drawdown_stop: decimal(config::DRAWDOWN_STOP),
    }
}

fn zero() -> Decimal {
    Decimal::from(0)
}

fn positive(value: Decimal) -> Decimal {
    if value > zero() { value } else { zero() }
}

/// Dollars to put on the venue before trading. Each part is one move: the
/// station's idle dollars go in one deposit, the vault's cash in a pull and
/// a deposit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Funding {
    pub from_station: Decimal,
    pub from_vault: Decimal,
}

/// What to do now.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Nothing would move the carry far enough to be worth a transaction.
    Nothing,
    /// Fund the venue as `fund` says, buy `spot` if there is one, then short
    /// the perpetual to match what the spot holds (see [`hedge`]).
    Build { fund: Funding, spot: Option<Order> },
    /// Buy back `perp` if there is one, then sell the spot down to what is
    /// still short (see [`trim`]).
    Reduce { perp: Option<Order> },
}

/// Whether an event from the venue is worth looking at the account for.
///
/// The venue's market operator posts funding for every market it lists, and
/// a post for a market this strategy doesn't trade can't change what it
/// wants to hold. Its own fills, and anything it can't read, are looked at.
pub fn wakes_for(settings: &Settings, event: &[u8]) -> bool {
    match from_payload::<AccountEvent>(event) {
        Ok(AccountEvent::Funding { market, .. }) => market == settings.perp,
        Ok(AccountEvent::Prices(market)) => {
            market.market == settings.spot || market.market == settings.perp
        }
        Ok(AccountEvent::Filled { .. }) | Err(_) => true,
    }
}

/// Whether a vault share's value has fallen far enough from its peak (see
/// [`PerShare`]) that this strategy should stop opening anything and only
/// get smaller.
pub fn only_reducing(share: &PerShare, limits: &Limits) -> bool {
    share.peak > zero() && (share.peak - share.now) / share.peak >= limits.drawdown_stop
}

/// What the account holds, for [`PerShare`]: each position at its mark, a
/// perpetual's less the funding its longs have paid (`funding_index`), so a
/// short is credited the funding it is paid. A position in a market the
/// account doesn't quote counts no move.
pub fn held(state: &AccountState) -> Vec<Held> {
    state
        .positions
        .iter()
        .filter_map(|position| {
            let market = state.market(&position.market)?;
            let paid = if market.is_perp {
                market.funding_index
            } else {
                zero()
            };
            Some(Held {
                what: position.market.clone(),
                quantity: position.size,
                unit_value: market.mark - paid,
            })
        })
        .collect()
}

/// Work out what to do, given what the account holds, the value of a vault
/// share and its peak, and the vault's cash that leavers are waiting on (see
/// [`Leavers`]).
pub fn decide(
    settings: &Settings,
    limits: &Limits,
    share: &PerShare,
    for_leavers: Decimal,
    state: &AccountState,
) -> Plan {
    let Some(perp) = state.market(&settings.perp) else {
        return Plan::Nothing;
    };
    if state.market(&settings.spot).is_none() {
        return Plan::Nothing;
    }
    let spot_held = state.position(&settings.spot);
    let perp_held = state.position(&settings.perp);
    let holding = spot_held > zero() || perp_held < zero();
    let funding = perp.funding_fraction();
    let reducing = only_reducing(share, limits);

    if holding && (reducing || funding < settings.exit_funding) {
        let perp = unwind(settings, limits, state);
        if perp.is_none() && trim(settings, limits, state, spot_held, perp_held).is_none() {
            return Plan::Nothing;
        }
        return Plan::Reduce { perp };
    }
    if reducing {
        return Plan::Nothing;
    }
    // A leg that didn't fill last time may have left the two sides apart.
    // They are levelled before anything is added: buying more spot while
    // some is still unhedged would only grow a position with a view on the
    // price.
    if hedge(settings, limits, state, spot_held, perp_held).is_some() {
        return Plan::Build {
            fund: Funding::default(),
            spot: None,
        };
    }
    if funding >= settings.entry_funding
        && let Some((fund, spot)) = build_step(settings, limits, for_leavers, state)
    {
        return Plan::Build {
            fund,
            spot: Some(spot),
        };
    }
    Plan::Nothing
}

/// The most of `price` an order may be for: its share of the vault, priced at
/// the furthest the order may fill, so even a fill at the slippage limit stays
/// inside the cap.
pub fn order_size_cap(
    settings: &Settings,
    limits: &Limits,
    state: &AccountState,
    price: Decimal,
) -> Decimal {
    state.vault_value * limits.max_order_fraction / (price * (Decimal::from(1) + settings.slippage))
}

/// What the two legs are worth at mark, held as `spot_held` and `perp_held`,
/// without regard to sign.
fn legs_gross(
    settings: &Settings,
    state: &AccountState,
    spot_held: Decimal,
    perp_held: Decimal,
) -> Decimal {
    let mark = |market: &str| state.market(market).map_or_else(zero, |held| held.mark);
    spot_held.abs() * mark(&settings.spot) + perp_held.abs() * mark(&settings.perp)
}

/// What every open position is worth at mark, added up without regard to
/// sign.
pub fn gross_notional(state: &AccountState) -> Decimal {
    state
        .positions
        .iter()
        .map(|held| {
            let price = state
                .market(&held.market)
                .map(|market| market.mark)
                .unwrap_or_else(zero);
            let size = if held.size < zero() {
                -held.size
            } else {
                held.size
            };
            size * price
        })
        .fold(zero(), |total, one| total + one)
}

/// The notional of each leg of the next pair: half what is still wanted,
/// since a pair adds both legs to the gross, inside the order cap and the
/// leverage ceiling.
pub fn pair_notional(
    wanted: Decimal,
    vault_value: Decimal,
    gross: Decimal,
    limits: &Limits,
    minimum: Decimal,
) -> Decimal {
    let two = Decimal::from(2);
    let per_leg = size_for(wanted / two, vault_value, gross, limits, minimum);
    let headroom = positive(vault_value * limits.max_gross_leverage - gross) / two;
    let per_leg = if per_leg > headroom {
        headroom
    } else {
        per_leg
    };
    if per_leg < minimum { zero() } else { per_leg }
}

/// The next pair's spot leg, and what has to reach the venue first to pay
/// for it; nothing if the carry is close enough to its target, or if the
/// order couldn't be paid for even after moving what may be moved.
fn build_step(
    settings: &Settings,
    limits: &Limits,
    for_leavers: Decimal,
    state: &AccountState,
) -> Option<(Funding, Order)> {
    let spot = state.market(&settings.spot)?;
    let perp = state.market(&settings.perp)?;
    if spot.ask <= zero() || perp.bid <= zero() || state.vault_value <= zero() {
        return None;
    }

    let working = state.vault_value * settings.target_deployment;
    let gross = gross_notional(state);
    let wanted = positive(working - gross);
    // Drift inside the band is not worth the cost of trading it away.
    if wanted < working * settings.rebalance_band {
        return None;
    }
    let per_leg = pair_notional(
        wanted,
        state.vault_value,
        gross,
        limits,
        settings.minimum_notional,
    );
    if per_leg <= zero() {
        return None;
    }

    let fund = funding_for(settings, limits, for_leavers, state, per_leg);
    // The spot is bought with the venue's cash; the short's sale gives it
    // back, so the same dollars pay for every pair in turn. The order is
    // sized so that what it pays at its limit fits both the cash and the
    // order cap.
    let payable = positive(state.cash) + fund.from_station + fund.from_vault;
    let reach = Decimal::from(1) + settings.slippage;
    let most = state.vault_value * limits.max_order_fraction;
    let notional = cap(cap(per_leg * reach, payable), most) / reach;
    if notional < settings.minimum_notional {
        // No order can follow, so nothing is moved either.
        return None;
    }
    Some((
        fund,
        Order {
            market: settings.spot.clone(),
            side: Side::Buy,
            size: notional / spot.ask,
            limit_price: spot.ask * reach,
            reduce_only: false,
        },
    ))
}

/// What to move to the venue before buying `per_leg` of spot.
///
/// The venue is meant to hold the short's whole notional at target, once
/// over: the spot leg's purchase price, which the short's sale hands back,
/// and then the collateral the short rests on. Everything already there
/// counts, at the venue or waiting in the station beside it. Money is moved
/// only when the next order couldn't be paid without it, or when the venue
/// is short of that by more than the band, and then the whole shortfall goes
/// in one move rather than an order's worth at a time. The vault's cash that
/// leavers are waiting on, `for_leavers`, is never part of it.
pub fn funding_for(
    settings: &Settings,
    limits: &Limits,
    for_leavers: Decimal,
    state: &AccountState,
    per_leg: Decimal,
) -> Funding {
    let goal = state.vault_value * settings.target_deployment / Decimal::from(2);
    let there = positive(state.value) + positive(state.station_idle);
    let short_of_goal = positive(goal - there);
    let reach = Decimal::from(1) + settings.slippage;
    let needed_now = positive(per_leg * reach - positive(state.cash));
    if needed_now <= zero() && short_of_goal < goal * settings.rebalance_band {
        return Funding::default();
    }

    // A move too small to place an order with is not worth its gas.
    let worth = |amount: Decimal| {
        if amount < settings.minimum_notional {
            zero()
        } else {
            amount
        }
    };
    // What waits in the station earns nothing there, and it takes one
    // deposit however much of it goes, so all of it goes.
    let from_station = worth(positive(state.station_idle));
    // The buffer stays in the vault, where a withdrawal reaches it without
    // closing anything, and so does what leavers are waiting on.
    let spare =
        positive(state.idle - state.vault_value * limits.idle_buffer - positive(for_leavers));
    // The shortfall from the goal already counts the station's dollars; the
    // next order's needs don't.
    let still = if short_of_goal > needed_now - from_station {
        short_of_goal
    } else {
        positive(needed_now - from_station)
    };
    let from_vault = worth(if spare < still { spare } else { still });
    Funding {
        from_station,
        from_vault,
    }
}

/// The perpetual order that brings the short level with `spot_held`, as long
/// as the gap is worth an order: a sale when the spot is ahead, a reduce-only
/// purchase when the short is. A gap wider than one order is closed in equal
/// orders, so the last of it is never a sliver too small to send.
pub fn hedge(
    settings: &Settings,
    limits: &Limits,
    state: &AccountState,
    spot_held: Decimal,
    perp_held: Decimal,
) -> Option<Order> {
    let perp = state.market(&settings.perp)?;
    let gap = spot_held + perp_held;
    let one = Decimal::from(1);
    if gap > zero() {
        if perp.bid <= zero() {
            return None;
        }
        let gross = legs_gross(settings, state, spot_held, perp_held);
        let headroom = positive(state.vault_value * limits.max_gross_leverage - gross);
        let size = cap(
            in_equal_orders(gap, order_size_cap(settings, limits, state, perp.bid)),
            headroom / perp.bid,
        );
        (size * perp.bid >= settings.minimum_notional).then(|| Order {
            market: settings.perp.clone(),
            side: Side::Sell,
            size,
            limit_price: perp.bid * (one - settings.slippage),
            reduce_only: false,
        })
    } else if gap < zero() && perp_held < zero() {
        if perp.ask <= zero() {
            return None;
        }
        let size = in_equal_orders(
            cap(-gap, -perp_held),
            order_size_cap(settings, limits, state, perp.ask),
        );
        (size * perp.ask >= settings.minimum_notional).then(|| Order {
            market: settings.perp.clone(),
            side: Side::Buy,
            size,
            limit_price: perp.ask * (one + settings.slippage),
            reduce_only: true,
        })
    } else {
        None
    }
}

/// One order's worth of the short bought back, if there is a short.
pub fn unwind(settings: &Settings, limits: &Limits, state: &AccountState) -> Option<Order> {
    let perp = state.market(&settings.perp)?;
    let short = -state.position(&settings.perp);
    if short <= zero() || perp.ask <= zero() {
        return None;
    }
    Some(Order {
        market: settings.perp.clone(),
        side: Side::Buy,
        size: cap(short, order_size_cap(settings, limits, state, perp.ask)),
        limit_price: perp.ask * (Decimal::from(1) + settings.slippage),
        reduce_only: true,
    })
}

/// The spot sale that brings the spot down to what is still short. Closing
/// the spot ahead of the short would leave a naked short, which is exactly
/// the exposure this strategy exists to avoid. While a short is still open a
/// sliver too small to be worth an order waits; once the short is gone,
/// whatever spot is left goes.
pub fn trim(
    settings: &Settings,
    limits: &Limits,
    state: &AccountState,
    spot_held: Decimal,
    perp_held: Decimal,
) -> Option<Order> {
    let spot = state.market(&settings.spot)?;
    let excess = spot_held + perp_held;
    if excess <= zero() || spot.bid <= zero() {
        return None;
    }
    let size = cap(excess, order_size_cap(settings, limits, state, spot.bid));
    if perp_held < zero() && size * spot.bid < settings.minimum_notional {
        return None;
    }
    Some(Order {
        market: settings.spot.clone(),
        side: Side::Sell,
        size,
        limit_price: spot.bid * (Decimal::from(1) - settings.slippage),
        reduce_only: true,
    })
}

fn cap(size: Decimal, most: Decimal) -> Decimal {
    if size > most { most } else { size }
}

/// The first of the fewest equal orders, none above `most`, that add up to
/// `size`.
fn in_equal_orders(size: Decimal, most: Decimal) -> Decimal {
    if size <= most || most <= zero() {
        return cap(size, most);
    }
    size / (size / most).ceil()
}

/// The vault's cash that leavers are waiting on, as far as the strategy can
/// tell.
///
/// A withdrawal the vault's cash can't cover waits for the next valuation,
/// and `buffer-low` says so: the agent brings the shortfall home from the
/// venue. Until a valuation pays the leavers, everything the vault then
/// holds above its buffer is theirs, and moving it back out would only send
/// it on a round trip at the tank's expense. The vault's cash falling by
/// more than the agent itself took out is the leavers being paid, and it
/// frees what was held for them by as much.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Leavers {
    held: Decimal,
    last_idle: Option<Decimal>,
}

impl Leavers {
    /// Cash has been brought home for leavers, and the vault now looks like
    /// `state`.
    pub fn brought_home(&mut self, limits: &Limits, state: &AccountState) {
        self.held = positive(state.idle - state.vault_value * limits.idle_buffer);
        self.last_idle = Some(state.idle);
    }

    /// The vault's cash as read now.
    pub fn seen(&mut self, idle: Decimal) {
        if let Some(last) = self.last_idle
            && idle < last
        {
            self.held = positive(self.held - (last - idle));
        }
        self.last_idle = Some(idle);
    }

    /// The agent asked to take `amount` out of the vault's cash, which is not
    /// a leaver being paid. Counted whether or not it went, which at worst
    /// holds the leavers' cash a little longer.
    pub fn taken(&mut self, amount: Decimal) {
        if let Some(last) = &mut self.last_idle {
            *last -= amount;
        }
    }

    /// What is held for them now.
    pub fn held(&self) -> Decimal {
        self.held
    }
}

/// What the account looked like when the venue or the station last said no:
/// what only the strategy's own actions, a deposit or a withdrawal change,
/// and never a price.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    spot: Decimal,
    perp: Decimal,
    cash: Decimal,
    idle: Decimal,
    station_idle: Decimal,
}

impl Snapshot {
    pub fn of(settings: &Settings, state: &AccountState) -> Self {
        Self {
            spot: state.position(&settings.spot),
            perp: state.position(&settings.perp),
            cash: state.cash,
            idle: state.idle,
            station_idle: state.station_idle,
        }
    }
}

/// The most wake-ups a refusal is waited out for.
const PATIENCE_MOST: u32 = 64;

/// How long to leave a plan the venue or the station refused.
///
/// Sending the same thing into the same refusal on every event is how an
/// agent burns gas doing nothing. After a refusal the agent waits out a
/// number of wake-ups before trying again, doubling with each refusal in a
/// row, up to [`PATIENCE_MOST`]; anything that changes the account (a fill,
/// a deposit, a withdrawal, a move landing) ends the wait at once. It counts
/// events rather than time, so the agent still never sleeps on a timer.
#[derive(Debug, Default)]
pub struct Patience {
    refused_at: Option<Snapshot>,
    strikes: u32,
    waiting: u32,
}

impl Patience {
    /// Whether to hold back now, with the account as it is.
    pub fn holds_back(&mut self, now: &Snapshot) -> bool {
        match &self.refused_at {
            Some(then) if then == now => {
                if self.waiting > 0 {
                    self.waiting -= 1;
                    true
                } else {
                    false
                }
            }
            Some(_) => {
                *self = Self::default();
                false
            }
            None => false,
        }
    }

    /// The plan made with the account at `at` was refused.
    pub fn refused(&mut self, at: Snapshot) {
        self.strikes = if self.refused_at.as_ref() == Some(&at) {
            self.strikes + 1
        } else {
            1
        };
        self.waiting = (1u32 << self.strikes.min(6)).min(PATIENCE_MOST) - 1;
        self.refused_at = Some(at);
    }

    /// Something went through: the next plan is tried at once.
    pub fn went_through(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_sdk::{MarketState, PositionState};

    fn d(text: &str) -> Decimal {
        decimal(text)
    }

    /// A vault with `idle` in its own cash and the venue holding `cash`, the
    /// spot and the perpetual both at 50,000 with a dollar either side, and
    /// funding at `funding` of the mark a day.
    fn state(idle: &str, cash: &str, funding: &str) -> AccountState {
        let price = d("50000");
        let market = |name: &str, is_perp: bool| MarketState {
            market: name.to_string(),
            is_perp,
            mark: price,
            oracle: price,
            bid: price - d("1"),
            ask: price + d("1"),
            funding_index: zero(),
            funding_rate: if is_perp { price * d(funding) } else { zero() },
        };
        let mut state = AccountState {
            cash: d(cash),
            value: d(cash),
            exit_value: d(cash),
            vault_value: zero(),
            idle: d(idle),
            station_idle: zero(),
            margin_bps: 1000,
            positions: Vec::new(),
            markets: vec![market(config::SPOT, false), market(config::PERP, true)],
        };
        state.vault_value = state.idle + state.value;
        state
    }

    /// Hold `spot` and short `perp` (both in units), the venue's value
    /// following them at mark.
    fn holding(mut state: AccountState, spot: &str, perp: &str) -> AccountState {
        let price = d("50000");
        state.positions = vec![
            PositionState {
                market: config::SPOT.to_string(),
                size: d(spot),
            },
            PositionState {
                market: config::PERP.to_string(),
                size: -d(perp),
            },
        ];
        state.value = state.cash + d(spot) * price - d(perp) * price;
        state.vault_value = state.idle + state.station_idle + state.value;
        state
    }

    const ABOVE_ENTRY: &str = "0.001";
    const BELOW_EXIT: &str = "0.00001";

    fn plan(state: &AccountState) -> Plan {
        decide(
            &Settings::shipped(),
            &limits(),
            &PerShare::default(),
            zero(),
            state,
        )
    }

    /// Fill `order` at its quoted price (one dollar either side of the
    /// mark), and keep the venue's books the way the stand-in venue does:
    /// cash pays for a purchase and is credited for a sale.
    fn fill(state: &mut AccountState, order: &Order) {
        let quoted = match order.side {
            Side::Buy => d("50001"),
            Side::Sell => d("49999"),
        };
        let signed = match order.side {
            Side::Buy => order.size,
            Side::Sell => -order.size,
        };
        state.cash -= signed * quoted;
        match state
            .positions
            .iter_mut()
            .find(|held| held.market == order.market)
        {
            Some(held) => held.size += signed,
            None => state.positions.push(PositionState {
                market: order.market.clone(),
                size: signed,
            }),
        }
        state.positions.retain(|held| held.size != zero());
        let price = d("50000");
        state.value = state.cash
            + state
                .positions
                .iter()
                .fold(zero(), |sum, held| sum + held.size * price);
        state.vault_value = state.idle + state.station_idle + state.value;
    }

    /// The transactions the agent would send to carry out `plan`, carried
    /// out against `state` as the stand-in venue would: a pull and a deposit
    /// for a move from the vault, a deposit for one from the station, and
    /// one transaction per order, the short levelled an order at a time.
    fn carry_out(state: &mut AccountState, plan: &Plan) -> usize {
        let settings = Settings::shipped();
        let limits = limits();
        let mut sent = 0;
        match plan {
            Plan::Nothing => {}
            Plan::Build { fund, spot } => {
                if fund.from_station > zero() {
                    state.station_idle -= fund.from_station;
                    state.cash += fund.from_station;
                    sent += 1;
                }
                if fund.from_vault > zero() {
                    state.idle -= fund.from_vault;
                    state.cash += fund.from_vault;
                    sent += 2;
                }
                if let Some(order) = spot {
                    fill(state, order);
                    sent += 1;
                }
                for _ in 0..8 {
                    let spot_held = state.position(config::SPOT);
                    let perp_held = state.position(config::PERP);
                    let Some(order) = hedge(&settings, &limits, state, spot_held, perp_held) else {
                        break;
                    };
                    fill(state, &order);
                    sent += 1;
                }
            }
            Plan::Reduce { perp } => {
                if let Some(order) = perp {
                    fill(state, order);
                    sent += 1;
                }
                let spot_held = state.position(config::SPOT);
                let perp_held = state.position(config::PERP);
                if let Some(order) = trim(&settings, &limits, state, spot_held, perp_held) {
                    fill(state, &order);
                    sent += 1;
                }
            }
        }
        sent
    }

    /// Wake the agent until it has nothing more to do, and count what it
    /// sent.
    fn settle(state: &mut AccountState) -> usize {
        let mut sent = 0;
        for _ in 0..100 {
            let next = plan(state);
            if next == Plan::Nothing {
                return sent;
            }
            sent += carry_out(state, &next);
        }
        panic!("the agent never settled");
    }

    #[test]
    fn a_deposit_is_deployed_with_one_move_and_one_order_per_leg_per_step() {
        let mut state = state("25000", "0", ABOVE_ENTRY);
        let first = plan(&state);
        let Plan::Build { fund, spot } = &first else {
            panic!("a fresh deposit with funding above entry opens: {first:?}");
        };
        // The whole carry's worth, once: the short's notional at target.
        assert_eq!(fund.from_vault, d("10000"));
        assert_eq!(fund.from_station, zero());
        assert!(spot.is_some());

        let sent = settle(&mut state);
        // One pull and one deposit, then eight pairs of 5% of the vault.
        assert_eq!(sent, 2 + 8 * 2);
        let gross = gross_notional(&state);
        let target = state.vault_value * d(config::TARGET_DEPLOYMENT);
        assert!(gross <= target && gross >= target * d("0.9"), "{gross}");
        assert_eq!(
            state.position(config::SPOT),
            -state.position(config::PERP),
            "the legs are level"
        );
    }

    #[test]
    fn a_quiet_market_sends_nothing() {
        let mut state = state("25000", "0", ABOVE_ENTRY);
        settle(&mut state);
        // A day of funding posts every few minutes: the price still, the
        // funding paid into the account a few cents at a time.
        for _ in 0..(24 * 60 / 3) {
            state.cash += d("0.01");
            state.value += d("0.01");
            state.vault_value += d("0.01");
            assert_eq!(plan(&state), Plan::Nothing);
        }
    }

    #[test]
    fn small_price_drift_inside_the_band_sends_nothing() {
        let mut state = state("25000", "0", ABOVE_ENTRY);
        settle(&mut state);
        for market in &mut state.markets {
            market.mark *= d("0.95");
            market.bid = market.mark - d("1");
            market.ask = market.mark + d("1");
            market.funding_rate = if market.is_perp {
                market.mark * d(ABOVE_ENTRY)
            } else {
                zero()
            };
        }
        // Hedged, so the value hardly moves; the gross falls 5%.
        assert_eq!(plan(&state), Plan::Nothing);
    }

    #[test]
    fn cash_already_at_the_venue_is_used_before_more_is_moved() {
        // Most of the vault is already at the venue and nothing is open.
        let state = state("2000", "23000", ABOVE_ENTRY);
        let Plan::Build { fund, spot } = plan(&state) else {
            panic!("it opens");
        };
        assert_eq!(fund, Funding::default(), "nothing more is moved");
        assert!(spot.is_some());
    }

    #[test]
    fn money_waiting_in_the_station_goes_on_before_any_more_is_pulled() {
        let mut state = state("15000", "0", ABOVE_ENTRY);
        state.station_idle = d("10000");
        state.vault_value = d("25000");
        let Plan::Build { fund, .. } = plan(&state) else {
            panic!("it opens");
        };
        assert_eq!(fund.from_station, d("10000"));
        assert_eq!(fund.from_vault, zero());
    }

    #[test]
    fn nothing_is_moved_when_no_order_could_follow() {
        // The vault is down to its buffer and the venue holds too little for
        // the smallest order: moving what little there is would be gas for
        // nothing.
        let mut state = state("1500", "50", ABOVE_ENTRY);
        state.vault_value = d("25000");
        assert_eq!(plan(&state), Plan::Nothing);
    }

    #[test]
    fn funding_below_entry_opens_nothing() {
        let state = state("25000", "0", "0.0001");
        assert_eq!(plan(&state), Plan::Nothing);
    }

    #[test]
    fn a_spot_leg_that_did_not_fill_is_not_shorted() {
        let state = state("25000", "0", ABOVE_ENTRY);
        let settings = Settings::shipped();
        // The spot leg missed: nothing to short against.
        assert_eq!(hedge(&settings, &limits(), &state, zero(), zero()), None);
    }

    #[test]
    fn a_short_left_without_its_spot_is_bought_back_before_more_is_added() {
        // The spot leg missed on an earlier step and the short went through.
        let state = holding(state("15000", "11000", "0.0001"), "0", "0.02");
        let Plan::Build { fund, spot: None } = plan(&state) else {
            panic!("the legs are levelled");
        };
        assert_eq!(fund, Funding::default());
        let order = hedge(&Settings::shipped(), &limits(), &state, zero(), -d("0.02")).unwrap();
        assert_eq!(order.side, Side::Buy);
        assert!(order.reduce_only);
    }

    #[test]
    fn an_unwind_never_sells_spot_ahead_of_the_short() {
        let state = holding(state("15000", "10000", BELOW_EXIT), "0.2", "0.2");
        let Plan::Reduce { perp: Some(perp) } = plan(&state) else {
            panic!("funding below exit unwinds");
        };
        assert_eq!(perp.side, Side::Buy);
        let settings = Settings::shipped();
        // The short's purchase missed: the spot stays.
        assert_eq!(
            trim(&settings, &limits(), &state, d("0.2"), -d("0.2")),
            None
        );
        // It went through: the spot follows it down.
        let after = -d("0.2") + perp.size;
        let spot = trim(&settings, &limits(), &state, d("0.2"), after).unwrap();
        // Level, to the last of the decimal's 34 digits.
        let apart = spot.size - perp.size;
        assert!(apart < d("1e-30") && apart > -d("1e-30"), "{apart}");
    }

    #[test]
    fn spot_left_over_from_an_unwind_is_still_sold() {
        // The short is gone and spot is left: a carry this strategy no longer
        // has, and a view on the price it never means to hold.
        let state = holding(state("15000", "0", BELOW_EXIT), "0.1", "0");
        let Plan::Reduce { perp: None } = plan(&state) else {
            panic!("the spot is sold");
        };
    }

    /// Every order the strategy sends, filled at the furthest its slippage
    /// lets it, is still no more than 5% of the vault, as the manifest
    /// claims: the spot buys while building, the shorts that hedge them, the
    /// short bought back and the spot sold on the way out.
    #[test]
    fn an_order_filled_at_its_slippage_limit_is_never_above_its_share_of_the_vault() {
        let settings = Settings::shipped();
        let limits = limits();
        let within = |state: &AccountState, order: &Order| {
            let cap = state.vault_value * d(config::MAX_ORDER_FRACTION);
            let price = state.market(&order.market).unwrap();
            let furthest = match order.side {
                Side::Buy => price.ask * (Decimal::from(1) + settings.slippage),
                // A sale fills at the bid or below; the bid itself is the
                // most it is worth.
                Side::Sell => price.bid * (Decimal::from(1) + settings.slippage),
            };
            assert!(
                order.size * furthest <= cap,
                "{order:?} could fill at {} of a {cap} cap",
                order.size * furthest
            );
        };
        let mut state = state("100000", "0", ABOVE_ENTRY);
        let mut orders = 0;
        for _ in 0..40 {
            let next = plan(&state);
            if let Plan::Build {
                spot: Some(order), ..
            } = &next
            {
                within(&state, order);
                let mut after = state.clone();
                fill(&mut after, order);
                let spot = after.position(config::SPOT);
                let perp = after.position(config::PERP);
                within(
                    &state,
                    &hedge(&settings, &limits, &state, spot, perp).unwrap(),
                );
                orders += 2;
            }
            if next == Plan::Nothing {
                break;
            }
            carry_out(&mut state, &next);
        }
        assert!(orders > 0);
        for market in &mut state.markets {
            market.funding_rate = market.mark * d(BELOW_EXIT);
        }
        let Plan::Reduce { perp: Some(perp) } = plan(&state) else {
            panic!("funding below exit unwinds");
        };
        within(&state, &perp);
        let spot = state.position(config::SPOT);
        let short = state.position(config::PERP) + perp.size;
        within(
            &state,
            &trim(&settings, &limits, &state, spot, short).unwrap(),
        );
    }

    /// A spot buy whose short missed leaves the vault with a view on the
    /// price. The next step shorts against it before buying anything more,
    /// and keeps shorting, an order at a time, until the two legs are level.
    #[test]
    fn spot_left_without_its_short_is_hedged_before_more_is_bought() {
        // 0.1 BTC ($5,000) bought on the last step; its short missed.
        let mut state = holding(state("50000", "45000", ABOVE_ENTRY), "0.1", "0");
        let first = plan(&state);
        let Plan::Build { fund, spot: None } = &first else {
            panic!("more spot is bought on top of an unhedged 0.1 BTC: {first:?}");
        };
        assert_eq!(*fund, Funding::default(), "nothing is moved to hedge");
        carry_out(&mut state, &first);
        assert_eq!(state.position(config::SPOT), -state.position(config::PERP));
    }

    /// Once the hedge has missed more than once, the unhedged spot can be
    /// wider than one order. It is still all shorted before more is bought,
    /// within the per-order cap, across as many orders as it takes.
    #[test]
    fn a_gap_wider_than_one_order_is_hedged_in_full_before_more_is_bought() {
        // 0.25 BTC ($12,500) unhedged in a $100,000 vault: two and a half
        // orders' worth.
        let mut state = holding(state("50000", "37500", ABOVE_ENTRY), "0.25", "0");
        let first = plan(&state);
        assert!(matches!(first, Plan::Build { spot: None, .. }), "{first:?}");
        let sent = carry_out(&mut state, &first);
        assert_eq!(sent, 3, "three orders, each inside the cap");
        assert_eq!(state.position(config::SPOT), -state.position(config::PERP));
    }

    /// When a fresh spot buy lands on top of spot whose short missed, the
    /// short that follows covers all of it, not just the new order.
    #[test]
    fn the_short_after_a_spot_buy_covers_everything_unhedged() {
        // A sliver unhedged, under the smallest order, so the step builds.
        let mut state = holding(state("50000", "45000", ABOVE_ENTRY), "0.001", "0");
        let next = plan(&state);
        assert!(
            matches!(next, Plan::Build { spot: Some(_), .. }),
            "{next:?}"
        );
        carry_out(&mut state, &next);
        assert_eq!(state.position(config::SPOT), -state.position(config::PERP));
    }

    /// `state`'s value of a share, after `share` has looked at it.
    fn looked(share: &mut PerShare, state: &AccountState) -> PerShare {
        share.look(state.vault_value, held(state));
        share.clone()
    }

    #[test]
    fn a_leaver_taking_a_tenth_of_the_vault_does_not_stop_the_agent() {
        // A $100,000 vault carrying 0.8 against 0.8 short, $15,000 of it at
        // the venue in cash and $85,000 in the vault.
        let before = holding(state("85000", "15000", ABOVE_ENTRY), "0.8", "0.8");
        let mut share = PerShare::default();
        looked(&mut share, &before);
        // A holder of 15% of the shares leaves, and their fraction of both
        // legs and of the cash goes with them. No price moved.
        let after = holding(state("72250", "12750", ABOVE_ENTRY), "0.68", "0.68");
        assert_eq!(after.vault_value, d("85000"));
        let share = looked(&mut share, &after);
        assert_eq!(share.now, d("1"));
        assert!(!only_reducing(&share, &limits()));
        assert_eq!(
            decide(&Settings::shipped(), &limits(), &share, zero(), &after),
            plan(&after)
        );
    }

    #[test]
    fn a_fall_in_a_share_s_value_past_the_stop_only_reduces() {
        let before = holding(state("85000", "15000", ABOVE_ENTRY), "0.8", "0.8");
        let mut share = PerShare::default();
        looked(&mut share, &before);
        // The spot falls $14,000 under a perpetual that doesn't: $11,200 of
        // a $100,000 vault lost.
        let mut after = before.clone();
        for market in &mut after.markets {
            if !market.is_perp {
                market.mark -= d("14000");
            }
        }
        after.vault_value -= d("11200");
        let share = looked(&mut share, &after);
        assert!(only_reducing(&share, &limits()), "{:?}", share.now);
        assert!(matches!(
            decide(&Settings::shipped(), &limits(), &share, zero(), &after),
            Plan::Reduce { .. }
        ));
        // Funding the short is paid counts for it: the same fall with
        // $2,000 a unit paid to shorts meanwhile is under the stop.
        let mut paid = after.clone();
        for market in &mut paid.markets {
            if market.is_perp {
                market.funding_index += d("2000");
            }
        }
        let mut share = PerShare::default();
        looked(&mut share, &before);
        let share = looked(&mut share, &paid);
        assert!(!only_reducing(&share, &limits()), "{:?}", share.now);
    }

    /// `buffer-low` brings cash home from the venue for leavers waiting on a
    /// valuation. Until the valuation pays them, that cash stays in the vault
    /// however far the carry is from its target.
    #[test]
    fn cash_brought_home_for_leavers_is_not_sent_back_out() {
        // A $100,000 vault half way to its target carry, funding above
        // entry. A holder of 92% asks to leave and $91,000 comes home: the
        // vault now holds $97,000, the venue $3,000 and the carry.
        let mut home = holding(state("97000", "3000", ABOVE_ENTRY), "0.4", "0.4");
        home.vault_value = d("100000");
        let mut leavers = Leavers::default();
        leavers.brought_home(&limits(), &home);
        let decided = |state: &AccountState, leavers: &Leavers| {
            decide(
                &Settings::shipped(),
                &limits(),
                &PerShare::default(),
                leavers.held(),
                state,
            )
        };
        let plan = decided(&home, &leavers);
        if let Plan::Build { fund, .. } = &plan {
            assert_eq!(fund.from_vault, zero(), "{plan:?}");
        }

        // A valuation pays them: the vault's cash falls by what they were
        // owed. Nothing is held for them any more, so a deposit after that
        // is put to work as usual.
        leavers.seen(d("6000"));
        assert_eq!(leavers.held(), zero());
        let deposit = state("25000", "0", ABOVE_ENTRY);
        leavers.seen(deposit.idle);
        let Plan::Build { fund, .. } = decided(&deposit, &leavers) else {
            panic!("a deposit after the leavers are paid is put to work");
        };
        assert_eq!(fund.from_vault, d("10000"));
    }

    /// The agent's own pulls from the vault's cash don't count as leavers
    /// being paid.
    #[test]
    fn the_agents_own_pull_does_not_free_the_leavers_cash() {
        let mut state = state("50000", "0", ABOVE_ENTRY);
        let mut leavers = Leavers::default();
        leavers.brought_home(&limits(), &state);
        let held = leavers.held();
        leavers.taken(d("1000"));
        state.idle -= d("1000");
        leavers.seen(state.idle);
        assert_eq!(leavers.held(), held);
    }

    #[test]
    fn funding_posted_for_another_market_does_not_wake_it() {
        let settings = Settings::shipped();
        let funding = |market: &str| {
            strategy_sdk::to_payload(&AccountEvent::Funding {
                market: market.to_string(),
                paid_per_unit: d("0.5"),
            })
        };
        assert!(!wakes_for(&settings, &funding("ETH-PERP")));
        assert!(wakes_for(&settings, &funding(config::PERP)));
        assert!(wakes_for(&settings, b"not an event"));
    }

    #[test]
    fn a_refusal_is_not_resent_on_every_event() {
        let settings = Settings::shipped();
        let state = state("25000", "0", ABOVE_ENTRY);
        let at = Snapshot::of(&settings, &state);
        let mut patience = Patience::default();
        let mut tried = 0;
        for _ in 0..200 {
            if !patience.holds_back(&at) {
                tried += 1;
                patience.refused(at.clone());
            }
        }
        // Waits of 1, 3, 7, 15, 31, 63, then 63 again.
        assert!(tried <= 8, "{tried} tries in 200 wake-ups");
        assert!(tried >= 4, "it never gives up for good");
    }

    #[test]
    fn a_change_to_the_account_ends_the_wait() {
        let settings = Settings::shipped();
        let before = state("25000", "0", ABOVE_ENTRY);
        let mut patience = Patience::default();
        patience.refused(Snapshot::of(&settings, &before));
        patience.refused(Snapshot::of(&settings, &before));
        let after = state("30000", "0", ABOVE_ENTRY);
        assert!(!patience.holds_back(&Snapshot::of(&settings, &after)));
    }
}
