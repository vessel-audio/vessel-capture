//! Input device enumeration and selection by name.

use cpal::traits::{DeviceTrait, HostTrait};

use crate::error::CaptureError;

#[derive(Clone, Debug, PartialEq)]
pub struct InputDeviceInfo {
    pub name: String,
    pub is_default: bool,
}

fn input_device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|desc| desc.to_string())
        .unwrap_or_else(|_| "unnamed device".to_string())
}

/// Enumerate available input devices.
///
/// Default detection uses device IDs rather than names so that duplicate
/// device names do not cause multiple devices to be marked as default.
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, CaptureError> {
    let host = cpal::default_host();
    let default_id: Option<cpal::DeviceId> = host.default_input_device().and_then(|d| d.id().ok());

    let devices: Vec<InputDeviceInfo> = host
        .input_devices()
        .map_err(|err| CaptureError::InputDeviceEnumeration(err.to_string()))?
        .map(|d| {
            let name = input_device_name(&d);
            let is_default = default_id
                .as_ref()
                .and_then(|def_id| d.id().ok().map(|dev_id| def_id == &dev_id))
                .unwrap_or(false);
            InputDeviceInfo { name, is_default }
        })
        .collect();

    Ok(devices)
}

/// Select an input device by optional name.
///
/// When `requested` is `None`, the default input device is returned.
/// When `requested` is `Some(name)`, an exact name match is required.
pub fn select_input_device(requested: Option<&str>) -> Result<InputDeviceInfo, CaptureError> {
    let devices = list_input_devices()?;
    select_input_device_from_list(requested, &devices)
}

/// Select an input device from a pre-enumerated list (testable).
pub fn select_input_device_from_list(
    requested: Option<&str>,
    devices: &[InputDeviceInfo],
) -> Result<InputDeviceInfo, CaptureError> {
    match requested {
        None => devices
            .iter()
            .find(|d| d.is_default)
            .cloned()
            .ok_or(CaptureError::NoDefaultInputDevice),
        Some(name) => {
            let matches: Vec<&InputDeviceInfo> =
                devices.iter().filter(|d| d.name == name).collect();
            match matches.len() {
                0 => Err(CaptureError::InputDeviceNotFound(name.to_string())),
                1 => Ok(matches[0].clone()),
                _ => Err(CaptureError::AmbiguousInputDevice(name.to_string())),
            }
        }
    }
}

/// Resolve an input device by optional name for use in config negotiation.
///
/// When `name` is `None`, returns the default input device. When `name` is
/// `Some`, searches by exact name. Returns the cpal `Device` so callers can
/// inspect supported configurations before building a stream.
pub fn resolve_input_device(device_name: Option<&str>) -> Result<cpal::Device, CaptureError> {
    let host = cpal::default_host();
    match device_name {
        None => host
            .default_input_device()
            .ok_or(CaptureError::NoDefaultInputDevice),
        Some(name) => {
            let mut found = None;
            let devices = host
                .input_devices()
                .map_err(|e| CaptureError::InputDeviceEnumeration(e.to_string()))?;
            for d in devices {
                if input_device_name(&d) == name {
                    if found.is_some() {
                        return Err(CaptureError::AmbiguousInputDevice(name.to_string()));
                    }
                    found = Some(d);
                }
            }
            found.ok_or_else(|| CaptureError::InputDeviceNotFound(name.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{select_input_device_from_list, InputDeviceInfo};
    use crate::CaptureError;

    fn make_device(name: &str, is_default: bool) -> InputDeviceInfo {
        InputDeviceInfo {
            name: name.to_string(),
            is_default,
        }
    }

    #[test]
    fn default_device_returned_when_no_name_given() {
        let devices = vec![make_device("Mic A", false), make_device("Mic B", true)];
        let selected = select_input_device_from_list(None, &devices).unwrap();
        assert_eq!(selected, make_device("Mic B", true));
    }

    #[test]
    fn exact_name_selection_succeeds() {
        let devices = vec![make_device("Mic A", true), make_device("Mic B", false)];
        let selected = select_input_device_from_list(Some("Mic A"), &devices).unwrap();
        assert_eq!(selected, make_device("Mic A", true));
    }

    #[test]
    fn missing_name_errors() {
        let devices = vec![make_device("Mic A", true)];
        let err = select_input_device_from_list(Some("Missing"), &devices).unwrap_err();
        assert!(matches!(err, CaptureError::InputDeviceNotFound(name) if name == "Missing"));
    }

    #[test]
    fn duplicate_name_errors() {
        let devices = vec![make_device("Mic", false), make_device("Mic", false)];
        let err = select_input_device_from_list(Some("Mic"), &devices).unwrap_err();
        assert!(matches!(err, CaptureError::AmbiguousInputDevice(name) if name == "Mic"));
    }

    #[test]
    fn no_default_device_errors() {
        let devices = vec![make_device("Mic A", false)];
        let err = select_input_device_from_list(None, &devices).unwrap_err();
        assert!(matches!(err, CaptureError::NoDefaultInputDevice));
    }

    #[test]
    fn input_info_preserves_name_and_default() {
        let info = make_device("USB Mic", true);
        assert_eq!(info.name, "USB Mic");
        assert!(info.is_default);
    }

    #[test]
    fn empty_device_list_returns_no_default() {
        let devices: Vec<InputDeviceInfo> = vec![];
        let err = select_input_device_from_list(None, &devices).unwrap_err();
        assert!(matches!(err, CaptureError::NoDefaultInputDevice));
    }

    #[test]
    fn empty_device_list_with_exact_name_returns_not_found() {
        let devices: Vec<InputDeviceInfo> = vec![];
        let err = select_input_device_from_list(Some("Mic"), &devices).unwrap_err();
        assert!(matches!(err, CaptureError::InputDeviceNotFound(name) if name == "Mic"));
    }
}
