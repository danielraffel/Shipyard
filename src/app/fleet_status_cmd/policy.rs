use crate::capacity::parse_host_classes;
use crate::config::LoadedConfig;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum FleetLivenessPolicy {
    NoConfiguredPool,
    Delegated,
    MonitorConfiguredPool { every_ticks: u32 },
    InvalidPoolConfiguration { every_ticks: u32 },
}

impl FleetLivenessPolicy {
    pub(in crate::app) fn is_due(self, iteration: u32) -> bool {
        match self {
            Self::NoConfiguredPool | Self::Delegated => false,
            Self::MonitorConfiguredPool { every_ticks }
            | Self::InvalidPoolConfiguration { every_ticks } => {
                iteration.is_multiple_of(every_ticks)
            }
        }
    }
}

pub(in crate::app) fn fleet_liveness_policy(config: &LoadedConfig) -> FleetLivenessPolicy {
    let every_ticks = config
        .get("runner.watchdog.fleet_liveness_every_ticks")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1)
        .max(1);
    let delegated = !config
        .get("runner.watchdog.fleet_liveness")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    if delegated {
        return FleetLivenessPolicy::Delegated;
    }
    match parse_host_classes(&config.data) {
        Ok(classes) if classes.is_empty() => FleetLivenessPolicy::NoConfiguredPool,
        Ok(_) => FleetLivenessPolicy::MonitorConfiguredPool { every_ticks },
        Err(_) => FleetLivenessPolicy::InvalidPoolConfiguration { every_ticks },
    }
}
