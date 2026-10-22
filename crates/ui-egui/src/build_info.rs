//! Identity baked into this executable; replacing an installed app cannot change it.
pub const NUMBER: &str = env!("FILMCRAFT_BUILD_NUMBER");
pub const PROFILE: &str = env!("FILMCRAFT_BUILD_PROFILE");
pub const COMMIT: &str = env!("FILMCRAFT_BUILD_SHA");
pub const DATE: &str = env!("FILMCRAFT_BUILD_DATE");

pub fn label() -> String {
    format!("FilmCraft {} · {PROFILE} · Build {NUMBER}", env!("CARGO_PKG_VERSION"))
}
