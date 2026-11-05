//! Who should play whom: the pure rules of matchmaking.
//!
//! Players waiting for a game with the same time control and the same rated/casual choice are in one *bucket*. Within a
//! bucket, two players may be paired when their ratings are close enough. "Close enough" starts narrow and widens the
//! longer a player waits, so a strong or weak player eventually gets a game but nobody is matched with a wildly
//! different opponent straight away. Both players' windows must allow the pairing (the narrower one decides), so a
//! patient player is never forced into a mismatch by someone who has only just arrived.

/// How far apart ratings may be for someone who has just joined.
pub const START_WINDOW: f64 = 100.0;
/// How much the window grows for every second of waiting.
pub const WIDEN_PER_SECOND: f64 = 25.0;
/// The window never grows beyond this.
pub const MAX_WINDOW: f64 = 800.0;

/// The widest rating gap this player will accept after waiting `waited_ms`.
pub fn window(waited_ms: u64) -> f64 {
    (START_WINDOW + WIDEN_PER_SECOND * waited_ms as f64 / 1000.0).min(MAX_WINDOW)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Waiting {
    /// Any identifier the caller likes (an index, a user id): it is handed back in the pairs.
    pub id: u64,
    pub rating: f64,
    pub waited_ms: u64,
}

/// Pairs up as many players as possible. The longest-waiting player is served first and gets the closest-rated
/// opponent that both windows allow. The same input always gives the same pairs. Returns pairs of `id`s.
pub fn pair(waiting: &[Waiting]) -> Vec<(u64, u64)> {
    let mut order: Vec<&Waiting> = waiting.iter().collect();
    // Longest wait first; ties broken by id so the result never depends on input order.
    order.sort_by(|a, b| b.waited_ms.cmp(&a.waited_ms).then(a.id.cmp(&b.id)));
    let mut taken = vec![false; order.len()];
    let mut pairs = Vec::new();
    for i in 0..order.len() {
        if taken[i] {
            continue;
        }
        let a = order[i];
        let best = (i + 1..order.len())
            .filter(|&j| !taken[j])
            .filter(|&j| {
                let b = order[j];
                (a.rating - b.rating).abs() <= window(a.waited_ms).min(window(b.waited_ms))
            })
            .min_by(|&x, &y| {
                let (dx, dy) = (
                    (a.rating - order[x].rating).abs(),
                    (a.rating - order[y].rating).abs(),
                );
                dx.total_cmp(&dy).then(order[x].id.cmp(&order[y].id))
            });
        if let Some(j) = best {
            taken[i] = true;
            taken[j] = true;
            pairs.push((a.id, order[j].id));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn w(id: u64, rating: f64, waited_ms: u64) -> Waiting {
        Waiting {
            id,
            rating,
            waited_ms,
        }
    }

    #[test]
    fn the_window_starts_narrow_widens_and_stops() {
        assert_eq!(window(0), 100.0);
        assert_eq!(window(4_000), 200.0);
        assert_eq!(window(10_000_000), MAX_WINDOW);
    }

    #[test]
    fn close_players_are_paired_at_once_and_far_ones_are_not() {
        assert_eq!(pair(&[w(1, 1500.0, 0), w(2, 1560.0, 0)]), [(1, 2)]);
        assert!(pair(&[w(1, 1500.0, 0), w(2, 1700.0, 0)]).is_empty());
        assert!(pair(&[w(1, 1500.0, 0)]).is_empty());
        assert!(pair(&[]).is_empty());
    }

    #[test]
    fn waiting_widens_the_window_but_both_sides_must_agree() {
        // 300 apart: a patient player (waited 10 s, window 350) alone is not enough, as the other has just arrived.
        assert!(pair(&[w(1, 1500.0, 10_000), w(2, 1800.0, 0)]).is_empty());
        // When both have waited, it works.
        assert_eq!(
            pair(&[w(1, 1500.0, 10_000), w(2, 1800.0, 10_000)]),
            [(1, 2)]
        );
    }

    #[test]
    fn the_longest_waiter_is_served_first_with_the_closest_opponent() {
        // 1 has waited longest; both 2 (1520) and 3 (1590) are acceptable and 2 is closer.
        let pairs = pair(&[w(3, 1590.0, 0), w(2, 1520.0, 0), w(1, 1500.0, 5_000)]);
        assert_eq!(pairs, [(1, 2)]);
    }

    #[test]
    fn a_crowd_is_paired_off_without_reusing_anyone() {
        let crowd: Vec<Waiting> = (0..10).map(|i| w(i, 1500.0 + i as f64 * 5.0, 0)).collect();
        let pairs = pair(&crowd);
        assert_eq!(pairs.len(), 5);
        let mut seen: Vec<u64> = pairs.iter().flat_map(|&(a, b)| [a, b]).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 10);
    }

    proptest! {
        #[test]
        fn pairs_are_valid_and_independent_of_input_order(
            raw in prop::collection::vec((600.0f64..2800.0, 0u64..60_000), 0..14),
        ) {
            let list: Vec<Waiting> = raw.iter().enumerate().map(|(i, &(r, t))| w(i as u64, r, t)).collect();
            let pairs = pair(&list);
            let mut used = std::collections::HashSet::new();
            for &(a, b) in &pairs {
                prop_assert!(a != b && used.insert(a) && used.insert(b), "someone is paired twice");
                let (pa, pb) = (&list[a as usize], &list[b as usize]);
                prop_assert!((pa.rating - pb.rating).abs() <= window(pa.waited_ms).min(window(pb.waited_ms)) + 1e-9);
            }
            let mut reversed = list.clone();
            reversed.reverse();
            prop_assert_eq!(pair(&reversed), pairs);
        }
    }
}
