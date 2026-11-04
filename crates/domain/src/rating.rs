//! Glicko-2 ratings (Glickman, "Example of the Glicko-2 system", 2013), as pure functions.
//!
//! A rating has three parts: the **rating** itself (1500 for a newcomer), the **deviation** (how unsure we are of it:
//! 350 for a newcomer, shrinking as they play) and the **volatility** (how erratic their results are). Ratings are
//! updated once per *rating period*, using every game played in it; the caller decides what a period is.
//!
//! Floating point is fine here, unlike for money: a rating is an estimate, not a balance. The functions are
//! deterministic, so the same games always give the same numbers.

/// The scale factor between the familiar Glicko scale and the internal Glicko-2 scale.
const SCALE: f64 = 173.7178;
const DEFAULT_RATING: f64 = 1500.0;
/// A rating can never be more certain than this, so a long-time player can still move.
pub const MIN_DEVIATION: f64 = 30.0;
pub const MAX_DEVIATION: f64 = 350.0;
const CONVERGENCE: f64 = 1e-6;

/// How strongly volatility may change per period (Glickman suggests 0.3 to 1.2; smaller is steadier).
pub const DEFAULT_TAU: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rating {
    pub rating: f64,
    pub deviation: f64,
    pub volatility: f64,
}

impl Rating {
    /// What every new player starts with.
    pub const NEW: Rating = Rating {
        rating: DEFAULT_RATING,
        deviation: MAX_DEVIATION,
        volatility: 0.06,
    };
}

/// One game from the rated player's point of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Game {
    /// The opponent's rating when the period began.
    pub opponent: Rating,
    /// 1.0 for a win, 0.5 for a draw, 0.0 for a loss.
    pub score: f64,
}

fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi * phi / (std::f64::consts::PI * std::f64::consts::PI)).sqrt()
}

fn expected(mu: f64, mu_j: f64, phi_j: f64) -> f64 {
    1.0 / (1.0 + (-g(phi_j) * (mu - mu_j)).exp())
}

/// The chance (0 to 1) that `a` scores against `b`, counting a draw as half.
pub fn expected_score(a: &Rating, b: &Rating) -> f64 {
    expected(
        (a.rating - DEFAULT_RATING) / SCALE,
        (b.rating - DEFAULT_RATING) / SCALE,
        b.deviation / SCALE,
    )
}

/// The rating after one period in which the player played `games`. With no games, only the deviation grows
/// (we become less sure of someone who has not played).
pub fn update(player: &Rating, games: &[Game], tau: f64) -> Rating {
    let mu = (player.rating - DEFAULT_RATING) / SCALE;
    let phi = player.deviation / SCALE;
    let sigma = player.volatility;

    if games.is_empty() {
        let phi_star = (phi * phi + sigma * sigma).sqrt();
        return Rating {
            deviation: (phi_star * SCALE).min(MAX_DEVIATION),
            ..*player
        };
    }

    let mut v_inv = 0.0;
    let mut delta_sum = 0.0;
    for game in games {
        let mu_j = (game.opponent.rating - DEFAULT_RATING) / SCALE;
        let phi_j = game.opponent.deviation / SCALE;
        let e = expected(mu, mu_j, phi_j);
        v_inv += g(phi_j).powi(2) * e * (1.0 - e);
        delta_sum += g(phi_j) * (game.score - e);
    }
    let v = 1.0 / v_inv;
    let delta = v * delta_sum;

    let new_sigma = new_volatility(phi, sigma, v, delta, tau);

    let phi_star = (phi * phi + new_sigma * new_sigma).sqrt();
    let new_phi = 1.0 / (1.0 / (phi_star * phi_star) + 1.0 / v).sqrt();
    let new_mu = mu + new_phi * new_phi * delta_sum;

    Rating {
        rating: new_mu * SCALE + DEFAULT_RATING,
        deviation: (new_phi * SCALE).clamp(MIN_DEVIATION, MAX_DEVIATION),
        volatility: new_sigma,
    }
}

