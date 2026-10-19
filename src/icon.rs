//! 运行时像素绘制的 32×32 托盘图标（额度饼环）——无需内嵌 .ico。
//!
//! 外环灰框；内饼按"已用百分比"从 12 点顺时针填充，颜色与条上色点同阈值：
//! 已用 ≥(100-阈值) 红、≥50 黄、否则绿；无数据 = 空灰饼。

use std::mem::{size_of, zeroed};

use windows::Win32::Foundation::{BOOL, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, GetDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

const S: usize = 32;

/// `used` = 已用百分比（0–100），`None` = 未知/未取到。
pub fn make_icon(used: Option<f64>, low_threshold: u8) -> windows::core::Result<HICON> {
    unsafe {
        let dc = GetDC(HWND::default());

        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = S as i32;
        bmi.bmiHeader.biHeight = -(S as i32); // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)?;

        let px = bits.cast::<u32>();
        let used = used.map(|u| u.clamp(0.0, 100.0));
        let slice_color = match used {
            Some(u) if u >= 100.0 - low_threshold.max(1) as f64 => 0xE5484D,
            Some(u) if u >= 50.0 => 0xE8930C,
            Some(_) => 0x2FA05C,
            None => 0x0000_0000,
        };
        let used_frac = used.map(|u| u / 100.0).unwrap_or(0.0);

        let (cx, cy) = (16.0f64, 16.0f64);
        for y in 0..S as i32 {
            for x in 0..S as i32 {
                let dx = x as f64 + 0.5 - cx;
                let dy = y as f64 + 0.5 - cy;
                let r2 = dx * dx + dy * dy;
                let rgb = if (11.0..=14.5).contains(&r2.sqrt()) {
                    // 外环
                    0x45454A
                } else if r2 <= 10.0 * 10.0 {
                    // 内饼：12 点起顺时针
                    if used.is_none() {
                        0x2A2A2E
                    } else {
                        let mut deg = dx.atan2(-dy).to_degrees(); // 北=0，东=90
                        if deg < 0.0 {
                            deg += 360.0;
                        }
                        // 0% 必须画不出扇形（`<=` 会把北边界 0° 算进去）
                        if used_frac > 0.0 && deg / 360.0 <= used_frac {
                            slice_color
                        } else {
                            0x2A2A2E
                        }
                    }
                } else {
                    // 图标外：alpha=0
                    *px.add((y * S as i32 + x) as usize) = 0;
                    continue;
                };
                *px.add((y * S as i32 + x) as usize) = (0xFFu32 << 24) | rgb;
            }
        }

        let hbm_mask = CreateBitmap(S as i32, S as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbmp,
        };
        let icon = CreateIconIndirect(&info)?;

        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        let _ = ReleaseDC(HWND::default(), dc);
        Ok(icon)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_math_slice_angle_and_boundaries() {
        // 与 make_icon 同款的扇形判定：北=0°、东=90°、南=180°、西=270°，顺时针
        let deg = |dx: f64, dy: f64| {
            let mut d = dx.atan2(-dy).to_degrees();
            if d < 0.0 {
                d += 360.0;
            }
            d
        };
        assert_eq!(deg(0.0, -1.0), 0.0); // 北
        assert_eq!(deg(1.0, 0.0), 90.0); // 东
        assert_eq!(deg(0.0, 1.0), 180.0); // 南
        assert_eq!(deg(-1.0, 0.0), 270.0); // 西

        let covers = |used: f64, dx: f64, dy: f64| {
            used > 0.0 && deg(dx, dy) / 360.0 <= used / 100.0
        };
        assert!(!covers(0.0, 0.0, -1.0)); // 0%：不含任何角
        assert!(covers(100.0, -1.0, 0.0)); // 100%：全含（含西边界）
        assert!(covers(25.0, 1.0, 0.0)); // 25% 恰到东 90°
        assert!(!covers(25.0, 0.0, 1.0)); // 25% 不含南 180°
        assert!(covers(50.0, 0.0, 1.0)); // 50% 恰到南 180°
        assert!(!covers(50.0, -1.0, 0.0)); // 50% 不含西 270°
        assert!(covers(75.0, -1.0, 0.0)); // 75% 恰到西 270°
    }
}
