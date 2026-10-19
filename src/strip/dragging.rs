//! Drag one provider without activating windows or dragging its neighbours.
use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST;

#[derive(Clone)]
pub(super) struct DragState {
    pub(super) source: isize,
    pub(super) provider: ProviderId,
    pub(super) start: POINT,
    pub(super) grab: (i32, i32),
    pub(super) active: bool,
    pub(super) preview: isize,
}

fn cursor() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point).ok()?;
    }
    Some(point)
}

pub(super) fn exceeded_threshold(start: POINT, current: POINT, dpi: i32) -> bool {
    let threshold = Metrics::with_text_pct(dpi, 100).sc(6).max(4);
    (i64::from(current.x) - i64::from(start.x)).abs() >= i64::from(threshold)
        || (i64::from(current.y) - i64::from(start.y)).abs() >= i64::from(threshold)
}

impl App {
    fn drop_destination(&self, point: POINT) -> Option<(isize,String)> {
        self.available_taskbars().into_iter().find_map(|taskbar| {
            let rect = geometry::taskbar_screen_rect(taskbar)?;
            let margin = if self.monitor_anchors.lock().unwrap().contains(&hi(taskbar)) { 150 } else { 20 };
            if point.x < rect.left || point.x >= rect.right || point.y < rect.top-margin || point.y >= rect.bottom+20 { return None; }
            geometry::monitor_device_of(taskbar).map(|(device,_)| (hi(taskbar),device))
        })
    }
    fn provider_at_drop(&self, point: POINT, device: &str) -> Option<ProviderId> {
        let insts = self.instances.lock().unwrap();
        insts.iter().filter(|i| !i.drag_preview && geometry::monitor_device_of(hw(i.taskbar))
            .is_some_and(|(name,_)| name == device)).find_map(|inst| {
            let mut rect = RECT::default();
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hw(inst.hwnd), &mut rect).ok()?; }
            if point.x < rect.left || point.x >= rect.right || point.y < rect.top || point.y >= rect.bottom { return None; }
            let x = point.x - rect.left;
            inst.layout.drag_targets.iter().min_by_key(|(_,target)| (x-(target.left+target.right)/2).abs())
                .map(|(provider,_)| *provider)
        })
    }

    pub(super) fn drag_start(&self, hwnd: HWND, lparam: LPARAM) {
        let local = POINT {
            x: (lparam.0 as u16 as i16) as i32,
            y: ((lparam.0 >> 16) as u16 as i16) as i32,
        };
        let hit = {
            let insts = self.instances.lock().unwrap();
            insts
                .iter()
                .find(|i| i.hwnd == hi(hwnd) && !i.drag_preview)
                .and_then(|inst| {
                    inst.layout
                        .drag_targets
                        .iter()
                        .find(|(_, rect)| {
                            local.x >= rect.left
                                && local.x < rect.right
                                && local.y >= rect.top
                                && local.y < rect.bottom
                        })
                        .map(|(provider, rect)| (*provider, *rect, inst.dpi))
                })
        };
        let Some((provider, rect, dpi)) = hit else {
            return;
        };
        let Some(start) = cursor() else {
            return;
        };
        let m = self.metrics(dpi);
        *self.drag.lock().unwrap() = Some(DragState {
            source: hi(hwnd),
            provider,
            start,
            grab: (
                local.x - rect.left + m.st(MARGIN_X) + m.st(CHIP_PAD_X),
                m.sc(STRIP_HEIGHT) / 2,
            ),
            active: false,
            preview: 0,
        });
        unsafe {
            let _ = SetCapture(hwnd);
        }
    }

    pub(super) fn drag_move(&self, hwnd: HWND) {
        if unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } < 0 {
            self.drag_finish(false);
            return;
        }
        let Some(point) = cursor() else {
            return;
        };
        self.drag_move_at(hwnd, point);
    }

    fn drag_move_at(&self, hwnd: HWND, point: POINT) {
        let state = self.drag.lock().unwrap().clone();
        let Some(mut state) = state.filter(|d| d.source == hi(hwnd)) else {
            return;
        };
        if !state.active {
            let taskbar_dpi = {
                let insts = self.instances.lock().unwrap();
                insts
                    .iter()
                    .find(|i| i.hwnd == state.source)
                    .map(|i| (i.taskbar, i.dpi))
            };
            let Some((taskbar, dpi)) = taskbar_dpi else {
                self.drag_finish(false);
                return;
            };
            if !exceeded_threshold(state.start, point, dpi) {
                return;
            }
            let Some(mut preview) = self.create_strip_instance(taskbar) else {
                self.drag_finish(false);
                return;
            };
            preview.provider = Some(state.provider);
            preview.drag_preview = true;
            let mut content = self.content();
            content.rows.retain(|r| r.provider == state.provider);
            content.any_enabled = !content.rows.is_empty();
            content.any_content = content
                .rows
                .iter()
                .any(|r| r.has_data() || r.error.is_some());
            let m = self.metrics(preview.dpi);
            let height = m.sc(STRIP_HEIGHT);
            preview.layout = compute_layout(&preview, &content, &m, height);
            preview.layout_valid = true;
            preview.layout_height = height;
            preview.drag_place = Some((
                point.x - state.grab.0,
                point.y - state.grab.1,
                preview.layout.width,
                height,
            ));
            state.active = true;
            state.preview = preview.hwnd;
            self.instances.lock().unwrap().push(preview);
            *self.drag.lock().unwrap() = Some(state.clone());
            self.hide_panel();
        }
        let place = {
            let mut insts = self.instances.lock().unwrap();
            let Some(preview) = insts.iter_mut().find(|i| i.hwnd == state.preview) else {
                return;
            };
            let m = self.metrics(preview.dpi);
            let place = (
                point.x - state.grab.0,
                point.y - m.sc(STRIP_HEIGHT) / 2,
                preview.layout.width,
                m.sc(STRIP_HEIGHT),
            );
            preview.drag_place = Some(place);
            preview.last_place = place;
            place
        };
        unsafe {
            let _ = SetWindowPos(
                hw(state.preview),
                HWND_TOPMOST,
                place.0,
                place.1,
                place.2,
                place.3,
                SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0),
            );
            let _ = InvalidateRect(hw(state.preview), None, false);
        }
    }

    /// Returns true only for an actual drag; a plain click still toggles details.
    pub(super) fn drag_finish(&self, save: bool) -> bool {
        self.drag_finish_at(save, cursor())
    }

    fn drag_finish_at(&self, save: bool, point: Option<POINT>) -> bool {
        let Some(state) = self.drag.lock().unwrap().take() else {
            return false;
        };
        unsafe {
            if GetCapture() == hw(state.source) {
                let _ = ReleaseCapture();
            }
        }
        let mut preview = {
            let mut insts = self.instances.lock().unwrap();
            insts
                .iter()
                .position(|i| i.hwnd == state.preview && i.drag_preview)
                .map(|index| insts.remove(index))
        };
        if let Some(preview) = preview.as_mut() {
            destroy_instance_window(preview);
        }
        if !state.active {
            return false;
        }
        if !save { self.press_closed.store(true, Ordering::Relaxed); }
        if save {
            if let Some((_, device)) = point.and_then(|point| self.drop_destination(point)) {
                let source_device = self.instances.lock().unwrap().iter().find(|i| i.hwnd == state.source)
                    .and_then(|i| geometry::monitor_device_of(hw(i.taskbar))).map(|(device,_)| device);
                let swap = point.and_then(|point| self.provider_at_drop(point, &device))
                    .filter(|provider| *provider != state.provider);
                self.commit(|settings| {
                    if source_device.as_deref() != Some(device.as_str()) {
                        settings.provider_positions.insert(state.provider.as_str().into(),
                            crate::config::ProviderPosition { device, x_fraction: 0.0 });
                    }
                    if let Some(other) = swap { settings.swap_providers(state.provider,other); }
                });
            } else {
                self.show_balloon("未移动", "请拖到另一个供应商图标，或目标显示器的额度条/任务栏上", false);
            }
        }
        self.sync_instances();
        self.heal_all();
        let content = self.content();
        self.refresh_layouts(true, &content);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "native drag session fixture; uses isolated settings and no network"]
    fn native_drag_preview_moves_only_one_provider_and_saves_its_destination() {
        use crate::model::{Money, MoneyUnit};
        struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Environment {
            fn drop(&mut self) {
                for (key, value) in self.0.drain(..) {
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }
        let root = std::env::temp_dir().canonicalize().unwrap();
        let dir = root.join(format!(
            "quota-bar-drag-test-{}",
            crate::oauth::pkce::random_token(12)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let environment = Environment(
            ["LOCALAPPDATA", "DEEPSEEK_API_KEY", "OPENROUTER_API_KEY"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        unsafe {
            std::env::set_var("LOCALAPPDATA", &dir);
            std::env::set_var("DEEPSEEK_API_KEY", "fixture-not-sent");
            std::env::set_var("OPENROUTER_API_KEY", "fixture-not-sent");
            let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let mut settings = Settings::default();
        settings.providers.clear();
        settings.set_enabled(ProviderId::DeepSeek, true);
        settings.set_enabled(ProviderId::OpenRouter, true);
        settings.save();
        let shared = Arc::new(RwLock::new(settings));
        let store = SnapshotStore::new();
        let mut first = ProviderSnapshot::new(ProviderId::DeepSeek);
        first.balance = Some(Money {
            amount: 1.0,
            unit: MoneyUnit::Cny,
        });
        let mut second = ProviderSnapshot::new(ProviderId::OpenRouter);
        second.balance = Some(Money {
            amount: 2.0,
            unit: MoneyUnit::Usd,
        });
        store.replace_all(vec![first, second]);
        let app = Arc::new(App::new(shared, store, PollHandle::without_worker()));
        assert!(APP.set(app.clone()).is_ok());
        unsafe {
            let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: w!("QuotaBarStrip"),
                ..Default::default()
            };
            assert_ne!(RegisterClassExW(&class), 0);
        }
        let taskbar = find_primary_taskbar().unwrap();
        let source = app.create_strip_instance(hi(taskbar)).unwrap();
        let source_hwnd = source.hwnd;
        app.instances.lock().unwrap().push(source);
        app.refresh_layouts(true, &app.content());
        let start = POINT { x: 200, y: 200 };
        for save in [false, true] {
            // A deterministic fixture viewport keeps both test providers visible,
            // independent of the user's current taskbar vacancy.
            let content = app.content();
            let (width,height,embedded) = {
                let mut instances = app.instances.lock().unwrap();
                let source = instances.iter_mut().find(|i| i.hwnd == source_hwnd).unwrap();
                let metrics = app.metrics(source.dpi);
                source.lane = Some((0,2000));
                let height = metrics.sc(STRIP_HEIGHT);
                source.layout = compute_layout(source,&content,&metrics,height);
                (source.layout.width,height,source.embedded)
            };
            let rect = geometry::taskbar_screen_rect(taskbar).unwrap();
            unsafe {
                let _ = SetWindowPos(hw(source_hwnd),HWND::default(),if embedded { 0 } else {rect.left},
                    if embedded {0} else {rect.top},width,height,
                    SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0|SWP_NOZORDER.0));
                let _ = ShowWindow(hw(source_hwnd),SW_HIDE);
            }
            *app.drag.lock().unwrap() = Some(DragState {
                source: source_hwnd,
                provider: ProviderId::DeepSeek,
                start,
                grab: (20, 20),
                active: false,
                preview: 0,
            });
            app.drag_move_at(hw(source_hwnd), POINT { x: 280, y: 200 });
            let drag = app.drag.lock().unwrap().clone().unwrap();
            assert!(drag.active && drag.preview != 0);
            {
                let instances = app.instances.lock().unwrap();
                let preview = instances.iter().find(|i| i.hwnd == drag.preview).unwrap();
                assert!(!preview.layout.segs.is_empty());
                assert!(
                    preview
                        .layout
                        .segs
                        .iter()
                        .all(|segment| segment.provider == ProviderId::DeepSeek)
                );
            }
            let point = {
                let instances = app.instances.lock().unwrap();
                let source = instances.iter().find(|i| i.hwnd == source_hwnd).unwrap();
                let target = source.layout.drag_targets.iter().find(|(p,_)| *p == ProviderId::OpenRouter).unwrap().1;
                let mut rect = RECT::default();
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hw(source_hwnd),&mut rect).unwrap(); }
                POINT { x: rect.left+(target.left+target.right)/2, y:(rect.top+rect.bottom)/2 }
            };
            assert!(app.drag_finish_at(save, Some(point)));
            assert!(app.drag.lock().unwrap().is_none());
            assert!(!app.instances.lock().unwrap().iter().any(|i| i.drag_preview));
            let persisted = Settings::load();
            if save {
                let instances = app.instances.lock().unwrap();
                assert!(instances.iter().filter(|i| !i.drag_preview).all(|i| i.provider.is_none()));
                assert!(!persisted.provider_positions.contains_key("openrouter"));
                let order = persisted.ordered_providers();
                assert!(order.iter().position(|p| *p == ProviderId::OpenRouter) < order.iter().position(|p| *p == ProviderId::DeepSeek));
                assert!(instances.iter().flat_map(|i| &i.layout.segs).any(|s| s.provider == ProviderId::DeepSeek));
            } else { assert!(persisted.provider_positions.is_empty()); }
        }
        let source_device = geometry::monitor_device_of(taskbar).unwrap().0;
        if let Some(destination) = app.available_taskbars().into_iter().find(|target|
            geometry::monitor_device_of(*target).is_some_and(|(device,_)| device != source_device)) {
            let target_device = geometry::monitor_device_of(destination).unwrap().0;
            let rect = geometry::taskbar_screen_rect(destination).unwrap();
            let point = POINT { x: rect.left+200, y:rect.bottom-30 };
            *app.drag.lock().unwrap() = Some(DragState { source:source_hwnd,provider:ProviderId::DeepSeek,
                start,grab:(20,20),active:false,preview:0 });
            app.drag_move_at(hw(source_hwnd),point);
            assert!(app.drag_finish_at(true,Some(point)));
            assert_eq!(Settings::load().provider_positions["deepseek"].device,target_device);
            let instances = app.instances.lock().unwrap();
            let source = instances.iter().find(|i| i.taskbar == hi(taskbar)).unwrap();
            assert!(source.layout.segs.iter().all(|s| s.provider != ProviderId::DeepSeek));
            let target = instances.iter().find(|i| i.taskbar == hi(destination)).unwrap();
            assert!(target.layout.segs.iter().any(|s| s.provider == ProviderId::DeepSeek));
            assert!(instances.iter().all(|i| !i.drag_preview && i.provider.is_none()));
        }
        assert_eq!(app.available_taskbars().len(),geometry::display_dock_bounds().len(),
            "每个显示器都应具有原生任务栏或备用通知区");
        app.shutting_down.store(true, Ordering::Release);
        app.teardown();
        drop(environment);
        // Only remove the resolved, isolated test directory under the temp root.
        let resolved = dir.canonicalize().unwrap();
        assert!(resolved.starts_with(&root) && resolved != root);
        let _ = std::fs::remove_dir_all(resolved);
    }

    #[test]
    fn drag_threshold_distinguishes_clicks_and_handles_negative_monitors() {
        let start = POINT { x: -1900, y: 1020 };
        assert!(!exceeded_threshold(start, POINT { x: -1896, y: 1023 }, 144));
        assert!(exceeded_threshold(start, POINT { x: -1888, y: 1020 }, 144));
        assert!(exceeded_threshold(start, POINT { x: 50, y: 1020 }, 144));
    }
}
