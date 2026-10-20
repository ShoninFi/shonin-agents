//! What the strategy wants to do, worked out from what it can see.
//!
//! Everything here is arithmetic on the account and the latest quotes: no
//! host call, nothing sent. The program around it reads the account, asks
//! what to do, and does it, which is what lets the tests below pin down when
//! the strategy spends gas and when it doesn't.
//!
//! Orders and moves between the account's own perp and spot balances are
//! signed actions on HyperCore and cost no gas. Bringing money from the
//! vault does: a pull and a deposit on HyperEVM, paid from the vault's tank.
//! So the account is funded for the whole carry in one move, counting what
//! is already outside the vault's cash, and only when an order can follow;
//! dollars already waiting in the station go in first.

use hl::{AccountEvent, AccountState, Order, Side, Tif};
use strategy_sdk::hyperliquid as hl;
use strategy_sdk::{Decimal, Limits, decimal, from_payload, size_for};

use crate::config;
use crate::per_share::{Held, PerShare};

/// The limits in `config.rs`, as numbers.
pub fn limits() -> Limits {
    Limits {
        max_gross_leverage: decimal(config::MAX_GROSS_LEVERAGE),
        max_order_fraction: decimal(config::MAX_ORDER_FRACTION),
        idle_buffer: decimal(config::IDLE_BUFFER),
        drawdown_stop: decimal(config::DRAWDOWN_STOP),
    }
}

pub fn zero() -> Decimal {
    Decimal::from(0)
}

fn positive(value: Decimal) -> Decimal {
    if value > zero() { value } else { zero() }
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

/// The most of `price` an order may be for: its share of the vault, priced at
/// the furthest the order may fill, so even a fill at the slippage limit stays
/// inside the cap.
pub fn order_size_cap(limits: &Limits, state: &AccountState, price: Decimal) -> Decimal {
    state.vault_value * limits.max_order_fraction
        / (price * (Decimal::from(1) + decimal(config::SLIPPAGE)))
}

/// The best bid and ask of one leg, as last heard.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Top {
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

/// What the strategy knows of the market: both legs' books and the funding.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quotes {
    pub spot: Top,
    pub perp: Top,
    /// Funding, as a fraction of notional per day.
    pub funding: Option<Decimal>,
}

/// The legs as the account holds them: spot tokens, and the perp position
/// (negative when short).
pub fn held(state: &AccountState) -> (Decimal, Decimal) {
    let spot = state
        .spot
        .iter()
        .find(|balance| balance.token == config::SPOT_TOKEN)
        .map_or_else(zero, |balance| balance.total);
    let perp = state
        .positions
        .iter()
        .find(|position| position.market == config::PERP)
        .map_or_else(zero, |position| position.size);
    (spot, perp)
}

/// The dollars on the account's spot side that no order holds.
pub fn spot_dollars(state: &AccountState) -> Decimal {
    state
        .spot
        .iter()
        .find(|balance| balance.token == "USDC")
        .map_or_else(zero, |balance| balance.total - balance.hold)
}

/// `size` rounded down to `decimals` places.
pub fn round_down(size: Decimal, decimals: u32) -> Decimal {
    let scale = Decimal::from(10u64.pow(decimals));
    (size * scale).floor() / scale
}

/// `amount` rounded up to whole cents.
pub fn round_up_cents(amount: Decimal) -> Decimal {
    let scale = Decimal::from(100);
    (amount * scale).ceil() / scale
}

/// An immediate-or-cancel order, its limit written as the venue takes it.
pub fn order(market: &str, side: Side, size: Decimal, limit: Decimal, reduce_only: bool) -> Order {
    Order {
        market: market.to_string(),
        side,
        size,
        limit_price: five_figures(limit, side),
        tif: Tif::Ioc,
        reduce_only,
        client_id: None,
    }
}

/// A price at five significant figures, the most the venue takes, rounded
/// away from the market (down for a buy, up for a sell) so it never reaches
/// further than meant.
pub fn five_figures(price: Decimal, side: Side) -> Decimal {
    if price <= zero() {
        return price;
    }
    let mut scale = Decimal::from(1);
    let ten = Decimal::from(10);
    while price * scale < Decimal::from(10_000) {
        scale *= ten;
    }
    while price * scale >= Decimal::from(100_000) {
        scale /= ten;
    }
    let scaled = price * scale;
    let rounded = match side {
        Side::Buy => scaled.floor(),
        Side::Sell => scaled.ceil(),
    };
    rounded / scale
}

/// Dollars to bring to the account before trading. Each part is one move:
/// the station's idle dollars in one deposit, the vault's cash in a pull and
/// a deposit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Funding {
    pub from_station: Decimal,
    pub from_vault: Decimal,
}

