#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceConditions {
    pub low_power_mode: bool,
    pub on_battery: bool,
    pub battery_percent: Option<u8>,
    pub severe_thermal_state: bool,
    pub memory_pressure: bool,
    pub volume_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkAllowance {
    Normal,
    Reduced,
    Paused,
}

pub const fn work_allowance(user_paused: bool, conditions: ResourceConditions) -> WorkAllowance {
    if user_paused
        || !conditions.volume_available
        || conditions.severe_thermal_state
        || conditions.memory_pressure
    {
        WorkAllowance::Paused
    } else if conditions.low_power_mode
        || (conditions.on_battery
            && matches!(conditions.battery_percent, Some(percent) if percent <= 20))
    {
        WorkAllowance::Reduced
    } else {
        WorkAllowance::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_conditions_pause_and_constrained_power_reduces_work() {
        let normal = ResourceConditions {
            volume_available: true,
            ..ResourceConditions::default()
        };
        assert_eq!(work_allowance(false, normal), WorkAllowance::Normal);
        assert_eq!(work_allowance(true, normal), WorkAllowance::Paused);
        assert_eq!(
            work_allowance(
                false,
                ResourceConditions {
                    low_power_mode: true,
                    ..normal
                }
            ),
            WorkAllowance::Reduced
        );
        for constrained in [
            ResourceConditions {
                severe_thermal_state: true,
                ..normal
            },
            ResourceConditions {
                memory_pressure: true,
                ..normal
            },
            ResourceConditions {
                volume_available: false,
                ..normal
            },
        ] {
            assert_eq!(work_allowance(false, constrained), WorkAllowance::Paused);
        }
    }

    #[test]
    fn low_battery_reduces_only_while_running_on_battery() {
        let low = ResourceConditions {
            on_battery: true,
            battery_percent: Some(20),
            volume_available: true,
            ..ResourceConditions::default()
        };
        assert_eq!(work_allowance(false, low), WorkAllowance::Reduced);
        assert_eq!(
            work_allowance(
                false,
                ResourceConditions {
                    on_battery: false,
                    ..low
                }
            ),
            WorkAllowance::Normal
        );
    }
}
