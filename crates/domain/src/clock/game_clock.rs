use std::fmt;

use super::control::TimeControl;
use crate::chess::Color;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    InvalidTimeControl(&'static str),
    InvalidLagPolicy(&'static str),
    /// A move arrived stamped earlier than the turn began: the caller's timestamps are inconsistent.
    TimeWentBackwards,
    /// Stored clock values that cannot be right (used when rebuilding a clock from storage).
    InvalidState(&'static str),
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClockError::InvalidTimeControl(why)
            | ClockError::InvalidLagPolicy(why)
            | ClockError::InvalidState(why) => write!(f, "{why}"),
            ClockError::TimeWentBackwards => {
                write!(f, "a move cannot arrive before its turn started")
            }
        }
    }
}

impl std::error::Error for ClockError {}

/// How much of a player's network lag the server refunds, so a slow connection does not cost them the game.
///
/// Three limits keep it from being abused (a player who claims huge lag on every move gains only a little):
/// each move's refund is at most `max_credit_per_move_ms`; the refunds draw on a per-player quota of at most
/// `quota_max_ms`; and the quota refills by `quota_regen_per_move_ms` after each move. A refund also never exceeds the
/// time actually spent on the move, and the lag itself is measured by the server, never reported by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagPolicy {
    pub max_credit_per_move_ms: u64,
    pub quota_max_ms: u64,
    pub quota_regen_per_move_ms: u64,
}

impl Default for LagPolicy {
    /// Starting values, to be set against the product requirements: up to 1 s refunded per move, from a quota of
    /// 2 s that refills 0.1 s per move.
    fn default() -> Self {
        LagPolicy {
            max_credit_per_move_ms: 1_000,
            quota_max_ms: 2_000,
            quota_regen_per_move_ms: 100,
        }
    }
}

impl LagPolicy {
    pub fn new(
        max_credit_per_move_ms: u64,
        quota_max_ms: u64,
        quota_regen_per_move_ms: u64,
    ) -> Result<LagPolicy, ClockError> {
        if max_credit_per_move_ms > 5_000
            || quota_max_ms > 10_000
            || quota_regen_per_move_ms > max_credit_per_move_ms
        {
            return Err(ClockError::InvalidLagPolicy("credit per move at most 5 s, quota at most 10 s, refill no more than one move's credit"));
        }
        Ok(LagPolicy {
            max_credit_per_move_ms,
            quota_max_ms,
            quota_regen_per_move_ms,
        })
    }
}

/// Everything needed to rebuild a clock. Store this (or the parts of it) with each move, and a restarted server can
/// carry on from the last move with no loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockState {
    pub control: TimeControl,
    pub policy: LagPolicy,
    /// Time each player had banked when the current turn began: index 0 is White, 1 is Black.
    pub remaining_ms: [u64; 2],
    /// Lag-credit quota each player has left.
    pub lag_quota_ms: [u64; 2],
    pub turn: Color,
    /// When the current turn began, in the caller's milliseconds.
    pub turn_started_at_ms: u64,
}

