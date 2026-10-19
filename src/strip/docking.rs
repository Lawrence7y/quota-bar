//! Taskbar vacancy discovery. UI Automation runs off the UI thread.
use super::geometry::{
    GAP_TRAY, Metrics, TASKBAR_RESERVE, client_origin_screen, hw, taskbar_client_geometry,
    taskbar_screen_rect,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_ButtonControlTypeId,
    UIA_EditControlTypeId, UIA_ListItemControlTypeId,
};

struct Entry {
    checked: Instant,
    pending: bool,
    rectangles: Option<Vec<RECT>>,
}
struct Cache {
    entries: Mutex<BTreeMap<isize, Entry>>,
    sender: mpsc::Sender<isize>,
    revision: AtomicU64,
}
static CACHE: OnceLock<Cache> = OnceLock::new();

fn cache() -> &'static Cache {
    CACHE.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let automation: Option<IUIAutomation> =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() };
            for taskbar in receiver {
                let rectangles = automation.as_ref().and_then(|a| scan(a, taskbar));
                if let Some(cache) = CACHE.get() {
                    if let Some(entry) = cache.entries.lock().unwrap().get_mut(&taskbar) {
                        let changed = entry.rectangles != rectangles;
                        entry.rectangles = rectangles;
                        entry.pending = false;
                        entry.checked = Instant::now();
                        if changed {
                            cache.revision.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        Cache {
            entries: Mutex::new(BTreeMap::new()),
            sender,
            revision: AtomicU64::new(0),
        }
    })
}

fn scan(automation: &IUIAutomation, taskbar: isize) -> Option<Vec<RECT>> {
    let tb = taskbar_screen_rect(hw(taskbar))?;
    unsafe {
        let root = automation.ElementFromHandle(hw(taskbar)).ok()?;
        let condition = automation.CreateTrueCondition().ok()?;
        let elements = root.FindAll(TreeScope_Descendants, &condition).ok()?;
        let mut out = Vec::new();
        for index in 0..elements.Length().ok()?.min(512) {
            let Ok(element) = elements.GetElement(index) else {
                continue;
            };
            let Ok(kind) = element.CurrentControlType() else {
                continue;
            };
            let class = element
                .CurrentClassName()
                .ok()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let other_strip = class.ends_with("BarStrip") && class != "QuotaBarStrip";
            if !other_strip
                && ![
                    UIA_ButtonControlTypeId,
                    UIA_ListItemControlTypeId,
                    UIA_EditControlTypeId,
                ]
                .contains(&kind)
            {
                continue;
            }
            let Ok(rect) = element.CurrentBoundingRectangle() else {
                continue;
            };
            if rect.right > rect.left
                && rect.bottom > rect.top
                && rect.bottom > tb.top
                && rect.top < tb.bottom
            {
                out.push(rect);
            }
        }
        // An empty accessibility tree is not evidence that the taskbar is empty.
        (!out.is_empty()).then_some(out)
    }
}

pub fn subtract_bounds_for_instance(
    taskbar: isize,
    metrics: &Metrics,
    occupied: &[(i32, i32)],
) -> Vec<(i32, i32)> {
    lanes(taskbar, metrics)
        .into_iter()
        .flat_map(|lane| subtract(lane, occupied))
        .collect()
}

pub fn revision() -> u64 {
    cache().revision.load(Ordering::Relaxed)
}

/// Pixel intervals in taskbar client coordinates; reserve the notification area.
pub fn lanes(taskbar: isize, metrics: &Metrics) -> Vec<(i32, i32)> {
    let Some((tb, tray)) = taskbar_client_geometry(hw(taskbar)) else {
        return Vec::new();
    };
    let right = tray
        .map(|x| x - metrics.sc(GAP_TRAY))
        .unwrap_or(tb.right - metrics.sc(TASKBAR_RESERVE))
        .min(tb.right);
    let left = tb.left + metrics.sc(4);
    let cache = cache();
    let rectangles = {
        let mut entries = cache.entries.lock().unwrap();
        let entry = entries.entry(taskbar).or_insert_with(|| Entry {
            checked: Instant::now() - Duration::from_secs(5),
            pending: false,
            rectangles: None,
        });
        if !entry.pending && entry.checked.elapsed() >= Duration::from_secs(3) {
            entry.pending = true;
            let _ = cache.sender.send(taskbar);
        }
        entry.rectangles.clone()
    };
    let occupied = match rectangles {
        Some(rectangles) => {
            let origin = unsafe { client_origin_screen(hw(taskbar)) };
            rectangles
                .iter()
                .map(|r| {
                    (
                        r.left - origin.x - metrics.sc(4),
                        r.right - origin.x + metrics.sc(4),
                    )
                })
                .collect()
        }
        None => vec![(tb.left, tb.left + (tb.right - tb.left) * 3 / 4)],
    };
    subtract((left, right), &occupied)
}

pub fn subtract(bounds: (i32, i32), occupied: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut gaps = vec![bounds];
    for &(left, right) in occupied {
        if left >= right {
            continue;
        }
        gaps = gaps
            .into_iter()
            .flat_map(|(a, b)| {
                if right <= a || left >= b {
                    return vec![(a, b)];
                }
                let mut out = Vec::new();
                if a < left {
                    out.push((a, left.min(b)));
                }
                if right < b {
                    out.push((right.max(a), b));
                }
                out
            })
            .collect();
    }
    gaps.into_iter().filter(|(a, b)| b > a).collect()
}

pub fn choose_lane(
    lanes: &[(i32, i32)],
    preferred_x: i32,
    wanted: i32,
    minimum: i32,
) -> Option<(i32, i32)> {
    lanes
        .iter()
        .copied()
        .filter(|(a, b)| b - a >= minimum)
        .min_by_key(|(a, b)| {
            let fit = b - a >= wanted;
            let x = preferred_x.clamp(*a, (b - wanted.min(b - a)).max(*a));
            (!fit, (i64::from(x) - i64::from(preferred_x)).abs())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::geometry::hi;
    #[test]
    #[ignore = "read-only check of the installed grouped bars on physical monitors"]
    fn installed_grouped_bar_is_visible_on_each_display() {
        use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, EnumChildWindows,
            GetClassNameW, IsWindowVisible, GetWindowRect};
        unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
            unsafe {
                let mut class = [0u16;64];
                let length = GetClassNameW(hwnd,&mut class) as usize;
                if String::from_utf16_lossy(&class[..length]) == "QuotaBarStrip" && IsWindowVisible(hwnd).as_bool() {
                    (&mut *(data.0 as *mut Vec<isize>)).push(hi(hwnd));
                }
                BOOL(1)
            }
        }
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let mut windows = Vec::<isize>::new();
            let pointer = LPARAM(&mut windows as *mut _ as isize);
            let _ = EnumWindows(Some(collect),pointer);
            for taskbar in super::super::geometry::enumerate_taskbars(true) {
                let _ = EnumChildWindows(taskbar,Some(collect),pointer);
            }
            windows.sort(); windows.dedup();
            let mut screens = BTreeMap::<String,usize>::new();
            for window in windows {
                let (device,_) = super::super::geometry::monitor_device_of(hw(window)).unwrap();
                let mut rect = RECT::default();
                GetWindowRect(hw(window),&mut rect).unwrap();
                assert!(rect.right>rect.left && rect.bottom>rect.top);
                *screens.entry(device.clone()).or_default()+=1;
                println!("{device}: {rect:?}");
            }
            for (device,_,_) in super::super::geometry::display_dock_bounds() {
                assert_eq!(screens.get(&device),Some(&1),"每屏只应有一条可见额度条：{device}");
            }
        }
    }
    #[test]
    fn free_lanes_exclude_icons_and_other_providers() {
        assert_eq!(
            subtract(
                (-1920, -200),
                &[(-1600, -1200), (-1300, -1000), (-400, 100)]
            ),
            vec![(-1920, -1600), (-1000, -400)]
        );
        let lanes = subtract((0, 1000), &[(400, 600), (800, 900)]);
        assert_eq!(choose_lane(&lanes, 430, 150, 40), Some((600, 800)));
        assert!(choose_lane(&[(0, 20)], 0, 150, 40).is_none());
    }
    #[test]
    #[ignore = "requires a live Windows taskbar"]
    fn system_taskbar_buttons_are_discoverable() {
        let scan = std::thread::spawn(|| unsafe {
            let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
            let taskbars = super::super::geometry::enumerate_taskbars(true);
            assert!(!taskbars.is_empty());
            for taskbar in taskbars {
                let rectangles = super::scan(&automation, hi(taskbar))
                    .expect("taskbar exposes button rectangles");
                assert!(!rectangles.is_empty());
            }
        });
        scan.join().unwrap();
    }

    #[test]
    #[ignore = "read-only verification of the installed quota bar against actual taskbar buttons"]
    fn installed_quota_strips_do_not_cover_taskbar_buttons() {
        let scan = std::thread::spawn(|| unsafe {
            let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
            let mut seen = 0;
            for taskbar in super::super::geometry::enumerate_taskbars(true) {
                let occupied = super::scan(&automation, hi(taskbar)).unwrap();
                let root = automation.ElementFromHandle(taskbar).unwrap();
                let condition = automation.CreateTrueCondition().unwrap();
                let elements = root.FindAll(TreeScope_Descendants, &condition).unwrap();
                for index in 0..elements.Length().unwrap() {
                    let element = elements.GetElement(index).unwrap();
                    if element.CurrentClassName().ok().map(|s| s.to_string()).as_deref() != Some("QuotaBarStrip") { continue; }
                    let hwnd = element.CurrentNativeWindowHandle().unwrap();
                    if !windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool() { continue; }
                    let rect = element.CurrentBoundingRectangle().unwrap();
                    seen += 1;
                    println!("quota window {:?}", rect);
                    for blocked in &occupied {
                        assert!(rect.right <= blocked.left || rect.left >= blocked.right
                            || rect.bottom <= blocked.top || rect.top >= blocked.bottom,
                            "quota rectangle {rect:?} intersects taskbar rectangle {blocked:?}");
                    }
                }
            }
            assert!(seen > 0, "installed quota bar should have a visible taskbar pane");
        });
        scan.join().unwrap();
    }
}