/// What to do now.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Nothing would move the carry far enough to be worth doing.
    Nothing,
    /// Fund the account as `fund` says, buy `spot` if there is one, then
    /// short the perpetual to match what the spot holds (see [`hedge`]).
    Build { fund: Funding, spot: Option<Order> },
    /// Buy back `perp` if there is one, then sell the spot down to what is
    /// still short (see [`trim`]).
    Reduce { perp: Option<Order> },
}

/// Which side of the funding thresholds the market is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    /// Worth opening.
    Open,
    /// Worth holding, not adding to.
    Hold,
    /// Worth unwinding.
    Exit,
}

pub fn regime(funding: Decimal) -> Regime {
    if funding >= decimal(config::ENTRY_FUNDING) {
        Regime::Open
    } else if funding < decimal(config::EXIT_FUNDING) {
        Regime::Exit
    } else {
        Regime::Hold
    }
}

/// Whether a funding update is worth looking at the account for.
///
/// The venue sends one every few seconds. When the last look found nothing
/// to do and the funding is on the same side of its thresholds, nothing it
/// says can change the answer; the account's own events (fills, the hourly
/// funding payment) still wake the strategy whatever the funding does.
pub fn steps_on_funding(before: Option<Decimal>, now: Decimal, busy: bool) -> bool {
    busy || before.map(regime) != Some(regime(now))
}

/// Whether an event on the account is worth looking at it for. An order's
/// change of state says nothing a fill doesn't: an order that filled sends
/// its fill too, and one cancelled unfilled changed nothing.
pub fn wakes_for(event: &[u8]) -> bool {
    !matches!(
        from_payload::<AccountEvent>(event),
        Ok(AccountEvent::Order { .. })
    )
}

/// Whether a vault share's value has fallen far enough from its peak (see
/// [`PerShare`]), now or at an earlier look the stop hasn't lifted since,
/// that this strategy should stop opening anything and only get smaller.
pub fn only_reducing(share: &PerShare, limits: &Limits) -> bool {
    share.stopped()
        || (share.peak > zero() && (share.peak - share.now) / share.peak >= limits.drawdown_stop)
}

/// What the account holds, for [`PerShare`]: the spot and the perpetual each
/// at the middle of its book, and the funding the account has been paid
/// since the agent started (`funding`, negative when it paid), whose growth
/// is what it earned. A leg whose book isn't known counts no move.
pub fn share_holdings(state: &AccountState, quotes: &Quotes, funding: Decimal) -> Vec<Held> {
    let mid = |top: Top| match (top.bid, top.ask) {
        (Some(bid), Some(ask)) if bid > zero() && ask > zero() => {
            Some((bid + ask) / Decimal::from(2))
        }
        _ => None,
    };
    let (spot, perp) = held(state);
    let mut holdings = vec![Held {
        what: "funding".to_string(),
        quantity: Decimal::from(1),
        unit_value: funding,
    }];
    for (what, quantity, top) in [("spot", spot, quotes.spot), ("perp", perp, quotes.perp)] {
        if let Some(price) = mid(top) {
            holdings.push(Held {
                what: what.to_string(),
                quantity,
                unit_value: price,
            });
        }
    }
    holdings
}

/// The funding the account was paid (negative when it paid), as an event
/// from the venue says.
pub fn funding_paid(event: &[u8]) -> Decimal {
    match from_payload::<AccountEvent>(event) {
        Ok(AccountEvent::Funding { amount, .. }) => amount,
        _ => zero(),
    }
}

/// What both legs are worth at the prices they would close at.
pub fn gross(state: &AccountState, quotes: &Quotes) -> Decimal {
    let (spot, perp) = held(state);
    legs_gross(quotes, spot, perp)
}

/// What the legs would be worth at those prices, held as `spot` and `perp`.
fn legs_gross(quotes: &Quotes, spot: Decimal, perp: Decimal) -> Decimal {
    let spot_price = quotes.spot.ask.unwrap_or_else(zero);
    let perp_price = quotes.perp.bid.unwrap_or_else(zero);
    positive(spot) * spot_price + positive(-perp) * perp_price
}

/// Work out what to do, given the account, the market, the most the vault
/// has been worth, and the vault's cash that leavers are waiting on (see
/// [`Leavers`]).
pub fn decide(
    limits: &Limits,
    share: &PerShare,
    for_leavers: Decimal,
    state: &AccountState,
    quotes: &Quotes,
) -> Plan {
    let Some(funding) = quotes.funding else {
        return Plan::Nothing;
    };
    let (spot, perp) = held(state);
    let holding = spot > zero() || perp < zero();
    let reducing = only_reducing(share, limits);

    if holding && (reducing || regime(funding) == Regime::Exit) {
        let perp_order = unwind(limits, state, quotes);
        if perp_order.is_none() && trim(limits, state, quotes, spot, perp).is_none() {
            return Plan::Nothing;
        }
        return Plan::Reduce { perp: perp_order };
    }
    if reducing {
        return Plan::Nothing;
    }
    // A leg that didn't fill last time may have left the two sides apart.
    // They are levelled before anything is added: buying more spot while
    // some is still unhedged would only grow a position with a view on the
    // price.
    if hedge(limits, state, quotes, spot, perp).is_some() {
        return Plan::Build {
            fund: Funding::default(),
            spot: None,
        };
    }
    if regime(funding) == Regime::Open
        && let Some((fund, spot_order)) = build_step(limits, for_leavers, state, quotes)
    {
        return Plan::Build {
            fund,
            spot: Some(spot_order),
        };
    }
    Plan::Nothing
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
    let per_leg = cap(per_leg, headroom);
    if per_leg < minimum { zero() } else { per_leg }
}

