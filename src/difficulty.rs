//! Endless difficulty: EASY / NORMAL / HARD on the endless setup screen.
//! Every number it changes is in [`DifficultyParams`]. NORMAL is the game as
//! it always was; challenge and multiplayer always play NORMAL.

use crate::{modes::GameMode, weapons::Seeker};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Difficulty {
    Easy,
    #[default]
    Normal,
    Hard,
}

impl Difficulty {
    pub const ALL: [Self; 3] = [Self::Easy, Self::Normal, Self::Hard];

    pub fn name(self) -> &'static str {
        match self {
            Self::Easy => "easy",
            Self::Normal => "normal",
            Self::Hard => "hard",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn params(self) -> &'static DifficultyParams {
        match self {
            Self::Easy => &EASY,
            Self::Normal => &NORMAL,
            Self::Hard => &HARD,
        }
    }
}

/// The round's difficulty: the picked one in endless, EASY in the tutorial, NORMAL otherwise.
pub fn active(mode: GameMode, picked: Difficulty) -> &'static DifficultyParams {
    match mode {
        GameMode::Endless => picked.params(),
        GameMode::Tutorial => &EASY,
        _ => &NORMAL,
    }
}

pub struct DifficultyParams {
    /// Hostiles in wave `n` (1-based): `wave_base + wave_step * n`, at most `wave_max`.
    pub wave_base: u32,
    pub wave_step: u32,
    pub wave_max: u32,
    /// Hostile skill (0.5 rookie .. 1.0 ace): `lo + (hi - lo) * r^skew` for a
    /// uniform `r`, so a skew above 1 piles them up near `lo`, below 1 near `hi`.
    pub skill: (f32, f32),
    pub skill_skew: f32,
    /// The player's missiles.
    pub seeker: Seeker,
    /// Seconds for the player's missile lock to build.
    pub lock_time: f32,
}

impl DifficultyParams {
    pub fn wave_size(&self, wave: u32) -> u32 {
        (self.wave_base + self.wave_step * wave).min(self.wave_max)
    }

    /// A hostile's skill for a uniform `r` in 0..1.
    pub fn skill_at(&self, r: f32) -> f32 {
        let (lo, hi) = self.skill;
        lo + (hi - lo) * r.clamp(0.0, 1.0).powf(self.skill_skew)
    }

    /// The mean skill.
    #[cfg(test)]
    pub fn mean_skill(&self) -> f32 {
        let (lo, hi) = self.skill;
        lo + (hi - lo) / (1.0 + self.skill_skew)
    }
}

/// 3, 4, 5, 6 hostiles (then 6), mostly rookies; the player's missiles pull
/// harder, react faster, see wider, mostly ignore flares and lock quicker.
pub const EASY: DifficultyParams = DifficultyParams {
    wave_base: 2,
    wave_step: 1,
    wave_max: 6,
    skill: (0.5, 0.85),
    skill_skew: 2.0,
    seeker: Seeker { max_g: 65.0, gimbal: 1.25, lag: 0.5, nav_gain: 4.0, decoy_odds: 0.15 },
    lock_time: 0.6,
};

/// The original game: 6, 8, 10, 12 hostiles, skill evenly 0.55..1.0.
pub const NORMAL: DifficultyParams = DifficultyParams {
    wave_base: 4,
    wave_step: 2,
    wave_max: 12,
    skill: (0.55, 1.0),
    skill_skew: 1.0,
    seeker: Seeker::NORMAL,
    lock_time: 1.2,
};

/// 8, 10, 12, 14, 16 hostiles, mostly aces; the player's missiles are a
/// little easier to shake and lock a little slower.
pub const HARD: DifficultyParams = DifficultyParams {
    wave_base: 6,
    wave_step: 2,
    wave_max: 16,
    skill: (0.7, 1.0),
    skill_skew: 0.5,
    seeker: Seeker { max_g: 40.0, gimbal: 0.87, lag: 1.1, nav_gain: 3.0, decoy_odds: 0.65 },
    lock_time: 1.5,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_is_the_original_game() {
        let n = Difficulty::Normal.params();
        assert_eq!((1..=5).map(|w| n.wave_size(w)).collect::<Vec<_>>(), [6, 8, 10, 12, 12]);
        assert_eq!((n.skill_at(0.0), n.skill_at(1.0)), (0.55, 1.0));
        assert!((n.skill_at(0.5) - 0.775).abs() < 1e-6);
    }

    #[test]
    fn easier_is_fewer_and_greener() {
        let [e, n, h] = Difficulty::ALL.map(|d| d.params());
        for w in 1..=8 {
            assert!(e.wave_size(w) < n.wave_size(w) && n.wave_size(w) < h.wave_size(w));
        }
        assert!(e.mean_skill() < n.mean_skill() && n.mean_skill() < h.mean_skill());
        println!("mean skill: easy {:.3}, normal {:.3}, hard {:.3}", e.mean_skill(), n.mean_skill(), h.mean_skill());
    }
}
