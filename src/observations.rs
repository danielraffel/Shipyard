//! Process-scoped policy for optional automatic observations only.
//! Explicit imports, historical reads and operational evidence do not use it.

#[derive(Clone, Copy, Debug)]
pub(crate) struct AutomaticObservations(bool);

impl AutomaticObservations {
    pub(crate) fn from_env() -> Self {
        Self::from_value(std::env::var("SHIPYARD_OBSERVATIONS").ok().as_deref())
    }

    pub(crate) fn from_value(value: Option<&str>) -> Self {
        Self(!value.is_some_and(|value| value.trim().eq_ignore_ascii_case("off")))
    }

    pub(crate) fn enabled(self) -> bool {
        self.0
    }
}
