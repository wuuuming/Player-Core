//! ReplayGain: the level a song is played at so that songs sound equally loud, read from the
//! values its tags state. Which value a session plays at is the host's setting; the values
//! themselves are the song's, read where it is decoded.

/// Which of a song's ReplayGain values its sound is played at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ReplayGain {
    /// The sound as the file holds it.
    #[default]
    Off,
    /// The song's own value, else its album's.
    Track,
    /// The album's value, else the song's own: the songs of one album keep their levels apart.
    Album,
}

#[cfg(any(windows, target_os = "linux"))]
impl ReplayGain {
    pub(crate) fn to_bits(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Track => 1,
            Self::Album => 2,
        }
    }

    pub(crate) fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Track,
            2 => Self::Album,
            _ => Self::Off,
        }
    }
}

/// A song's ReplayGain values in dB, as its tags state them: `REPLAYGAIN_TRACK_GAIN` and
/// `REPLAYGAIN_ALBUM_GAIN` (`-6.48 dB`), else Opus' `R128_TRACK_GAIN` / `R128_ALBUM_GAIN` (Q7.8
/// against −23 LUFS, moved to ReplayGain's −18). A value that does not read, or reads outside
/// ±60 dB, is not there.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReplayGainTags {
    pub track: Option<f32>,
    pub album: Option<f32>,
}

#[cfg(any(windows, target_os = "linux"))]
impl ReplayGainTags {
    /// The values `lookup` finds by tag name (any case).
    pub(crate) fn read(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let value = |gain: &str, r128: &str| {
            lookup(gain)
                .as_deref()
                .and_then(parse_gain)
                .or_else(|| lookup(r128).as_deref().and_then(parse_r128))
        };
        Self {
            track: value("REPLAYGAIN_TRACK_GAIN", "R128_TRACK_GAIN"),
            album: value("REPLAYGAIN_ALBUM_GAIN", "R128_ALBUM_GAIN"),
        }
    }

    /// The gain `mode` plays at, as a factor: the value it names, else the other one, else unity.
    pub(crate) fn factor(self, mode: ReplayGain) -> f32 {
        let db = match mode {
            ReplayGain::Off => None,
            ReplayGain::Track => self.track.or(self.album),
            ReplayGain::Album => self.album.or(self.track),
        };
        db.map_or(1.0, |db| 10f32.powf(db / 20.0))
    }
}

/// The largest gain a tag may state; beyond it the tag is taken as damaged.
#[cfg(any(windows, target_os = "linux"))]
const GAIN_LIMIT_DB: f32 = 60.0;

/// `-6.48 dB`, `+2.1 dB`, `-6.48`: a signed decimal, an optional `dB` in any case.
#[cfg(any(windows, target_os = "linux"))]
fn parse_gain(text: &str) -> Option<f32> {
    let text = text.trim();
    let number = text
        .len()
        .checked_sub(2)
        .filter(|&at| text.is_char_boundary(at) && text[at..].eq_ignore_ascii_case("db"))
        .map_or(text, |at| &text[..at]);
    let db = number.trim().parse::<f32>().ok()?;
    (db.is_finite() && db.abs() <= GAIN_LIMIT_DB).then_some(db)
}

/// Opus' R128 gain: an integer in 1/256 dB against −23 LUFS; ReplayGain's reference is 5 dB
/// louder.
#[cfg(any(windows, target_os = "linux"))]
fn parse_r128(text: &str) -> Option<f32> {
    let q78 = text.trim().parse::<i32>().ok()?;
    let db = q78 as f32 / 256.0 + 5.0;
    (db.abs() <= GAIN_LIMIT_DB).then_some(db)
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn gains_read_in_every_form_tags_write_them() {
        assert_eq!(parse_gain("-6.48 dB"), Some(-6.48));
        assert_eq!(parse_gain(" +2.10 DB "), Some(2.1));
        assert_eq!(parse_gain("-3"), Some(-3.0));
        assert_eq!(parse_gain("loud"), None);
        assert_eq!(parse_gain("99 dB"), None);
        assert_eq!(parse_gain("NaN dB"), None);
        assert_eq!(parse_r128("-1280"), Some(0.0));
        assert_eq!(parse_r128("256"), Some(6.0));
    }

    #[test]
    fn a_mode_falls_back_to_the_other_value_then_to_unity() {
        let both = ReplayGainTags {
            track: Some(-20.0),
            album: Some(20.0),
        };
        assert_eq!(both.factor(ReplayGain::Off), 1.0);
        assert!((both.factor(ReplayGain::Track) - 0.1).abs() < 1e-6);
        assert!((both.factor(ReplayGain::Album) - 10.0).abs() < 1e-4);
        let track = ReplayGainTags {
            track: Some(-20.0),
            album: None,
        };
        assert!((track.factor(ReplayGain::Album) - 0.1).abs() < 1e-6);
        assert_eq!(ReplayGainTags::default().factor(ReplayGain::Track), 1.0);
        let read =
            ReplayGainTags::read(|name| (name == "R128_TRACK_GAIN").then(|| "-1280".to_owned()));
        assert_eq!(
            read,
            ReplayGainTags {
                track: Some(0.0),
                album: None
            }
        );
    }
}
