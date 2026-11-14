//! How bright plain SDR white is on the display.
//!
//! DXGI says what a display can do (peak luminance, colour space); it does not
//! say where the user put the SDR white point. Windows does, through the
//! DisplayConfig API: each active path carries a `DISPLAYCONFIG_SDR_WHITE_LEVEL`
//! for its target, found by matching the path's GDI device name against the
//! DXGI output's.
//!
//! The value is the multiplier Windows applies to the SDR reference white of
//! 80 nits, in thousandths: `nits = SDRWhiteLevel / 1000 * 80`.

use std::mem::size_of;

use windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
use windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
use windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO;
use windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO;
use windows::Win32::Devices::Display::DISPLAYCONFIG_SDR_WHITE_LEVEL;
use windows::Win32::Devices::Display::DISPLAYCONFIG_SOURCE_DEVICE_NAME;
use windows::Win32::Devices::Display::DisplayConfigGetDeviceInfo;
use windows::Win32::Devices::Display::GetDisplayConfigBufferSizes;
use windows::Win32::Devices::Display::QDC_ONLY_ACTIVE_PATHS;
use windows::Win32::Devices::Display::QueryDisplayConfig;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Foundation::LUID;

/// The SDR reference white Windows scales: 80 nits at a level of 1000.
const SDR_REFERENCE_NITS: f32 = 80.0;

/// The SDR white point of the output whose GDI device name is `device_name`.
///
/// `device_name` is `DXGI_OUTPUT_DESC::DeviceName` (`\\.\DISPLAY1`), padded with
/// zeroes to 32 wide characters.
pub fn white_nits(device_name: &[u16; 32]) -> anyhow::Result<f32> {
    for path in active_paths()? {
        if source_name(&path) != *device_name {
            continue;
        }
        return white_level(path.targetInfo.adapterId, path.targetInfo.id);
    }
    anyhow::bail!("no active display path for this output")
}

/// Every active display path, source and target together.
fn active_paths() -> anyhow::Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
    let mut paths = 0;
    let mut modes = 0;
    // SAFETY: both out-pointers name live locals.
    let status =
        unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "GetDisplayConfigBufferSizes failed: {}",
        status.0
    );

    let mut path_info = vec![DISPLAYCONFIG_PATH_INFO::default(); usize::try_from(paths)?];
    let mut mode_info = vec![DISPLAYCONFIG_MODE_INFO::default(); usize::try_from(modes)?];
    // SAFETY: both buffers have exactly the element counts the previous call
    // asked for, and the counts are updated in place.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths,
            path_info.as_mut_ptr(),
            &mut modes,
            mode_info.as_mut_ptr(),
            None,
        )
    };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "QueryDisplayConfig failed: {}",
        status.0
    );
    path_info.truncate(usize::try_from(paths)?);
    Ok(path_info)
}

/// The GDI device name a path's source draws into.
fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> [u16; 32] {
    let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    name.header.size = u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).unwrap_or(0);
    name.header.adapterId = path.sourceInfo.adapterId;
    name.header.id = path.sourceInfo.id;
    // SAFETY: the header is filled in, and the struct is at least as large as
    // the size it advertises.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut name.header) };
    if status == ERROR_SUCCESS.0 as i32 {
        name.viewGdiDeviceName
    } else {
        [0; 32]
    }
}

/// The SDR white level of one target, in nits.
fn white_level(adapter: LUID, id: u32) -> anyhow::Result<f32> {
    let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
    level.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
    level.header.size = u32::try_from(size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>()).unwrap_or(0);
    level.header.adapterId = adapter;
    level.header.id = id;
    // SAFETY: as above; this request names a target of the same path.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut level.header) };
    anyhow::ensure!(
        status == ERROR_SUCCESS.0 as i32,
        "DisplayConfigGetDeviceInfo(SDR_WHITE_LEVEL) failed: {status}"
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "the level is a small integer, far inside f32's exact range"
    )]
    let nits = level.SDRWhiteLevel as f32 / 1000.0 * SDR_REFERENCE_NITS;
    anyhow::ensure!(nits > 1.0, "the display reports an unusable SDR white level");
    Ok(nits)
}
