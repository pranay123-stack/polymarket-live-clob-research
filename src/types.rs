//! Fixed-point primitives for prices, quantities and cash.
//!
//! Polymarket quotes binary-outcome tokens in USDC between `0.00` and `1.00`.
//! Floating point is unacceptable in an execution simulator: accumulating
//! `f64` rounding across millions of replayed events makes P&L attribution
//! non-reproducible, and the accounting identity in [`crate::portfolio`]
//! must hold *exactly*. Every monetary value in this crate is an integer.
//!
//! | Type    | Unit                | Scale  |
//! |---------|---------------------|--------|
//! | [`Price`] | dollars per share | `1e-4` |
//! | [`Qty`]   | shares            | `1e-6` |
//! | [`Usdc`]  | dollars           | `1e-6` |
//!
//! `1e-4` price resolution exactly represents both tick sizes Polymarket
//! uses (`0.01` and `0.001`). `1e-6` matches USDC's on-chain decimals.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Price ticks per dollar (`1e4`).
pub const PRICE_SCALE: u32 = 10_000;
/// Quantity units per share (`1e6`).
pub const QTY_SCALE: u64 = 1_000_000;
/// USDC units per dollar (`1e6`).
pub const USDC_SCALE: i64 = 1_000_000;

/// A price in units of `1e-4` dollars, bounded to `[0, 1]` dollars.
///
/// Binary outcome tokens settle at exactly `$0` or `$1`, so the domain is
/// closed at both ends and `u32` is ample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(pub u32);

impl Price {
    /// Worthless outcome.
    pub const ZERO: Price = Price(0);
    /// A share that has settled `true`, i.e. `$1.00`.
    pub const ONE: Price = Price(PRICE_SCALE);
    /// Highest price the exchange will accept an order at.
    pub const MAX: Price = Price(PRICE_SCALE);

    /// Parses a decimal string such as `"0.51"` as it appears on the wire.
    ///
    /// Rejects anything outside `[0, 1]` and any value requiring finer than
    /// `1e-4` resolution, rather than silently truncating it.
    pub fn parse(s: &str) -> Result<Price, ParseError> {
        let v = parse_decimal(s, PRICE_SCALE as u64)?;
        if v > PRICE_SCALE as u64 {
            return Err(ParseError::OutOfRange);
        }
        Ok(Price(v as u32))
    }

    /// Constructs from raw `1e-4` ticks, clamping to the valid domain.
    #[inline]
    pub const fn from_ticks(t: u32) -> Price {
        Price(if t > PRICE_SCALE { PRICE_SCALE } else { t })
    }

    /// Raw value in `1e-4` dollars.
    #[inline]
    pub const fn ticks(self) -> u32 {
        self.0
    }

    /// Lossy conversion, for display and reporting only — never for accounting.
    #[inline]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / PRICE_SCALE as f64
    }

    /// The complementary price `1 - p`.
    ///
    /// On a binary market the `Up` and `Down` tokens are complements, so a
    /// bid of `p` on one is economically an ask of `1 - p` on the other.
    #[inline]
    pub const fn complement(self) -> Price {
        Price(PRICE_SCALE - self.0)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}", self.to_f64())
    }
}

/// A quantity of outcome shares in units of `1e-6`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Qty(pub u64);

impl Qty {
    /// No shares.
    pub const ZERO: Qty = Qty(0);

    /// Parses a decimal share count such as `"94104.93"`.
    pub fn parse(s: &str) -> Result<Qty, ParseError> {
        Ok(Qty(parse_decimal(s, QTY_SCALE)?))
    }

    /// Constructs from a whole number of shares.
    #[inline]
    pub const fn from_shares(n: u64) -> Qty {
        Qty(n * QTY_SCALE)
    }

    /// True when the quantity is exactly zero.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Saturating subtraction — a quantity can never go negative.
    #[inline]
    pub const fn saturating_sub(self, other: Qty) -> Qty {
        Qty(self.0.saturating_sub(other.0))
    }