/// The dollars one dollar of pair takes from the account: the spot paid in
/// full at its limit, and the short's margin at its leverage.
fn dollars_per_dollar() -> Decimal {
    Decimal::from(1)
        + decimal(config::SLIPPAGE)
        + Decimal::from(1) / Decimal::from(config::PERP_LEVERAGE)
}

/// The next pair's spot leg, and what has to reach the account first to pay
/// for it; nothing if the carry is close enough to its target, or if the
/// order couldn't be paid for even after moving what may be moved.
fn build_step(
    limits: &Limits,
    for_leavers: Decimal,
    state: &AccountState,
    quotes: &Quotes,
) -> Option<(Funding, Order)> {
    let (Some(spot_ask), Some(perp_bid)) = (quotes.spot.ask, quotes.perp.bid) else {
        return None;
    };
    if spot_ask <= zero() || perp_bid <= zero() || state.vault_value <= zero() {
        return None;
    }
    let minimum = decimal(config::MINIMUM_NOTIONAL);
    let band = decimal(config::REBALANCE_BAND);
    let working = state.vault_value * decimal(config::TARGET_DEPLOYMENT);
    let gross = gross(state, quotes);
    let wanted = positive(working - gross);
    if wanted < working * band {
        return None;
    }
    let per_leg = pair_notional(wanted, state.vault_value, gross, limits, minimum);
    if per_leg <= zero() {
        return None;
    }

    let fund = funding_for(limits, for_leavers, state, per_leg);
    let available = positive(spot_dollars(state))
        + positive(state.withdrawable)
        + fund.from_station
        + fund.from_vault;
    let notional = cap(per_leg, available / dollars_per_dollar());
    let size = round_down(
        cap(notional / spot_ask, order_size_cap(limits, state, spot_ask)),
        config::SPOT_SIZE_DECIMALS,
    );
    if size * spot_ask < minimum {
        // No order can follow, so nothing is moved either.
        return None;
    }
    let reach = Decimal::from(1) + decimal(config::SLIPPAGE);
    Some((
        fund,
        order(config::SPOT, Side::Buy, size, spot_ask * reach, false),
    ))
}

