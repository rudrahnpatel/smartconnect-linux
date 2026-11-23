//! Turning a streamed phone display upright when the phone rotates it (svc 15 cmd 16).
//!
//! The phone keeps encoding the display's natural, portrait frame (720x1584 in a capture) and
//! draws a rotated app into it sideways; it reports the turn as `cmd 16 {rotation: 0..3}` and
//! sends no new frame size (OBSERVED on a motorola edge 70 max). So the receiver has to turn the
//! picture: `videoflip` in the decode chain, with the window changing shape to match. `videoflip`
//! maps the window's pointer positions back into the frame, so input needs no change.
//!
//! `SMARTCONNECT_MIRROR_ROTATION=off` leaves the picture as sent; `=invert` turns it the other way
//! (in case a phone rotates the opposite way to the one assumed here).

pub const ENV_ROTATION: &str = "SMARTCONNECT_MIRROR_ROTATION";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Normal,
    Inverted,
}

pub fn mode(env: Option<&str>) -> Mode {
    match env.map(str::trim) {
        Some("off" | "0" | "no" | "false") => Mode::Off,
        Some("invert" | "inverted") => Mode::Inverted,
        _ => Mode::Normal,
    }
}

pub fn mode_from_env() -> Mode {
    mode(std::env::var(ENV_ROTATION).ok().as_deref())
}

/// `videoflip`'s `video-direction` for a display rotated `rotation` quarter turns, Android's
/// `Surface.ROTATION_*`: 1 is a quarter turn counter-clockwise from natural, so the frame, which
/// holds the content turned the other way, is turned back counter-clockwise (INFERRED from
/// Android's definition; `invert` is the way out if it is wrong).
pub fn direction(rotation: i64, mode: Mode) -> &'static str {
    match (mode, rotation.rem_euclid(4)) {
        (Mode::Off, _) | (_, 0) => "identity",
        (_, 2) => "180",
        (Mode::Normal, 1) | (Mode::Inverted, 3) => "90l",
        _ => "90r",
    }
}

/// Whether the turned picture is landscape while the stream is portrait (or the reverse).
pub fn is_turned(rotation: i64, mode: Mode) -> bool {
    mode != Mode::Off && rotation.rem_euclid(4) % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_turns_flip_the_picture_and_half_turns_only_invert_it() {
        assert_eq!(direction(0, Mode::Normal), "identity");
        assert_eq!(direction(1, Mode::Normal), "90l");
        assert_eq!(direction(2, Mode::Normal), "180");
        assert_eq!(direction(3, Mode::Normal), "90r");
        assert_eq!(direction(-1, Mode::Normal), "90r");
        assert_eq!(direction(5, Mode::Normal), "90l");
    }

    #[test]
    fn invert_turns_the_other_way_and_off_changes_nothing() {
        assert_eq!(direction(1, Mode::Inverted), "90r");
        assert_eq!(direction(3, Mode::Inverted), "90l");
        assert_eq!(direction(2, Mode::Inverted), "180");
        assert!((0..4).all(|r| direction(r, Mode::Off) == "identity" && !is_turned(r, Mode::Off)));
    }

    #[test]
    fn only_quarter_turns_change_the_window_shape() {
        let turned: Vec<bool> = (0..4).map(|r| is_turned(r, Mode::Normal)).collect();
        assert_eq!(turned, [false, true, false, true]);
    }

    #[test]
    fn the_environment_picks_the_mode() {
        assert_eq!(mode(None), Mode::Normal);
        assert_eq!(mode(Some("off")), Mode::Off);
        assert_eq!(mode(Some(" invert ")), Mode::Inverted);
        assert_eq!(mode(Some("1")), Mode::Normal);
    }
}