/// Step 5 of the paper: solve for the new volatility with the Illinois (regula falsi) method.
fn new_volatility(phi: f64, sigma: f64, v: f64, delta: f64, tau: f64) -> f64 {
    let a = (sigma * sigma).ln();
    let f = |x: f64| {
        let ex = x.exp();
        let d2 = phi * phi + v + ex;
        ex * (delta * delta - phi * phi - v - ex) / (2.0 * d2 * d2) - (x - a) / (tau * tau)
    };

    let mut big_a = a;
    let mut big_b = if delta * delta > phi * phi + v {
        (delta * delta - phi * phi - v).ln()
    } else {
        let mut k = 1.0;
        while f(a - k * tau) < 0.0 {
            k += 1.0;
        }
        a - k * tau
    };
    let (mut f_a, mut f_b) = (f(big_a), f(big_b));
    // The loop converges in a handful of steps; the cap is only a guard against a pathological input.
    for _ in 0..100 {
        if (big_b - big_a).abs() <= CONVERGENCE {
            break;
        }
        let c = big_a + (big_a - big_b) * f_a / (f_b - f_a);
        let f_c = f(c);
        if f_c * f_b <= 0.0 {
            big_a = big_b;
            f_a = f_b;
        } else {
            f_a /= 2.0;
        }
        big_b = c;
        f_b = f_c;
    }
    (big_a / 2.0).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn r(rating: f64, deviation: f64) -> Rating {
        Rating {
            rating,
            deviation,
            volatility: 0.06,
        }
    }

    /// The worked example in Glickman's paper: 1500 / 200 / 0.06, tau 0.5, one win and two losses.
    #[test]
    fn matches_the_published_worked_example() {
        let games = [
            Game {
                opponent: r(1400.0, 30.0),
                score: 1.0,
            },
            Game {
                opponent: r(1550.0, 100.0),
                score: 0.0,
            },
            Game {
                opponent: r(1700.0, 300.0),
                score: 0.0,
            },
        ];
        let out = update(&r(1500.0, 200.0), &games, 0.5);
        assert!((out.rating - 1464.06).abs() < 0.01, "{}", out.rating);
        assert!((out.deviation - 151.52).abs() < 0.01, "{}", out.deviation);
        assert!(
            (out.volatility - 0.05999).abs() < 0.00001,
            "{}",
            out.volatility
        );
    }

    #[test]
    fn a_period_without_games_only_raises_the_deviation() {
        let p = r(1700.0, 100.0);
        let out = update(&p, &[], DEFAULT_TAU);
        assert_eq!((out.rating, out.volatility), (1700.0, 0.06));
        assert!(out.deviation > 100.0 && out.deviation < 101.0);
        // ...and never past the ceiling.
        assert_eq!(
            update(&Rating::NEW, &[], DEFAULT_TAU).deviation,
            MAX_DEVIATION
        );
    }

    #[test]
    fn expected_scores_are_sensible() {
        let (a, b) = (r(1500.0, 50.0), r(1500.0, 50.0));
        assert!((expected_score(&a, &b) - 0.5).abs() < 1e-12);
        let strong = r(1900.0, 50.0);
        assert!(expected_score(&strong, &a) > 0.9);
        assert!((expected_score(&strong, &a) + expected_score(&a, &strong) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn beating_a_stronger_player_gains_more_than_beating_a_weaker_one() {
        let me = r(1500.0, 80.0);
        let gain = |opp: f64| {
            update(
                &me,
                &[Game {
                    opponent: r(opp, 80.0),
                    score: 1.0,
                }],
                DEFAULT_TAU,
            )
            .rating
                - 1500.0
        };
        assert!(gain(1800.0) > gain(1500.0) && gain(1500.0) > gain(1200.0) && gain(1200.0) > 0.0);
    }

    #[test]
    fn playing_makes_us_surer_and_a_newcomer_moves_fast() {
        let games = [Game {
            opponent: r(1500.0, 50.0),
            score: 1.0,
        }];
        let out = update(&Rating::NEW, &games, DEFAULT_TAU);
        assert!(out.deviation < Rating::NEW.deviation);
        assert!(
            out.rating > 1600.0,
            "a first win moves a newcomer a lot: {}",
            out.rating
        );
        let settled = update(&r(1500.0, 40.0), &games, DEFAULT_TAU);
        assert!(settled.rating - 1500.0 < out.rating - 1500.0);
    }

    proptest! {
        #[test]
        fn results_always_stay_finite_and_in_bounds(
            rating in 600.0f64..2800.0, dev in 30.0f64..350.0, vol in 0.02f64..0.2,
            games in prop::collection::vec((600.0f64..2800.0, 30.0f64..350.0, 0u8..3), 0..12),
        ) {
            let p = Rating { rating, deviation: dev, volatility: vol };
            let games: Vec<Game> = games.into_iter()
                .map(|(o, d, s)| Game { opponent: r(o, d), score: s as f64 / 2.0 }).collect();
            let out = update(&p, &games, DEFAULT_TAU);
            prop_assert!(out.rating.is_finite() && out.volatility.is_finite() && out.volatility > 0.0);
            prop_assert!(out.deviation >= MIN_DEVIATION && out.deviation <= MAX_DEVIATION);
        }

        #[test]
        fn a_win_never_lowers_and_a_loss_never_raises_the_rating(
            rating in 800.0f64..2400.0, dev in 30.0f64..350.0, opp in 800.0f64..2400.0, opp_dev in 30.0f64..350.0,
        ) {
            let p = r(rating, dev);
            let win = update(&p, &[Game { opponent: r(opp, opp_dev), score: 1.0 }], DEFAULT_TAU);
            let loss = update(&p, &[Game { opponent: r(opp, opp_dev), score: 0.0 }], DEFAULT_TAU);
            prop_assert!(win.rating >= rating && loss.rating <= rating);
        }
    }
}