/// What to bring to the account before buying `per_leg` of spot.
///
/// The account is meant to hold the whole carry at target: the spot leg paid
/// in full and the short's notional again as its margin, which is the
/// deployment target of the vault. Every dollar outside the vault's cash
/// counts towards it, including any waiting in the station. Money is moved
/// only when the next pair couldn't be paid without it, or when the account
/// is short of that by more than the band, and then the whole shortfall goes
/// in one move rather than an order's worth at a time: the station's idle
/// dollars first, then the vault's cash. The vault's cash that leavers are
/// waiting on, `for_leavers`, is never part of it.
pub fn funding_for(
    limits: &Limits,
    for_leavers: Decimal,
    state: &AccountState,
    per_leg: Decimal,
) -> Funding {
    let goal = state.vault_value * decimal(config::TARGET_DEPLOYMENT);
    let outside = positive(state.vault_value - state.idle);
    let short_of_goal = positive(goal - outside);
    let available = positive(spot_dollars(state)) + positive(state.withdrawable);
    let needed_now = positive(per_leg * dollars_per_dollar() - available);
    if needed_now <= zero() && short_of_goal < goal * decimal(config::REBALANCE_BAND) {
        return Funding::default();
    }
    // A move too small to place an order with is not worth its gas.
    let worth = |amount: Decimal| {
        if amount < decimal(config::MINIMUM_NOTIONAL) {
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
    // next pair's needs don't.
    let still = if short_of_goal > needed_now - from_station {
        short_of_goal
    } else {
        positive(needed_now - from_station)
    };
    let from_vault = worth(cap(still, spare));
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
    limits: &Limits,
    state: &AccountState,
    quotes: &Quotes,
    spot_held: Decimal,
    perp_held: Decimal,
) -> Option<Order> {
    let minimum = decimal(config::MINIMUM_NOTIONAL);
    let slip = decimal(config::SLIPPAGE);
    let one = Decimal::from(1);
    let gap = round_down(spot_held, config::PERP_SIZE_DECIMALS) + perp_held;
    if gap > zero() {
        let bid = quotes.perp.bid.filter(|bid| *bid > zero())?;
        let gross = legs_gross(quotes, spot_held, perp_held);
        let headroom = positive(state.vault_value * limits.max_gross_leverage - gross);
        let size = round_down(
            cap(
                in_equal_orders(gap, order_size_cap(limits, state, bid)),
                headroom / bid,
            ),
            config::PERP_SIZE_DECIMALS,
        );
        (size * bid >= minimum)
            .then(|| order(config::PERP, Side::Sell, size, bid * (one - slip), false))
    } else if gap < zero() && perp_held < zero() {
        let ask = quotes.perp.ask.filter(|ask| *ask > zero())?;
        let size = round_down(
            in_equal_orders(cap(-gap, -perp_held), order_size_cap(limits, state, ask)),
            config::PERP_SIZE_DECIMALS,
        );
        (size * ask >= minimum)
            .then(|| order(config::PERP, Side::Buy, size, ask * (one + slip), true))
    } else {
        None
    }
}

/// One order's worth of the short bought back, if there is a short.
pub fn unwind(limits: &Limits, state: &AccountState, quotes: &Quotes) -> Option<Order> {
    let (_, perp) = held(state);
    let ask = quotes.perp.ask.filter(|ask| *ask > zero())?;
    if perp >= zero() {
        return None;
    }
    let size = round_down(
        cap(-perp, order_size_cap(limits, state, ask)),
        config::PERP_SIZE_DECIMALS,
    );
    let slip = decimal(config::SLIPPAGE);
    (size > zero()).then(|| {
        order(
            config::PERP,
            Side::Buy,
            size,
            ask * (Decimal::from(1) + slip),
            true,
        )
    })
}

/// The spot sale that brings the spot down to what is still short. Closing
/// the spot ahead of the short would leave a naked short. The account keeps
/// some of the same token to pay for its sends home, and that is not the
/// strategy's to sell: [`config::HYPE_KEPT_FOR_SENDS_HOME`] always stays,
/// however much is sold around it. A remainder too small to be worth an
/// order stays too.
pub fn trim(
    limits: &Limits,
    state: &AccountState,
    quotes: &Quotes,
    spot_held: Decimal,
    perp_held: Decimal,
) -> Option<Order> {
    let bid = quotes.spot.bid.filter(|bid| *bid > zero())?;
    let sellable = positive(spot_held - decimal(config::HYPE_KEPT_FOR_SENDS_HOME));
    let excess = cap(spot_held + perp_held, sellable);
    if excess <= zero() {
        return None;
    }
    let size = round_down(
        cap(excess, order_size_cap(limits, state, bid)),
        config::SPOT_SIZE_DECIMALS,
    );
    let slip = decimal(config::SLIPPAGE);
    (size * bid >= decimal(config::MINIMUM_NOTIONAL)).then(|| {
        order(
            config::SPOT,
            Side::Sell,
            size,
            bid * (Decimal::from(1) - slip),
            false,
        )
    })
}

/// The vault's cash that leavers are waiting on, as far as the strategy can
/// tell.
///
/// A withdrawal the vault's cash can't cover waits for the next valuation,
/// and `buffer-low` says so: the agent brings the shortfall home from the
/// account. Until a valuation pays the leavers, everything the vault then
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
/// what only the strategy's own actions, a deposit or a withdrawal change.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    spot: Decimal,
    perp: Decimal,
    spot_dollars: Decimal,
    withdrawable: Decimal,
    idle: Decimal,
    station_idle: Decimal,
}

impl Snapshot {
    pub fn of(state: &AccountState) -> Self {
        let (spot, perp) = held(state);
        Self {
            spot,
            perp,
            spot_dollars: spot_dollars(state),
            withdrawable: state.withdrawable,
            idle: state.idle,
            station_idle: state.station_idle,
        }
    }
}

/// The most wake-ups a refusal is waited out for.
const PATIENCE_MOST: u32 = 64;

/// How long to leave a plan the venue or the station refused: a number of
/// wake-ups that doubles with each refusal in a row, up to
/// [`PATIENCE_MOST`], and ends at once when the account changes. It counts
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
    use hl::{Position, SpotBalance};

    fn d(text: &str) -> Decimal {
        decimal(text)
    }

    const ABOVE_ENTRY: &str = "0.001";
    const BELOW_EXIT: &str = "0.00001";

    fn quotes(funding: &str) -> Quotes {
        let top = Top {
            bid: Some(d("24.99")),
            ask: Some(d("25.01")),
        };
        Quotes {
            spot: top,
            perp: top,
            funding: Some(d(funding)),
        }
    }

    /// A vault with `idle` in its own cash and `perp_side` dollars on the
    /// account's perp side, nothing open.
    fn state(idle: &str, perp_side: &str) -> AccountState {
        let mut state = AccountState {
            vault_value: zero(),
            idle: d(idle),
            station_idle: zero(),
            account_value: d(perp_side),
            withdrawable: d(perp_side),
            positions: Vec::new(),
            spot: vec![
                SpotBalance {
                    token: "USDC".to_string(),
                    total: zero(),
                    hold: zero(),
                },
                SpotBalance {
                    token: config::SPOT_TOKEN.to_string(),
                    total: zero(),
                    hold: zero(),
                },
            ],
            open_orders: Vec::new(),
        };
        revalue(&mut state);
        state
    }

    fn balance<'a>(state: &'a mut AccountState, token: &str) -> &'a mut SpotBalance {
        state
            .spot
            .iter_mut()
            .find(|balance| balance.token == token)
            .unwrap()
    }

    fn revalue(state: &mut AccountState) {
        let (spot, _) = held(state);
        state.vault_value = state.idle
            + state.station_idle
            + state.account_value
            + spot_dollars(state)
            + spot * d("25");
    }

    fn perp_size(state: &mut AccountState) -> &mut Decimal {
        if state.positions.is_empty() {
            state.positions.push(Position {
                market: config::PERP.to_string(),
                size: zero(),
                entry_price: None,
                leverage: config::PERP_LEVERAGE,
            });
        }
        &mut state.positions[0].size
    }

    /// Fill `order` at the quote, and keep the account's books the way the
    /// venue does: the spot paid from the spot side's dollars, the short's
    /// margin held on the perp side.
    fn fill(state: &mut AccountState, order: &Order) {
        let margin = Decimal::from(config::PERP_LEVERAGE);
        match (order.market.as_str(), order.side) {
            (config::SPOT, Side::Buy) => {
                balance(state, "USDC").total -= order.size * d("25.01");
                balance(state, config::SPOT_TOKEN).total += order.size;
            }
            (config::SPOT, Side::Sell) => {
                balance(state, "USDC").total += order.size * d("24.99");
                balance(state, config::SPOT_TOKEN).total -= order.size;
            }
            (_, Side::Sell) => {
                *perp_size(state) -= order.size;
                state.withdrawable -= order.size * d("25") / margin;
            }
            (_, Side::Buy) => {
                *perp_size(state) += order.size;
                state.withdrawable += order.size * d("25") / margin;
            }
        }
        revalue(state);
    }

    /// Carry `plan` out as the agent would, counting what costs gas (a
    /// deposit for a move from the station, a pull and a deposit for one from
    /// the vault) apart from the actions on the venue, which don't.
    fn carry_out(state: &mut AccountState, plan: &Plan) -> (usize, usize) {
        let limits = limits();
        let quotes = quotes(ABOVE_ENTRY);
        let (mut gas, mut actions) = (0, 0);
        match plan {
            Plan::Nothing => {}
            Plan::Build { fund, spot } => {
                if fund.from_station > zero() {
                    state.station_idle -= fund.from_station;
                    state.account_value += fund.from_station;
                    state.withdrawable += fund.from_station;
                    gas += 1;
                }
                if fund.from_vault > zero() {
                    state.idle -= fund.from_vault;
                    state.account_value += fund.from_vault;
                    state.withdrawable += fund.from_vault;
                    gas += 2;
                }
                if let Some(order) = spot {
                    let cost = order.size * order.limit_price;
                    let short = cost - spot_dollars(state);
                    if short > zero() {
                        let moving = round_up_cents(short);
                        state.withdrawable -= moving;
                        state.account_value -= moving;
                        balance(state, "USDC").total += moving;
                        actions += 1;
                    }
                    fill(state, order);
                    actions += 1;
                }
                for _ in 0..8 {
                    let (spot, perp) = held(state);
                    let Some(order) = hedge(&limits, state, &quotes, spot, perp) else {
                        break;
                    };
                    fill(state, &order);
                    actions += 1;
                }
            }
            Plan::Reduce { perp } => {
                if let Some(order) = perp {
                    fill(state, order);
                    actions += 1;
                }
                let (spot, perp) = held(state);
                if let Some(order) = trim(&limits, state, &quotes, spot, perp) {
                    fill(state, &order);
                    actions += 1;
                }
            }
        }
        (gas, actions)
    }

    fn plan(state: &AccountState, quotes: &Quotes) -> Plan {
        decide(
            &limits(),
            &PerShare::new(config::DRAWDOWN_STOP, config::STOP_LIFTS_WITHIN),
            zero(),
            state,
            quotes,
        )
    }

    fn settle(state: &mut AccountState) -> (usize, usize) {
        let quotes = quotes(ABOVE_ENTRY);
        let (mut gas, mut actions) = (0, 0);
        for _ in 0..100 {
            let next = plan(state, &quotes);
            if next == Plan::Nothing {
                return (gas, actions);
            }
            let (g, a) = carry_out(state, &next);
            gas += g;
            actions += a;
        }
        panic!("the agent never settled");
    }

    #[test]
    fn a_deposit_is_brought_to_the_venue_in_one_move() {
        let mut state = state("25000", "0");
        let Plan::Build { fund, .. } = plan(&state, &quotes(ABOVE_ENTRY)) else {
            panic!("a fresh deposit with funding above entry opens");
        };
        assert_eq!(fund.from_vault, d("20000"));
        let (gas, _) = settle(&mut state);
        assert_eq!(gas, 2, "one pull and one deposit");
        let (spot, perp) = held(&state);
        assert_eq!(spot, -perp, "the legs are level");
        let target = state.vault_value * d(config::TARGET_DEPLOYMENT);
        let gross = gross(&state, &quotes(ABOVE_ENTRY));
        assert!(gross >= target * d("0.9") && gross <= target, "{gross}");
    }

    #[test]
    fn a_quiet_market_sends_nothing() {
        let mut state = state("25000", "0");
        settle(&mut state);
        for _ in 0..1000 {
            state.account_value += d("0.01");
            state.withdrawable += d("0.01");
            revalue(&mut state);
            assert_eq!(plan(&state, &quotes(ABOVE_ENTRY)), Plan::Nothing);
        }
    }

    #[test]
    fn cash_already_at_the_venue_is_used_before_more_is_moved() {
        let state = state("2000", "23000");
        let Plan::Build { fund, spot } = plan(&state, &quotes(ABOVE_ENTRY)) else {
            panic!("it opens");
        };
        assert_eq!(fund, Funding::default());
        assert!(spot.is_some());
    }

    #[test]
    fn nothing_is_moved_when_no_order_could_follow() {
        let mut state = state("1500", "50");
        state.vault_value = d("25000");
        assert_eq!(plan(&state, &quotes(ABOVE_ENTRY)), Plan::Nothing);
    }

    #[test]
    fn funding_updates_on_the_same_side_of_the_thresholds_do_not_wake_an_idle_agent() {
        let above = d(ABOVE_ENTRY);
        assert!(steps_on_funding(None, above, false));
        assert!(!steps_on_funding(Some(above), above * d("1.1"), false));
        assert!(steps_on_funding(Some(above), d("0.0001"), false));
        assert!(steps_on_funding(Some(above), above, true));
    }

    #[test]
    fn an_order_changing_state_does_not_wake_it_but_a_fill_does() {
        let changed = strategy_sdk::to_payload(&AccountEvent::Order {
            market: config::PERP.to_string(),
            order_id: 1,
            client_id: None,
            status: "canceled".to_string(),
            time_ms: 0,
        });
        assert!(!wakes_for(&changed));
        let paid = strategy_sdk::to_payload(&AccountEvent::Funding {
            market: config::PERP.to_string(),
            amount: d("1"),
            time_ms: 0,
        });
        assert!(wakes_for(&paid));
    }

    #[test]
    fn an_unwind_never_sells_spot_ahead_of_the_short() {
        let mut state = state("5000", "20000");
        settle(&mut state);
        let exit = quotes(BELOW_EXIT);
        let Plan::Reduce { perp: Some(perp) } = plan(&state, &exit) else {
            panic!("funding below exit unwinds");
        };
        let (spot, short) = held(&state);
        assert_eq!(trim(&limits(), &state, &exit, spot, short), None);
        let spot_order = trim(&limits(), &state, &exit, spot, short + perp.size).unwrap();
        assert_eq!(spot_order.size, perp.size);
    }

    #[test]
    fn a_little_of_the_token_kept_for_sends_home_is_never_sold() {
        let mut state = state("15000", "10000");
        balance(&mut state, config::SPOT_TOKEN).total = d("0.5");
        revalue(&mut state);
        assert_eq!(plan(&state, &quotes(BELOW_EXIT)), Plan::Nothing);
    }

    /// The executor keeps up to a whole HYPE on the account to pay for
    /// sends home. However much spot an unwind or a halt sells, that much is
    /// left: the strategy can't tell it from its own, so it never sells it.
    #[test]
    fn the_hype_kept_for_sends_home_is_never_sold() {
        // The short is bought back; the account holds the carry's 9 HYPE and
        // the 0.1 HYPE the executor bought for sends home.
        let mut state = state("1000", "5000");
        balance(&mut state, config::SPOT_TOKEN).total = d("9.1");
        revalue(&mut state);
        let exit = quotes(BELOW_EXIT);
        let kept = d(config::HYPE_KEPT_FOR_SENDS_HOME);
        let order = trim(&limits(), &state, &exit, d("9.1"), zero()).expect("a sale");
        assert_eq!(order.size, d("9.1") - kept);

        // A halt lifts the order cap and sells everything it may, in one go.
        let halt = Limits {
            max_order_fraction: Decimal::from(1),
            ..limits()
        };
        balance(&mut state, config::SPOT_TOKEN).total = d("40");
        revalue(&mut state);
        let order = trim(&halt, &state, &exit, d("40"), zero()).expect("a sale");
        assert_eq!(order.size, d("40") - kept);
        // And with only the reserve left, nothing is sold.
        assert_eq!(trim(&halt, &state, &exit, kept, zero()), None);
    }

    /// Every order the strategy sends, filled at the furthest its slippage
    /// lets it, is still no more than 5% of the vault, as the manifest
    /// claims: the spot buys while building, the shorts that hedge them, the
    /// short bought back and the spot sold on the way out.
    #[test]
    fn an_order_filled_at_its_slippage_limit_is_never_above_its_share_of_the_vault() {
        let limits = limits();
        let open = quotes(ABOVE_ENTRY);
        let reach = Decimal::from(1) + d(config::SLIPPAGE);
        let within = |state: &AccountState, order: &Order| {
            let cap = state.vault_value * d(config::MAX_ORDER_FRACTION);
            let top = if order.market == config::SPOT {
                open.spot
            } else {
                open.perp
            };
            let price = match order.side {
                Side::Buy => top.ask.unwrap(),
                // A sale fills at the bid or below; the bid itself is the
                // most it is worth.
                Side::Sell => top.bid.unwrap(),
            };
            let furthest = order.size * price * reach;
            assert!(
                furthest <= cap,
                "{order:?} could fill at {furthest} of a {cap} cap"
            );
        };
        let mut state = state("100000", "0");
        let mut orders = 0;
        for _ in 0..40 {
            let next = plan(&state, &open);
            if let Plan::Build {
                spot: Some(order), ..
            } = &next
            {
                within(&state, order);
                let mut after = state.clone();
                balance(&mut after, "USDC").total += d("100000");
                fill(&mut after, order);
                let (spot, perp) = held(&after);
                within(&state, &hedge(&limits, &state, &open, spot, perp).unwrap());
                orders += 2;
            }
            if next == Plan::Nothing {
                break;
            }
            carry_out(&mut state, &next);
        }
        assert!(orders > 0);
        let exit = quotes(BELOW_EXIT);
        let Plan::Reduce { perp: Some(perp) } = plan(&state, &exit) else {
            panic!("funding below exit unwinds");
        };
        within(&state, &perp);
        let (spot, short) = held(&state);
        within(
            &state,
            &trim(&limits, &state, &exit, spot, short + perp.size).unwrap(),
        );
    }

    /// A spot buy whose short missed leaves the vault with a view on the
    /// price. The next step shorts against it before buying anything more,
    /// and keeps shorting, an order at a time, until the two legs are level.
    #[test]
    fn spot_left_without_its_short_is_hedged_before_more_is_bought() {
        // 500 HYPE ($12,500, two and a half orders' worth) bought in a
        // $100,000 vault; its short missed.
        let mut state = state("50000", "37500");
        balance(&mut state, config::SPOT_TOKEN).total = d("500");
        revalue(&mut state);
        let first = plan(&state, &quotes(ABOVE_ENTRY));
        let Plan::Build { fund, spot: None } = &first else {
            panic!("more spot is bought on top of an unhedged 500 HYPE: {first:?}");
        };
        assert_eq!(*fund, Funding::default(), "nothing is moved to hedge");
        let (gas, actions) = carry_out(&mut state, &first);
        assert_eq!((gas, actions), (0, 3), "three orders, each inside the cap");
        let (spot, perp) = held(&state);
        assert_eq!(spot, -perp);
    }

    /// Dollars waiting in the station earn nothing there. They go to the
    /// account first, in one deposit, and only the rest comes from the
    /// vault's cash.
    #[test]
    fn money_waiting_in_the_station_goes_on_before_any_more_is_pulled() {
        let mut state = state("15000", "0");
        state.station_idle = d("10000");
        revalue(&mut state);
        let Plan::Build { fund, .. } = plan(&state, &quotes(ABOVE_ENTRY)) else {
            panic!("it opens");
        };
        assert_eq!(fund.from_station, d("10000"));
        assert_eq!(fund.from_vault, d("10000"));
        let (gas, _) = settle(&mut state);
        assert_eq!(gas, 3, "one deposit from the station, one pull and deposit");
        assert_eq!(state.station_idle, zero());
    }

    #[test]
    fn a_leaver_taking_a_tenth_of_the_vault_does_not_stop_the_agent() {
        // A $30,000 vault carrying 400 HYPE against 400 short.
        let mut before = state("15000", "5000");
        balance(&mut before, config::SPOT_TOKEN).total = d("400");
        *perp_size(&mut before) = d("-400");
        revalue(&mut before);
        let quotes = quotes(ABOVE_ENTRY);
        let mut share = PerShare::new(config::DRAWDOWN_STOP, config::STOP_LIFTS_WITHIN);
        share.look(
            before.vault_value,
            Some(before.vault_value),
            share_holdings(&before, &quotes, zero()),
        );
        // A holder of 15% of the shares leaves, and their fraction of both
        // legs and of the cash goes with them. No price moved.
        let mut after = state("12750", "4250");
        balance(&mut after, config::SPOT_TOKEN).total = d("340");
        *perp_size(&mut after) = d("-340");
        revalue(&mut after);
        share.look(
            after.vault_value,
            Some(after.vault_value),
            share_holdings(&after, &quotes, zero()),
        );
        assert_eq!(share.now, d("1"));
        assert!(!only_reducing(&share, &limits()));
        assert_eq!(
            decide(&limits(), &share, zero(), &after, &quotes),
            plan(&after, &quotes)
        );
    }

    #[test]
    fn a_fall_in_a_share_s_value_past_the_stop_only_reduces() {
        let mut held_now = state("15000", "5000");
        balance(&mut held_now, config::SPOT_TOKEN).total = d("400");
        *perp_size(&mut held_now) = d("-400");
        revalue(&mut held_now);
        let mut share = PerShare::new(config::DRAWDOWN_STOP, config::STOP_LIFTS_WITHIN);
        share.look(
            held_now.vault_value,
            Some(held_now.vault_value),
            share_holdings(&held_now, &quotes(ABOVE_ENTRY), zero()),
        );
        // The perpetual $8 higher and the spot where it was: $3,200 lost
        // on the short, more than a tenth of the $30,000 vault.
        let mut apart = quotes(ABOVE_ENTRY);
        apart.perp = Top {
            bid: Some(d("32.99")),
            ask: Some(d("33.01")),
        };
        let mut after = held_now.clone();
        after.vault_value -= d("3200");
        let mut fallen = share.clone();
        fallen.look(
            after.vault_value,
            Some(after.vault_value),
            share_holdings(&after, &apart, zero()),
        );
        assert!(only_reducing(&fallen, &limits()), "{:?}", fallen.now);
        assert!(matches!(
            decide(&limits(), &fallen, zero(), &after, &apart),
            Plan::Reduce { .. }
        ));
        // Funding the short was paid meanwhile counts for it.
        let mut paid = share.clone();
        paid.look(
            after.vault_value,
            Some(after.vault_value),
            share_holdings(&after, &apart, d("400")),
        );
        assert!(!only_reducing(&paid, &limits()), "{:?}", paid.now);
    }

    /// `buffer-low` brings cash home from the account for leavers waiting on
    /// a valuation. Until the valuation pays them, that cash stays in the
    /// vault however far the carry is from its target.
    #[test]
    fn cash_brought_home_for_leavers_is_not_sent_back_out() {
        // A $25,000 vault with nothing open, and $20,000 just brought home
        // for leavers.
        let home = state("25000", "0");
        let mut leavers = Leavers::default();
        leavers.brought_home(&limits(), &home);
        let decided = |state: &AccountState, leavers: &Leavers| {
            decide(
                &limits(),
                &PerShare::new(config::DRAWDOWN_STOP, config::STOP_LIFTS_WITHIN),
                leavers.held(),
                state,
                &quotes(ABOVE_ENTRY),
            )
        };
        assert_eq!(decided(&home, &leavers), Plan::Nothing);

        // A valuation pays them, and the vault's cash falls by as much.
        leavers.seen(d("1500"));
        assert_eq!(leavers.held(), zero());
        let deposit = state("25000", "0");
        leavers.seen(deposit.idle);
        let Plan::Build { fund, .. } = decided(&deposit, &leavers) else {
            panic!("a deposit after the leavers are paid is put to work");
        };
        assert_eq!(fund.from_vault, d("20000"));
    }

    #[test]
    fn a_refusal_is_not_resent_on_every_event() {
        let at = Snapshot::of(&state("25000", "0"));
        let mut patience = Patience::default();
        let mut tried = 0;
        for _ in 0..200 {
            if !patience.holds_back(&at) {
                tried += 1;
                patience.refused(at.clone());
            }
        }
        assert!((4..=8).contains(&tried), "{tried} tries in 200 wake-ups");
    }

    #[test]
    fn prices_keep_five_figures_and_never_reach_further_than_meant() {
        assert_eq!(five_figures(d("22.51234"), Side::Buy), d("22.512"));
        assert_eq!(five_figures(d("22.51234"), Side::Sell), d("22.513"));
        assert_eq!(five_figures(d("2710.55"), Side::Buy), d("2710.5"));
        assert_eq!(five_figures(d("123456"), Side::Sell), d("123460"));
    }

    #[test]
    fn sizes_round_down_to_the_venues_decimals() {
        assert_eq!(round_down(d("1.239"), 2), d("1.23"));
        assert_eq!(round_down(d("7"), 0), d("7"));
    }
}
