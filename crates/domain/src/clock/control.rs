use super::ClockError;

/// The shortest and longest base time, and the longest increment, a game may have.
pub const MIN_BASE_MS: u64 = 1_000;
pub const MAX_BASE_MS: u64 = 3 * 60 * 60 * 1_000;
pub const MAX_INCREMENT_MS: u64 = 3 * 60 * 1_000;

/// A Fischer time control: each player starts with `base_ms`, and gets `increment_ms` added after every move
/// they make. (`3+2` is a base of 3 minutes and an increment of 2 seconds.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeControl {
    base_ms: u64,
    increment_ms: u64,
}

/// The speed class of a game, from its estimated duration. Ratings are kept per class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Bullet,
    Blitz,
    Rapid,
    Classical,
}

impl TimeControl {
    pub fn new(base_ms: u64, increment_ms: u64) -> Result<TimeControl, ClockError> {
        if !(MIN_BASE_MS..=MAX_BASE_MS).contains(&base_ms) {
            return Err(ClockError::InvalidTimeControl(
                "the base time must be between 1 second and 3 hours",
            ));
        }
        if increment_ms > MAX_INCREMENT_MS {
            return Err(ClockError::InvalidTimeControl(
                "the increment cannot be more than 3 minutes",
            ));
        }
        Ok(TimeControl {
            base_ms,
            increment_ms,
        })
    }

    pub fn base_ms(self) -> u64 {
        self.base_ms
    }

    pub fn increment_ms(self) -> u64 {
        self.increment_ms
    }

    /// The base time plus 40 increments: a standard estimate of how long a game takes, used to classify it.
    pub fn estimated_duration_ms(self) -> u64 {
        self.base_ms + 40 * self.increment_ms
    }

    pub fn category(self) -> Category {
        match self.estimated_duration_ms() {
            d if d < 180_000 => Category::Bullet,
            d if d < 480_000 => Category::Blitz,
            d if d < 1_500_000 => Category::Rapid,
            _ => Category::Classical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_enforced() {
        assert!(TimeControl::new(MIN_BASE_MS, 0).is_ok());
        assert!(TimeControl::new(MAX_BASE_MS, MAX_INCREMENT_MS).is_ok());
        for (base, inc) in [
            (0, 0),
            (999, 0),
            (MAX_BASE_MS + 1, 0),
            (60_000, MAX_INCREMENT_MS + 1),
            (u64::MAX, 0),
        ] {
            assert!(TimeControl::new(base, inc).is_err(), "{base}+{inc}");
        }
    }

    #[test]
    fn games_are_classified_by_estimated_duration() {
        let cat = |base, inc| TimeControl::new(base, inc).unwrap().category();
        assert_eq!(cat(60_000, 0), Category::Bullet); // 1+0
        assert_eq!(cat(60_000, 1_000), Category::Bullet); // 1+1 = 100s
        assert_eq!(cat(180_000, 0), Category::Blitz); // 3+0 = 180s: the boundary belongs to the longer class
        assert_eq!(cat(180_000, 2_000), Category::Blitz); // 3+2 = 260s
        assert_eq!(cat(300_000, 3_000), Category::Blitz); // 5+3 = 420s
        assert_eq!(cat(600_000, 0), Category::Rapid); // 10+0
        assert_eq!(cat(900_000, 10_000), Category::Rapid); // 15+10 = 1300s
        assert_eq!(cat(1_800_000, 0), Category::Classical);
        assert_eq!(
            TimeControl::new(180_000, 2_000)
                .unwrap()
                .estimated_duration_ms(),
            260_000
        );
    }
}
