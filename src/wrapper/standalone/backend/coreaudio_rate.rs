//! A CoreAudio device's nominal rate, read straight from the HAL: one property read, microseconds.
//! cpal reads a device's rate by building and initializing an output AudioUnit, tens of
//! milliseconds a read, too costly for the run loop's check once a second (spec A17). The device
//! is resolved once per open. Off macOS nothing resolves.

/// The HAL device a shared CoreAudio output stream runs on.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeviceRate(u32);

impl DeviceRate {
    /// The device a stream opened on the output named `name` runs on: the HAL device with output
    /// streams that cpal names alike. `None` when there is none, or several and `on_default` does
    /// not single one out.
    #[cfg(target_os = "macos")]
    pub(crate) fn resolve(name: &str, on_default: bool) -> Option<Self> {
        let named: Vec<u32> = hal::devices()?
            .into_iter()
            .filter(|&id| hal::has_output(id) && hal::name(id).as_deref() == Some(name))
            .collect();
        let picked = pick(&named, hal::default_output(), on_default);
        if picked.is_none() {
            nih_log!(
                "{} output devices are named '{name}'; the session does not follow a rate change \
                 on this one",
                named.len()
            );
        }
        picked.map(Self)
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn resolve(_name: &str, _on_default: bool) -> Option<Self> {
        None
    }

    /// The device's nominal rate now. `None` when it does not answer, as when it is gone.
    #[cfg(target_os = "macos")]
    pub(crate) fn read(self) -> Option<f64> {
        hal::nominal_rate(self.0)
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn read(self) -> Option<f64> {
        None
    }
}

/// Which of the devices named like the opened output it runs on: the only one, or the system
/// default when the stream opened on the default.
#[cfg(any(target_os = "macos", test))]
fn pick(named: &[u32], default: Option<u32>, on_default: bool) -> Option<u32> {
    match named {
        [only] => Some(*only),
        _ if on_default => default.filter(|id| named.contains(id)),
        _ => None,
    }
}

/// Every unsafe call of this module: `AudioObjectGetPropertyData(Size)` on plain values.
#[cfg(target_os = "macos")]
mod hal {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use coreaudio_sys::{
        kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMaster,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress, CFStringRef,
    };
    use std::mem::{size_of, MaybeUninit};
    use std::ptr::null;

    fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMaster,
        }
    }

    fn size(object: AudioObjectID, address: &AudioObjectPropertyAddress) -> Option<u32> {
        let mut size = 0;
        // SAFETY: both pointers are valid for the call, which writes only `size`.
        let status =
            unsafe { AudioObjectGetPropertyDataSize(object, address, 0, null(), &mut size) };
        (status == 0).then_some(size)
    }

    /// `address` on `object`, read into a `T`; a read of another size is refused.
    ///
    /// # Safety
    ///
    /// `T` must be the type the HAL documents for the property (`AudioObjectID`, `Float64`,
    /// `CFStringRef`), so that any value it writes is a valid `T`.
    unsafe fn value<T: Copy>(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Option<T> {
        let mut value = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let status = AudioObjectGetPropertyData(
            object,
            address,
            0,
            null(),
            &mut size,
            value.as_mut_ptr().cast(),
        );
        (status == 0 && size as usize == size_of::<T>()).then(|| value.assume_init())
    }

    pub(super) fn devices() -> Option<Vec<AudioObjectID>> {
        let address = address(
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        );
        let capacity =
            size(kAudioObjectSystemObject, &address)? as usize / size_of::<AudioObjectID>();
        let mut devices: Vec<AudioObjectID> = vec![0; capacity];
        let mut size = (capacity * size_of::<AudioObjectID>()) as u32;
        // SAFETY: the HAL writes at most `size` bytes, which `devices` holds, and reports how many
        // it wrote; the list may have shrunk since it was sized.
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                null(),
                &mut size,
                devices.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return None;
        }
        devices.truncate(size as usize / size_of::<AudioObjectID>());
        Some(devices)
    }

    pub(super) fn default_output() -> Option<AudioObjectID> {
        let address = address(
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        );
        // SAFETY: the property is an `AudioObjectID`.
        unsafe { value(kAudioObjectSystemObject, &address) }
    }

    pub(super) fn has_output(device: AudioObjectID) -> bool {
        let address = address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput);
        size(device, &address).is_some_and(|bytes| bytes > 0)
    }

    /// The name cpal reports for the device: the same property, in the same scope.
    pub(super) fn name(device: AudioObjectID) -> Option<String> {
        let address = address(
            kAudioDevicePropertyDeviceNameCFString,
            kAudioObjectPropertyScopeOutput,
        );
        // SAFETY: the property is a `CFStringRef`.
        let name: CFStringRef = unsafe { value(device, &address) }?;
        if name.is_null() {
            return None;
        }
        // SAFETY: the HAL hands the caller a reference to the string it must release, which the
        // wrapper does when dropped.
        let name = unsafe { CFString::wrap_under_create_rule(name.cast()) };
        Some(name.to_string())
    }

    pub(super) fn nominal_rate(device: AudioObjectID) -> Option<f64> {
        let address = address(
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        );
        // SAFETY: the property is a `Float64`.
        unsafe { value(device, &address) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_only_device_with_the_name_is_the_one() {
        assert_eq!(pick(&[7], None, false), Some(7));
        assert_eq!(pick(&[7], Some(9), true), Some(7));
    }

    #[test]
    fn no_device_with_the_name_resolves_nothing() {
        assert_eq!(pick(&[], Some(9), true), None);
        assert_eq!(pick(&[], None, false), None);
    }

    #[test]
    fn devices_alike_resolve_only_to_the_default_the_stream_opened_on() {
        assert_eq!(pick(&[7, 9], Some(9), true), Some(9));
        assert_eq!(pick(&[7, 9], Some(9), false), None);
        assert_eq!(pick(&[7, 9], Some(3), true), None);
        assert_eq!(pick(&[7, 9], None, true), None);
    }
}