    /// Lossy conversion, for display and reporting only.
    #[inline]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / QTY_SCALE as f64
    }

    /// Notional value of `self` shares at `price`, rounded half-up to the
    /// nearest USDC unit.
    ///
    /// `1e-6 shares * 1e-4 dollars = 1e-10 dollars`, so the product is scaled
    /// down by `1e4` to land on USDC's `1e-6`. The intermediate is `i128`
    /// because a deep book level (`~1e5` shares) at `$1` overflows `u64` by a
    /// comfortable margin once scaled.
    #[inline]
    pub fn notional(self, price: Price) -> Usdc {
        let raw = self.0 as i128 * price.0 as i128; // 1e-10 dollars
        Usdc(((raw + 5_000) / 10_000) as i64)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.to_f64())
    }
}

/// A signed cash amount in units of `1e-6` USDC.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Usdc(pub i64);

impl Usdc {
    /// No money.
    pub const ZERO: Usdc = Usdc(0);

    /// Constructs from a whole number of dollars.
    #[inline]
    pub const fn from_dollars(d: i64) -> Usdc {
        Usdc(d * USDC_SCALE)
    }

    /// Lossy conversion, for display and reporting only.
    #[inline]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / USDC_SCALE as f64
    }

    /// Applies a basis-point fee rate, rounding half-up.
    #[inline]
    pub fn bps(self, bps: u32) -> Usdc {
        let raw = self.0 as i128 * bps as i128;
        Usdc(((raw + 5_000) / 10_000) as i64)
    }
}

impl std::ops::Add for Usdc {
    type Output = Usdc;
    #[inline]
    fn add(self, rhs: Usdc) -> Usdc {
        Usdc(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Usdc {
    type Output = Usdc;
    #[inline]
    fn sub(self, rhs: Usdc) -> Usdc {
        Usdc(self.0 - rhs.0)
    }
}

impl std::ops::Neg for Usdc {
    type Output = Usdc;
    #[inline]
    fn neg(self) -> Usdc {
        Usdc(-self.0)
    }
}

impl std::iter::Sum for Usdc {
    fn sum<I: Iterator<Item = Usdc>>(iter: I) -> Usdc {
        Usdc(iter.map(|u| u.0).sum())
    }
}

impl std::ops::AddAssign for Usdc {
    #[inline]
    fn add_assign(&mut self, rhs: Usdc) {
        self.0 += rhs.0;
    }
}

impl std::ops::SubAssign for Usdc {
    #[inline]
    fn sub_assign(&mut self, rhs: Usdc) {
        self.0 -= rhs.0;
    }
}

impl fmt::Display for Usdc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Format from the integer so the printed cents never disagree with
        // the stored value the way an f64 round-trip can.
        let neg = self.0 < 0;
        let a = self.0.unsigned_abs();
        let whole = a / USDC_SCALE as u64;
        let frac = (a % USDC_SCALE as u64) / 10_000; // 2dp
        write!(f, "{}${}.{:02}", if neg { "-" } else { "" }, whole, frac)
    }
}

/// Which side of the book a price level or order sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    /// A resting bid: willing to buy at or below this price.
    Buy,
    /// A resting ask: willing to sell at or above this price.
    Sell,
}

impl Side {
    /// The opposing side.
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// `+1` for buys, `-1` for sells — the sign a fill applies to a position.
    #[inline]
    pub const fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }

    /// Parses the exchange's `"BUY"` / `"SELL"` wire encoding.
    pub fn parse(s: &str) -> Result<Side, ParseError> {
        match s {
            "BUY" | "buy" => Ok(Side::Buy),
            "SELL" | "sell" => Ok(Side::Sell),
            _ => Err(ParseError::BadSide),
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        })
    }
}

/// Failure decoding a fixed-point value from the exchange wire format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The string was not a non-negative decimal number.
    #[error("malformed decimal")]
    Malformed,
    /// The value needs finer resolution than the target scale provides.
    #[error("more precision than the fixed-point scale can represent")]
    TooPrecise,
    /// The value fell outside the type's valid domain.
    #[error("value out of range")]
    OutOfRange,
    /// Side was neither `BUY` nor `SELL`.
    #[error("side must be BUY or SELL")]
    BadSide,
}