fn idx(color: Color) -> usize {
    match color {
        Color::White => 0,
        Color::Black => 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    state: ClockState,
}

/// The effect of one move on the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveTiming {
    /// The clock after the move: the opponent's turn has begun.
    pub clock: Clock,
    /// Time taken off the mover's clock (the time spent, less the lag refund).
    pub charged_ms: u64,
    /// Lag refunded.
    pub credited_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timed {
    Made(MoveTiming),
    /// The mover's time had already run out when the move arrived, even after any lag refund: they lose on time.
    Flagged {
        color: Color,
    },
}

impl Clock {
    /// Starts the clock: White to move, both players with the full base time and a full lag quota.
    pub fn start(control: TimeControl, policy: LagPolicy, now_ms: u64) -> Clock {
        Clock {
            state: ClockState {
                control,
                policy,
                remaining_ms: [control.base_ms(); 2],
                lag_quota_ms: [policy.quota_max_ms; 2],
                turn: Color::White,
                turn_started_at_ms: now_ms,
            },
        }
    }

    /// Rebuilds a clock from stored values, refusing ones that cannot have come from a real game.
    pub fn from_state(state: ClockState) -> Result<Clock, ClockError> {
        let ceiling = state
            .control
            .base_ms()
            .saturating_add(state.control.increment_ms().saturating_mul(1_000));
        if state.remaining_ms.iter().any(|r| *r == 0 || *r > ceiling) {
            return Err(ClockError::InvalidState(
                "a stored time is zero or impossibly large",
            ));
        }
        if state
            .lag_quota_ms
            .iter()
            .any(|q| *q > state.policy.quota_max_ms)
        {
            return Err(ClockError::InvalidState(
                "a stored lag quota is above the maximum",
            ));
        }
        Ok(Clock { state })
    }

    pub fn state(&self) -> ClockState {
        self.state
    }

    pub fn turn(&self) -> Color {
        self.state.turn
    }

    /// What `color` had banked when the current turn began.
    pub fn banked_ms(&self, color: Color) -> u64 {
        self.state.remaining_ms[idx(color)]
    }

    pub fn lag_quota_ms(&self, color: Color) -> u64 {
        self.state.lag_quota_ms[idx(color)]
    }

    /// The time `color` has left at `now_ms`, for showing on a screen: the player to move is running down; the
    /// other is not. No lag refund is applied here, since it is only worked out when a move arrives.
    pub fn time_left_ms(&self, color: Color, now_ms: u64) -> u64 {
        let banked = self.banked_ms(color);
        if color == self.state.turn {
            banked.saturating_sub(now_ms.saturating_sub(self.state.turn_started_at_ms))
        } else {
            banked
        }
    }

    /// The moment at which the player to move may be declared out of time. It is later than "banked time runs out"
    /// by the lag refund they could still be owed, because a move sent in time may simply still be on its way.
    pub fn flag_deadline_ms(&self) -> u64 {
        let s = &self.state;
        let grace = s
            .policy
            .max_credit_per_move_ms
            .min(s.lag_quota_ms[idx(s.turn)]);
        s.turn_started_at_ms
            .saturating_add(s.remaining_ms[idx(s.turn)])
            .saturating_add(grace)
    }

    /// For a timer: the player to move, if they are now out of time and no move of theirs can still count.
    pub fn flagged_at(&self, now_ms: u64) -> Option<Color> {
        (now_ms >= self.flag_deadline_ms()).then_some(self.state.turn)
    }

    /// Records that the player to move made their move, arriving at `received_at_ms`, with `measured_lag_ms` the
    /// lag the server measured for them. Returns the new clock, or says the mover ran out of time first.
    pub fn record_move(
        &self,
        received_at_ms: u64,
        measured_lag_ms: u64,
    ) -> Result<Timed, ClockError> {
        let s = &self.state;
        let elapsed = received_at_ms
            .checked_sub(s.turn_started_at_ms)
            .ok_or(ClockError::TimeWentBackwards)?;
        let i = idx(s.turn);
        let banked = s.remaining_ms[i];

        // The refund is the smallest of: the measured lag, the per-move cap, what is left of the quota, and the time
        // actually spent (nobody is refunded more than they used).
        let credited = measured_lag_ms
            .min(s.policy.max_credit_per_move_ms)
            .min(s.lag_quota_ms[i])
            .min(elapsed);
        let charged = elapsed - credited;
        if charged >= banked {
            return Ok(Timed::Flagged { color: s.turn });
        }

        let mut next = self.state;
        next.remaining_ms[i] = banked - charged + s.control.increment_ms();
        next.lag_quota_ms[i] = (s.lag_quota_ms[i] - credited + s.policy.quota_regen_per_move_ms)
            .min(s.policy.quota_max_ms);
        next.turn = s.turn.opposite();
        next.turn_started_at_ms = received_at_ms;
        Ok(Timed::Made(MoveTiming {
            clock: Clock { state: next },
            charged_ms: charged,
            credited_ms: credited,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn clock(base: u64, inc: u64) -> Clock {
        Clock::start(
            TimeControl::new(base, inc).unwrap(),
            LagPolicy::default(),
            0,
        )
    }

    fn made(t: Timed) -> MoveTiming {
        match t {
            Timed::Made(m) => m,
            Timed::Flagged { color } => panic!("{color:?} was flagged"),
        }
    }

    #[test]
    fn a_new_clock_gives_everyone_the_full_time_and_white_the_move() {
        let c = clock(300_000, 2_000);
        assert_eq!(
            (
                c.turn(),
                c.banked_ms(Color::White),
                c.banked_ms(Color::Black)
            ),
            (Color::White, 300_000, 300_000)
        );
        assert_eq!(c.lag_quota_ms(Color::White), 2_000);
    }

    #[test]
    fn a_move_costs_the_time_spent_and_earns_the_increment() {
        let c = clock(300_000, 2_000);
        let m = made(c.record_move(5_000, 0).unwrap());
        assert_eq!((m.charged_ms, m.credited_ms), (5_000, 0));
        assert_eq!(m.clock.banked_ms(Color::White), 300_000 - 5_000 + 2_000);
        assert_eq!(
            m.clock.banked_ms(Color::Black),
            300_000,
            "the opponent's time is untouched"
        );
        assert_eq!(m.clock.turn(), Color::Black);
        // Black replies 3 seconds later.
        let m2 = made(m.clock.record_move(8_000, 0).unwrap());
        assert_eq!(m2.clock.banked_ms(Color::Black), 300_000 - 3_000 + 2_000);
        assert_eq!(m2.clock.turn(), Color::White);
    }

    #[test]
    fn an_instant_move_is_fine_and_still_earns_the_increment() {
        let m = made(clock(60_000, 1_000).record_move(0, 0).unwrap());
        assert_eq!((m.charged_ms, m.clock.banked_ms(Color::White)), (0, 61_000));
    }

    #[test]
    fn running_out_of_time_loses_exactly_at_zero() {
        let c = clock(10_000, 0);
        assert!(
            matches!(c.record_move(9_999, 0).unwrap(), Timed::Made(_)),
            "1 ms to spare"
        );
        assert_eq!(
            c.record_move(10_000, 0).unwrap(),
            Timed::Flagged {
                color: Color::White
            },
            "exactly zero is out of time"
        );
        assert_eq!(
            c.record_move(60_000, 0).unwrap(),
            Timed::Flagged {
                color: Color::White
            }
        );
    }

    #[test]
    fn the_increment_is_not_paid_on_the_move_that_runs_out_of_time() {
        let c = clock(10_000, 5_000);
        assert_eq!(
            c.record_move(10_000, 0).unwrap(),
            Timed::Flagged {
                color: Color::White
            }
        );
    }

    #[test]
    fn lag_is_refunded_up_to_the_limits() {
        let c = clock(300_000, 0);
        // Lag below the cap is refunded in full.
        let m = made(c.record_move(5_000, 300).unwrap());
        assert_eq!((m.charged_ms, m.credited_ms), (4_700, 300));
        assert_eq!(m.clock.lag_quota_ms(Color::White), 2_000 - 300 + 100);
        // Lag above the per-move cap is refunded only up to the cap.
        let m = made(c.record_move(5_000, 9_000).unwrap());
        assert_eq!((m.charged_ms, m.credited_ms), (4_000, 1_000));
        // Nobody is refunded more than the time they used.
        let m = made(c.record_move(200, 900).unwrap());
        assert_eq!((m.charged_ms, m.credited_ms), (0, 200));
    }

    #[test]
    fn a_player_who_claims_lag_on_every_move_runs_out_of_quota() {
        let mut c = clock(600_000, 0);
        let mut now = 0;
        let mut total_credit = 0;
        for _ in 0..20 {
            now += 2_000;
            let white = made(c.record_move(now, 5_000).unwrap()); // White always claims lots of lag
            total_credit += white.credited_ms;
            now += 2_000;
            c = made(white.clock.record_move(now, 0).unwrap()).clock; // Black never does
        }
        // Claiming 5 s of lag on each of 20 moves could have earned 20 s. But the quota is conserved: what was
        // refunded plus what is left equals the starting quota plus the 20 slow refills.
        assert_eq!(
            total_credit + c.lag_quota_ms(Color::White),
            2_000 + 20 * 100
        );
        assert!(
            total_credit < 5_000 * 20 / 4,
            "far less than the 20 s claimed: {total_credit}"
        );
        assert!(
            c.lag_quota_ms(Color::White) <= 200,
            "the quota is nearly spent"
        );
        // Once the quota is spent, a claim of lag buys only the refill amount.
        let m = made(c.record_move(now + 2_000, 5_000).unwrap());
        assert!(m.credited_ms <= 200, "{}", m.credited_ms);
    }

    #[test]
    fn the_quota_refills_but_never_beyond_its_maximum() {
        let mut c = clock(600_000, 0);
        let mut now = 0;
        for _ in 0..50 {
            now += 1_000;
            c = made(c.record_move(now, 0).unwrap()).clock;
            now += 1_000;
            c = made(c.record_move(now, 0).unwrap()).clock;
        }
        assert_eq!(c.lag_quota_ms(Color::White), 2_000, "capped at the maximum");
    }

    #[test]
    fn the_time_left_runs_down_only_for_the_player_to_move() {
        let c = clock(300_000, 0);
        assert_eq!(c.time_left_ms(Color::White, 0), 300_000);
        assert_eq!(c.time_left_ms(Color::White, 12_345), 300_000 - 12_345);
        assert_eq!(
            c.time_left_ms(Color::Black, 12_345),
            300_000,
            "Black's clock is not running"
        );
        assert_eq!(
            c.time_left_ms(Color::White, 1_000_000),
            0,
            "never below zero"
        );
    }

    #[test]
    fn a_timer_only_calls_the_flag_after_the_lag_grace_has_passed() {
        let c = clock(10_000, 0);
        assert_eq!(
            c.flag_deadline_ms(),
            10_000 + 1_000,
            "banked time plus the most that could still be refunded"
        );
        assert_eq!(
            c.flagged_at(10_999),
            None,
            "a move sent in time may still be on its way"
        );
        assert_eq!(c.flagged_at(11_000), Some(Color::White));
        // The deadline agrees with what record_move would decide for a move arriving then, with the maximum lag.
        assert!(matches!(
            c.record_move(10_999, 5_000).unwrap(),
            Timed::Made(_)
        ));
        assert_eq!(
            c.record_move(11_000, 5_000).unwrap(),
            Timed::Flagged {
                color: Color::White
            }
        );
        // With the quota used up there is no grace.
        let mut low = c.state();
        low.lag_quota_ms[0] = 0;
        assert_eq!(Clock::from_state(low).unwrap().flag_deadline_ms(), 10_000);
    }

    #[test]
    fn a_move_stamped_before_its_turn_began_is_a_bug_in_the_caller() {
        let c = Clock::start(
            TimeControl::new(60_000, 0).unwrap(),
            LagPolicy::default(),
            5_000,
        );
        assert_eq!(c.record_move(4_999, 0), Err(ClockError::TimeWentBackwards));
    }

    #[test]
    fn a_clock_can_be_stored_and_rebuilt_exactly() {
        let mut c = clock(300_000, 3_000);
        for (i, t) in [4_000u64, 9_500, 21_000, 22_000, 31_000]
            .into_iter()
            .enumerate()
        {
            c = made(c.record_move(t, (i as u64) * 150).unwrap()).clock;
        }
        let rebuilt = Clock::from_state(c.state()).unwrap();
        assert_eq!(rebuilt, c);
        // ...and then behaves identically.
        assert_eq!(
            rebuilt.record_move(40_000, 200).unwrap(),
            c.record_move(40_000, 200).unwrap()
        );
    }

    #[test]
    fn nonsense_stored_state_is_refused() {
        let good = clock(60_000, 1_000).state();
        let mut zero = good;
        zero.remaining_ms[1] = 0;
        let mut huge = good;
        huge.remaining_ms[0] = u64::MAX;
        let mut quota = good;
        quota.lag_quota_ms[0] = 999_999;
        for bad in [zero, huge, quota] {
            assert!(matches!(
                Clock::from_state(bad),
                Err(ClockError::InvalidState(_))
            ));
        }
    }

    #[test]
    fn lag_policies_are_validated() {
        assert!(LagPolicy::new(1_000, 2_000, 100).is_ok());
        assert!(LagPolicy::new(5_001, 2_000, 100).is_err());
        assert!(LagPolicy::new(1_000, 10_001, 100).is_err());
        assert!(LagPolicy::new(1_000, 2_000, 1_001).is_err());
    }

    proptest! {
        /// Every move's books balance: the time spent is exactly what was charged plus what was refunded.
        #[test]
        fn time_spent_equals_charged_plus_credited(base in 1_000u64..600_000, inc in 0u64..30_000, elapsed in 0u64..1_000_000, lag in 0u64..10_000) {
            let c = clock(base, inc);
            if let Timed::Made(m) = c.record_move(elapsed, lag).unwrap() {
                prop_assert_eq!(m.charged_ms + m.credited_ms, elapsed);
                prop_assert_eq!(m.clock.banked_ms(Color::White), base - m.charged_ms + inc);
            }
        }

        /// The refund never exceeds any of its limits, and the quota stays within bounds.
        #[test]
        fn refunds_respect_every_limit(elapsed in 0u64..100_000, lag in 0u64..100_000, quota in 0u64..=2_000) {
            let mut s = clock(600_000, 0).state();
            s.lag_quota_ms[0] = quota;
            let c = Clock::from_state(s).unwrap();
            if let Timed::Made(m) = c.record_move(elapsed, lag).unwrap() {
                prop_assert!(m.credited_ms <= lag && m.credited_ms <= 1_000 && m.credited_ms <= quota && m.credited_ms <= elapsed);
                prop_assert!(m.clock.lag_quota_ms(Color::White) <= 2_000);
                prop_assert_eq!(m.clock.lag_quota_ms(Color::White), (quota - m.credited_ms + 100).min(2_000));
            }
        }

        /// More lag never costs the player more time.
        #[test]
        fn more_lag_never_hurts(elapsed in 0u64..100_000, lag in 0u64..5_000, extra in 0u64..5_000) {
            let c = clock(600_000, 0);
            let (a, b) = (c.record_move(elapsed, lag).unwrap(), c.record_move(elapsed, lag + extra).unwrap());
            if let (Timed::Made(a), Timed::Made(b)) = (a, b) {
                prop_assert!(b.charged_ms <= a.charged_ms);
            }
        }

        /// However many moves are claimed to be lagged, a player's total refund is bounded by quota_max plus the
        /// refills, so lag claims cannot be used to build up extra time.
        #[test]
        fn total_refunds_are_bounded_over_a_whole_game(thinks in proptest::collection::vec(0u64..3_000, 1..60), lags in proptest::collection::vec(0u64..10_000, 60)) {
            let mut c = clock(3_600_000, 0);
            let (mut now, mut total, mut white_moves) = (0u64, 0u64, 0u64);
            for (i, think) in thinks.iter().enumerate() {
                now += think;
                let mover = c.turn();
                match c.record_move(now, lags[i]).unwrap() {
                    Timed::Made(m) => {
                        if mover == Color::White { total += m.credited_ms; white_moves += 1; }
                        c = m.clock;
                    }
                    Timed::Flagged { .. } => break,
                }
            }
            prop_assert!(total <= 2_000 + 100 * white_moves, "{total} credited over {white_moves} moves");
        }

        /// The derived time left never rises, and is zero or less than the banked time once the turn is running.
        #[test]
        fn time_left_only_goes_down(base in 1_000u64..600_000, t1 in 0u64..700_000, t2 in 0u64..700_000) {
            let c = clock(base, 0);
            let (early, late) = (t1.min(t2), t1.max(t2));
            prop_assert!(c.time_left_ms(Color::White, late) <= c.time_left_ms(Color::White, early));
            prop_assert!(c.time_left_ms(Color::White, early) <= base);
        }

        /// The timer's deadline and the move check never disagree about the same instant (with maximum lag claimed).
        #[test]
        fn the_flag_deadline_matches_what_a_move_at_that_instant_would_get(base in 1_000u64..100_000, offset in 0u64..2_000) {
            let c = clock(base, 0);
            let deadline = c.flag_deadline_ms();
            let at = deadline.saturating_sub(offset);
            let flagged_by_timer = c.flagged_at(at).is_some();
            let flagged_by_move = matches!(c.record_move(at, u64::MAX).unwrap(), Timed::Flagged { .. });
            prop_assert_eq!(flagged_by_timer, flagged_by_move);
        }
    }
}
