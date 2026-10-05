//! Battery / whole-system power sampler via IOKit's `AppleSmartBattery`.
//!
//! `IOServiceGetMatchingService(IOServiceMatching("AppleSmartBattery"))` then
//! `IORegistryEntryCreateCFProperties` gives a CFDictionary of pack telemetry.
//! The headline number is whole-system drain:
//!
//!   system_power_mW = Voltage(mV) * InstantAmperage(mA) / 1000
//!
//! Negative = discharging (on battery). The signedness trap: ioreg prints
//! `InstantAmperage`/`Amperage` as unsigned 64-bit, but they are really i64
//! (e.g. 18446744073709550955 == -661). The shared `CFProps::i64` reads them
//! as signed, which gives the correct sign.

use crate::sampler::{BatterySampler, Sampler};
use crate::samplers::cf::CFProps;
use crate::types::BatteryFrame;

pub struct AppleSmartBatterySampler;

impl AppleSmartBatterySampler {
    pub fn new() -> Self {
        Self
    }
}

impl Sampler for AppleSmartBatterySampler {
    fn name(&self) -> &'static str {
        "AppleSmartBattery"
    }
}

impl BatterySampler for AppleSmartBatterySampler {
    fn read(&mut self) -> Option<BatteryFrame> {
        frame(&CFProps::for_service("AppleSmartBattery")?)
    }
}

/// Build a frame from AppleSmartBattery properties.
fn frame(props: &CFProps) -> Option<BatteryFrame> {
    let voltage_mv = props.f64("Voltage")?;
    // InstantAmperage must be read as signed i64 to get the discharge sign.
    let instant_ma = props
        .i64("InstantAmperage")
        .or_else(|| props.i64("Amperage"))? as f64;
    let external_connected = props.bool("ExternalConnected").unwrap_or(false);
    Some(BatteryFrame {
        system_power_mw: voltage_mv * instant_ma / 1000.0,
        voltage_mv,
        instant_amperage_ma: instant_ma,
        soc_percent: props.f64("CurrentCapacity").unwrap_or(0.0),
        temperature_c: props.f64("Temperature").unwrap_or(0.0) / 100.0,
        time_remaining_min: props.i64("TimeRemaining").filter(|m| *m > 0 && *m < 65535),
        is_charging: props.bool("IsCharging").unwrap_or(false),
        external_connected,
        adapter_watts: props
            .dict("AdapterDetails")
            .and_then(|a| a.f64("Watts"))
            .filter(|w| external_connected && *w > 0.),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samplers::cf::Value;
    use crate::types::BatteryState;

    /// Real reading: 60 W adapter, IsCharging=Yes, yet ioreg prints
    /// Amperage=18446744073709546162 (u64-wrapped -5454 mA) at 11.1 V.
    #[test]
    fn wrapped_negative_amperage_on_ac_is_draining_despite_charging_flag() {
        let wrapped = 18446744073709546162u64;
        let props = CFProps::from_pairs(&[
            ("Voltage", Value::Int(11097)),
            ("InstantAmperage", Value::Int(wrapped as i64)),
            ("CurrentCapacity", Value::Int(44)),
            ("IsCharging", Value::Bool(true)),
            ("ExternalConnected", Value::Bool(true)),
            (
                "AdapterDetails",
                Value::Dict(CFProps::from_pairs(&[("Watts", Value::Int(60))])),
            ),
        ]);
        let b = frame(&props).unwrap();
        assert_eq!(b.instant_amperage_ma, -5454.);
        assert!((b.system_power_mw / 1000. + 60.52).abs() < 0.01);
        assert!(b.is_charging);
        assert_eq!(b.adapter_watts, Some(60.));
        assert_eq!(b.state(), BatteryState::DrainingOnAc);
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["macos_is_charging_flag"], true);
        assert!(json.get("is_charging").is_none());
    }

    #[test]
    fn adapter_watts_ignored_without_external_power() {
        let props = CFProps::from_pairs(&[
            ("Voltage", Value::Int(12000)),
            ("Amperage", Value::Int(-1000)),
            ("ExternalConnected", Value::Bool(false)),
            (
                "AdapterDetails",
                Value::Dict(CFProps::from_pairs(&[("Watts", Value::Int(96))])),
            ),
        ]);
        let b = frame(&props).unwrap();
        assert_eq!(b.adapter_watts, None);
        assert_eq!(b.state(), BatteryState::DrainingOnBattery);
    }
}