/// Parses a non-negative decimal string into a fixed-point integer at `scale`.
///
/// Exact by construction: the fractional digits are accumulated into the
/// scale rather than going via `f64`, and any digit finer than the scale is
/// rejected instead of being silently dropped.
fn parse_decimal(s: &str, scale: u64) -> Result<u64, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Malformed);
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // An empty integer part is legal (".5"), but it must not also be the
    // whole string, which the emptiness check above already ruled out.
    let mut value: u64 = 0;
    for b in int_part.bytes() {
        let d = (b as char).to_digit(10).ok_or(ParseError::Malformed)?;
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(d as u64))
            .ok_or(ParseError::OutOfRange)?;
    }
    value = value.checked_mul(scale).ok_or(ParseError::OutOfRange)?;

    let mut step = scale;
    for b in frac_part.bytes() {
        let d = (b as char).to_digit(10).ok_or(ParseError::Malformed)?;
        step /= 10;
        if step == 0 {
            // Beyond our resolution: tolerate trailing zeros, reject real digits.
            if d != 0 {
                return Err(ParseError::TooPrecise);
            }
            continue;
        }
        value = value
            .checked_add(d as u64 * step)
            .ok_or(ParseError::OutOfRange)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices_seen_on_the_wire() {
        assert_eq!(Price::parse("0.51").unwrap(), Price(5_100));
        assert_eq!(Price::parse("0.2").unwrap(), Price(2_000));
        assert_eq!(Price::parse("0.001").unwrap(), Price(10));
        assert_eq!(Price::parse("1").unwrap(), Price::ONE);
        assert_eq!(Price::parse("0").unwrap(), Price::ZERO);
    }

    #[test]
    fn rejects_prices_outside_the_binary_domain() {
        assert_eq!(Price::parse("1.5"), Err(ParseError::OutOfRange));
        assert_eq!(Price::parse("abc"), Err(ParseError::Malformed));
        assert_eq!(Price::parse("0.00001"), Err(ParseError::TooPrecise));
        // Trailing zeros carry no information and must round-trip cleanly.
        assert_eq!(Price::parse("0.510000").unwrap(), Price(5_100));
    }

    #[test]
    fn parses_fractional_share_counts() {
        assert_eq!(Qty::parse("94104.93").unwrap(), Qty(94_104_930_000));
        assert_eq!(Qty::parse("60").unwrap(), Qty(60_000_000));
        assert_eq!(Qty::parse("0.5").unwrap(), Qty(500_000));
    }

    #[test]
    fn notional_rounds_half_up_to_usdc_units() {
        // 20 shares at $0.51 is exactly $10.20.
        assert_eq!(
            Qty::from_shares(20).notional(Price(5_100)),
            Usdc(10_200_000)
        );
        // A whole book level: 94104.93 shares at $0.01 is $941.0493.
        assert_eq!(
            Qty::parse("94104.93").unwrap().notional(Price(100)),
            Usdc(941_049_300)
        );
        // Half-up at the sub-USDC boundary: one quantity unit at $0.50 is
        // exactly half a USDC unit and must round up, while $0.4999 rounds down.
        assert_eq!(Qty(1).notional(Price(5_000)), Usdc(1));
        assert_eq!(Qty(1).notional(Price(4_999)), Usdc(0));
    }

    #[test]
    fn complement_prices_sum_to_one_dollar() {
        let p = Price::parse("0.51").unwrap();
        assert_eq!(p.complement(), Price::parse("0.49").unwrap());
        assert_eq!(p.ticks() + p.complement().ticks(), PRICE_SCALE);
    }

    #[test]
    fn usdc_displays_from_the_integer_not_a_float() {
        assert_eq!(Usdc(10_200_000).to_string(), "$10.20");
        assert_eq!(Usdc(-1_005_000).to_string(), "-$1.00");
        assert_eq!(Usdc::ZERO.to_string(), "$0.00");
    }
}
