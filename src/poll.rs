//! 轮询：后台线程按启用表抓取各 provider 额度，写入共享快照并打脏标记。
//!
//! 线程模型沿用 bt-battery-bar：一个线程 `recv_timeout` 阻塞等待（空闲零 CPU），
//! 到点或收到 kick（菜单"立即刷新"/设置变更）就跑一轮；结果写 `SnapshotStore`，
//! UI 线程看脏标记重绘。没有 tokio，没有定时器轮转。
//!
//! **多账号多行**：同一 provider 可以有多行（Antigravity 每账号一行，
//! `row_key = "antigravity:{24hex}"`），store 用 `Vec` 整体保序
//! （= 抓取序 = 账号存储序），告警按 `row_key` 各自独立去重。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Settings;
use crate::model::{now_sec, ProviderId, ProviderSnapshot};

/// UI 与轮询线程共享的观测结果（多账号 = 同 provider 多行，Vec 保序）。
pub struct SnapshotStore {
    snaps: RwLock<Vec<ProviderSnapshot>>,
    dirty: AtomicBool,
    /// 待弹出的托盘气泡（strip 的 1s 定时器取走）。
    alerts: Mutex<Option<String>>,
}

impl SnapshotStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            snaps: RwLock::new(Vec::new()),
            dirty: AtomicBool::new(true),
            alerts: Mutex::new(None),
        })
    }

    /// 整体替换本轮抓取结果：保持抓取序（= 账号存储序），
    /// **同 `row_key()` 去重——后者胜**（值与位置都取后者）。
    pub fn replace_all(&self, snaps: Vec<ProviderSnapshot>) {
        let mut out: Vec<ProviderSnapshot> = Vec::with_capacity(snaps.len());
        for s in snaps {
            let key = s.row_key().to_string();
            if let Some(pos) = out.iter().position(|x| x.row_key() == key.as_str()) {
                out.remove(pos);
            }
            out.push(s);
        }
        *self.snaps.write().unwrap() = out;
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// 面板/条用：按启用顺序遍历 provider，收集 `row_provider()==p` 的**全部**
    /// 行（多账号各自一行，组内保持存储序）；某 provider 一行都没有 →
    /// 追加**一个**占位 `ProviderSnapshot::new(p)`（`row_key` 空 → 归一为
    /// provider key，面板/条行数稳定逻辑保持）。
    pub fn rows(&self, enabled: &[ProviderId]) -> Vec<ProviderSnapshot> {
        let snaps = self.snaps.read().unwrap();
        let mut out: Vec<ProviderSnapshot> = Vec::new();
        for id in enabled {
            let mut found = false;
            for s in snaps.iter().filter(|s| s.row_provider() == *id) {
                out.push(s.clone());
                found = true;
            }
            if !found {
                out.push(ProviderSnapshot::new(*id));
            }
        }
        out
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn push_alert(&self, text: String) {
        *self.alerts.lock().unwrap() = Some(text);
    }

    pub fn take_alert(&self) -> Option<String> {
        self.alerts.lock().unwrap().take()
    }
}

pub struct PollHandle {
    kick_tx: Sender<()>,
}

impl PollHandle {
    #[cfg(test)]
    pub(crate) fn without_worker() -> Self {
        let (kick_tx, _) = channel();
        Self { kick_tx }
    }

    /// 立即唤醒一轮刷新（菜单"立即刷新"/设置变更后调用）。
    pub fn kick(&self) {
        let _ = self.kick_tx.send(());
    }
}

impl Clone for PollHandle {
    fn clone(&self) -> Self {
        Self {
            kick_tx: self.kick_tx.clone(),
        }
    }
}

/// 同一提醒的重发状态（进程内记忆，不持久化）。
struct AlertState {
    /// 上次弹气泡的 unix 秒；0 = 从未弹过（首次立即弹）。
    last_sent: u64,
}

pub fn spawn(
    settings: Arc<RwLock<Settings>>,
    store: Arc<SnapshotStore>,
) -> PollHandle {
    let (kick_tx, kick_rx) = channel::<()>();
    std::thread::Builder::new()
        .name("quota-poll".into())
        .spawn(move || poll_loop(settings, store, kick_rx))
        .expect("spawn poll thread");
    PollHandle { kick_tx }
}

fn poll_loop(
    settings: Arc<RwLock<Settings>>,
    store: Arc<SnapshotStore>,
    kick_rx: Receiver<()>,
) {
    let mut alert_state: BTreeMap<String, AlertState> = BTreeMap::new();
    // 凭据失效提醒去重（键 = row_key；恢复后清除，下次再失效重新提醒）。
    let mut auth_alerted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    loop {
        let interval = settings.read().unwrap().poll_interval();
        // 到点（Timeout）或被 kick（Ok）都跑一轮。
        match kick_rx.recv_timeout(interval) {
            Ok(()) => {
                // 合并 kick 风暴：清空积压的 kick，只跑一轮。
                while kick_rx.try_recv().is_ok() {}
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        run_cycle(&settings, &store, &mut alert_state, &mut auth_alerted);
    }
}

fn run_cycle(
    settings: &Arc<RwLock<Settings>>,
    store: &Arc<SnapshotStore>,
    alert_state: &mut BTreeMap<String, AlertState>,
    auth_alerted: &mut std::collections::BTreeSet<String>,
) {
    // 每轮先从磁盘重载配置：CLI（setkey/enable/login 后）与菜单的变更即时生效。
    // 菜单侧约定：改完必须 save() 再 kick —— 内存副本会被磁盘覆盖。
    *settings.write().unwrap() = Settings::load();
    // 凭证解析只碰 keyring/env（快），网络抓取在锁外并行跑。
    let providers = crate::providers::registry(&settings.read().unwrap());
    // 显示过滤（**存储即视图** → 条/面板/告警天然一致）：抓取之后、
    // evaluate_alerts + replace_all 之前 —— 账号白名单 + 过期套餐两条规则。
    let cfg = settings.read().unwrap().clone();
    let display = crate::providers::display_list(&cfg); // 启用 ∩ 已添加
    let snaps = filter_display_accounts(crate::providers::run(providers), &cfg);
    let snaps = filter_expired_plans(snaps, &cfg);
    // 凭据失效提醒只看**真正显示**的行（还没添加的 provider 不该催用户配凭据）。
    notify_credential_failures(&snaps, &display, auth_alerted, store);
    if cfg.notify_low {
        evaluate_alerts(&snaps, &cfg, alert_state, store);
    }
    store.replace_all(snaps);
}

/// **套餐已过期**的行不显示（条/面板/告警一致）：上游明确置位 `plan_expired`
/// 才滤（MiMo 的 `expired: true`、StepFun 的 `expired_at` 已过），认不出过期
/// 状态的行一律保留 —— 绝不因为"看不懂"就藏东西。
/// `Settings::hide_expired_plans = false` 时整条规则关闭（照常显示）。
pub fn filter_expired_plans(
    snaps: Vec<ProviderSnapshot>,
    settings: &Settings,
) -> Vec<ProviderSnapshot> {
    if !settings.hide_expired_plans {
        return snaps;
    }
    snaps.into_iter().filter(|s| !s.plan_expired
        || settings.display_mode(s.provider) == crate::config::DisplayMode::Balance).collect()
}

/// 凭据失效提醒（**一次性**气泡）：某账号从"正常/其它错误"变成
/// `auth` / `no-cookie` 时弹一次托盘气泡（Cookie 类 provider 的会话过期是常态，
/// 不提醒用户就只能自己发现）；凭据恢复后清记忆 → 下次再失效会重新提醒。
/// 去重键 = `row_key()`（多账号各自独立）。
///
/// `display` = 实际显示的 provider 集合（启用 ∩ 已添加）：**未添加的 provider 不提醒**
/// （用户还没配，催他"更新凭据"是噪音；这类行只出现在 `quota-bar json` 里）。
fn notify_credential_failures(
    snaps: &[ProviderSnapshot],
    display: &[ProviderId],
    alerted: &mut std::collections::BTreeSet<String>,
    store: &SnapshotStore,
) {
    for snap in snaps {
        let key = snap.row_key().to_string();
        let expired = matches!(snap.error.as_deref(), Some("auth") | Some("no-cookie"))
            && display.contains(&snap.row_provider());
        if !expired {
            alerted.remove(&key); // 恢复 / 未显示 → 清记忆
            continue;
        }
        if !alerted.insert(key) {
            continue; // 本状态已提醒过
        }
        // 文案：`{供应商}{·账号} 需要更新凭据` + 上游/指引的前半段
        //（气泡有长度限制，长指引留在面板行里）。
        let who = snap
            .account_label
            .as_deref()
            .map(|e| format!("·{}", e.chars().take(12).collect::<String>()))
            .or_else(|| {
                snap.row_account_id()
                    .map(|id| format!("·{}", &id[..id.len().min(6)]))
            })
            .unwrap_or_default();
        let hint: String = snap
            .message
            .as_deref()
            .unwrap_or("凭据已失效")
            .chars()
            .take(120)
            .collect();
        store.push_alert(format!(
            "{}{who} 需要更新凭据：{hint}",
            snap.provider.display_name()
        ));
    }
}

/// 账号显示过滤（**存储即视图**：条、面板、告警读的是同一份，天然一致）：
/// - `display_accounts == None` → 原样返回（显示全部账号）；
/// - `Some(list)` → **凡是带账号键的行**（`row_account_id().is_some()`：OAuth
///   多账号行 + API key/Cookie 多账号行）按白名单门控（未选中滤除）；
/// - **无账号键的行**（未登录错误行 / 占位）恒保留（登录态信息不该被显示名单抹掉）；
/// - 旧格式白名单（裸 id，只表达 Antigravity 意图）**只作用于 AGY**，其它
///   provider 照常显示（见 `config::should_show_account`）。
pub fn filter_display_accounts(
    snaps: Vec<ProviderSnapshot>,
    settings: &Settings,
) -> Vec<ProviderSnapshot> {
    let filter = settings.display_accounts.clone();
    if filter.is_none() {
        return snaps;
    }
    snaps
        .into_iter()
        .filter(|s| match s.row_account_id() {
            Some(acc) => crate::config::should_show_account(&filter, s.row_provider(), acc),
            None => true, // 无后缀行（错误/占位）恒保留
        })
        .collect()
}

/// 任一窗口"剩余 ≤ 阈值"→ 排队一个气泡提醒（同值不重发，超过 resend_minutes 才重发）。
/// 去重键 = `row_key()`（**String**）：同 provider 多账号各自独立计时/去重，
/// A 账号的低额度不被 B 账号的记录掩盖。
fn evaluate_alerts(
    snaps: &[ProviderSnapshot],
    settings: &Settings,
    alert_state: &mut BTreeMap<String, AlertState>,
    store: &SnapshotStore,
) {
    let threshold = settings.low_threshold_percent.clamp(1, 99) as f64;
    let resend_secs = settings.resend_minutes.max(1) * 60;
    let now = now_sec();
    let mut text: Option<String> = None;

    for snap in snaps {
        if !snap.is_ok() {
            continue;
        }
        let Some((slot, used)) = snap.worst_used() else {
            continue;
        };
        let key = snap.row_key().to_string();
        let remaining = 100.0 - used;
        if remaining > threshold {
            // 恢复正常：清掉记忆，下次再跌回来还能提醒。
            alert_state.remove(&key);
            continue;
        }
        let state = alert_state.entry(key).or_insert(AlertState {
            last_sent: 0,
        });
        // 首次（last_sent=0）立即弹；持续低额度按 resend 周期重弹；
        // 未到周期不重发（同值去重靠周期实现，恢复后条目被移除、再跌破即视为新告警）。
        let due = now.saturating_sub(state.last_sent) >= resend_secs;
        if due {
            state.last_sent = now;
            // 多账号文案区分：provider 名后追加 `·{email 截12字}`（有 account_label 才加）；
            // 邮箱缺失（损坏条目）→ 账号 id 前 6 位兜底，保证多账号告警仍可辨识。
            let who = snap
                .account_label
                .as_deref()
                .map(|e| format!("·{}", e.chars().take(12).collect::<String>()))
                .or_else(|| {
                    snap.row_account_id()
                        .map(|id| format!("·{}", &id[..id.len().min(6)]))
                })
                .unwrap_or_default();
            text = Some(format!(
                "{}{who} {}额度仅剩 {:.0}%（已用 {:.0}%）",
                snap.provider.display_name(),
                slot.label(),
                remaining,
                used,
            ));
        }
    }
    if let Some(t) = text {
        store.push_alert(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Money, MoneyUnit, UsageWindow};

    fn snap_with(provider: ProviderId, monthly_used: f64) -> ProviderSnapshot {
        let mut s = ProviderSnapshot::new(provider);
        s.monthly = Some(UsageWindow::new_percent(monthly_used));
        s
    }

    #[test]
    fn store_rows_fill_unobserved_enabled_with_placeholder() {
        let store = SnapshotStore::new();
        store.replace_all(vec![snap_with(ProviderId::Mimo, 63.0)]);
        let rows = store.rows(&[ProviderId::Mimo, ProviderId::OpenCodeGo]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].monthly.as_ref().unwrap().percent, Some(63.0));
        // OpenCode Go 没观测过 → 占位快照（无窗口、无错误，UI 显示"获取中"）
        assert!(rows[1].rolling.is_none() && rows[1].error.is_none());
        assert!(store.take_dirty());
        assert!(!store.take_dirty());
    }

    #[test]
    fn alerts_fire_on_threshold_and_respect_dedup() {
        let settings = Settings::default(); // notify_low=true, threshold=10, resend=30min
        let store = SnapshotStore::new();
        let mut state = BTreeMap::new();

        // 95% 已用（剩 5% ≤ 10%）→ 提醒
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 95.0)],
            &settings,
            &mut state,
            &store,
        );
        let alert = store.take_alert().expect("应弹低额度提醒");
        assert!(alert.contains("小米 MiMo"));
        assert!(alert.contains("仅剩 5%"));

        // 同值立即再查 → 未到重发周期，不重发
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 95.0)],
            &settings,
            &mut state,
            &store,
        );
        assert!(store.take_alert().is_none());

        // 持续低额度：超过 resend 周期 → 同值也重发（状态按 row_key 字符串索引）
        let probe = snap_with(ProviderId::Mimo, 95.0);
        state
            .get_mut(probe.row_key())
            .unwrap()
            .last_sent = now_sec().saturating_sub(settings.resend_minutes * 60 + 1);
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 95.0)],
            &settings,
            &mut state,
            &store,
        );
        assert!(store.take_alert().is_some());

        // 恢复到 50% → 记忆清除；再跌回 95% → 立即重新提醒
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 50.0)],
            &settings,
            &mut state,
            &store,
        );
        assert!(store.take_alert().is_none());
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 95.0)],
            &settings,
            &mut state,
            &store,
        );
        assert!(store.take_alert().is_some());
    }

    #[test]
    fn alerts_ignore_error_snapshots_and_safe_thresholds() {
        let settings = Settings::default();
        let store = SnapshotStore::new();
        let mut state = BTreeMap::new();
        let err = ProviderSnapshot::error(ProviderId::Mimo, "no-key", "未配置");
        evaluate_alerts(&[err], &settings, &mut state, &store);
        assert!(store.take_alert().is_none());

        // 坏阈值（0/200，手改 config.json）不 panic 且钳制
        let mut bad = Settings::default();
        bad.low_threshold_percent = 0;
        bad.resend_minutes = 0;
        evaluate_alerts(
            &[snap_with(ProviderId::Mimo, 100.0)],
            &bad,
            &mut state,
            &store,
        );
        // 钳到 1：剩 0% ≤ 1% 仍应提醒
        assert!(store.take_alert().is_some());
    }

    #[test]
    fn balance_carrying_snapshot_keeps_money_type() {
        // 金额类数据模型冒烟：与窗口数据并存
        let mut s = snap_with(ProviderId::OpenCodeGo, 63.0);
        s.balance = Some(Money {
            amount: 12.34,
            unit: MoneyUnit::Cny,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: ProviderSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.balance.unwrap().format(), "¥12.34");
    }

    // ================= 多账号多行（row_key 契约） =================

    fn agy_row(row_key: &str, email: Option<&str>, monthly_used: f64) -> ProviderSnapshot {
        let mut s = snap_with(ProviderId::Antigravity, monthly_used);
        s.row_key = row_key.into();
        s.account_label = email.map(|e| e.to_string());
        s
    }

    /// 同 provider 两行都进 rows、顺序保持抓取序；缺行 provider 占位只补一个。
    #[test]
    fn store_rows_keeps_multi_account_rows_and_single_placeholder() {
        let store = SnapshotStore::new();
        store.replace_all(vec![
            agy_row(
                "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
                Some("alpha@example.com"),
                30.0,
            ),
            agy_row(
                "antigravity:bbbbbbbbbbbbbbbbbbbbbbbb",
                Some("bravo@example.com"),
                70.0,
            ),
        ]);
        // enabled 序 = [AGY, Mimo]：AGY 两行按存储序 + Mimo 恰好一个占位
        let rows = store.rows(&[ProviderId::Antigravity, ProviderId::Mimo]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].account_label.as_deref(), Some("alpha@example.com"));
        assert_eq!(rows[1].account_label.as_deref(), Some("bravo@example.com"));
        assert_eq!(rows[0].monthly.as_ref().unwrap().percent, Some(30.0));
        assert_eq!(rows[1].monthly.as_ref().unwrap().percent, Some(70.0));
        assert_eq!(rows[0].row_account_id(), Some("aaaaaaaaaaaaaaaaaaaaaaaa"));
        // Mimo 没观测 → 只补**一个**占位（row_key 空 → 归一 provider key）
        assert!(rows[2].monthly.is_none() && rows[2].error.is_none());
        assert_eq!(rows[2].row_key(), "mimo");
        assert_eq!(store.rows(&[ProviderId::Claude]).len(), 1);
    }

    /// 同 row_key 去重：**后者胜**（值与位置都取后者，其余保持抓取序）。
    #[test]
    fn replace_all_dedupes_by_row_key_latter_wins() {
        let store = SnapshotStore::new();
        let dup = "antigravity:dupdupdupdupdupdupdupdu";
        let old = agy_row(dup, Some("old@example.com"), 10.0);
        let other = snap_with(ProviderId::Mimo, 50.0);
        let newer = agy_row(dup, Some("new@example.com"), 99.0);
        store.replace_all(vec![old, other, newer]);

        // 测试与 store 同模块 → 可直读内部 Vec 验证顺序
        let snaps = store.snaps.read().unwrap();
        assert_eq!(snaps.len(), 2, "同 row_key 去重后剩 2 行");
        assert_eq!(snaps[0].provider, ProviderId::Mimo, "其余行保序");
        assert_eq!(snaps[1].account_label.as_deref(), Some("new@example.com"));
        assert_eq!(snaps[1].monthly.as_ref().unwrap().percent, Some(99.0));
        drop(snaps);

        // rows 视角同样只出一行（取 newer）
        let rows = store.rows(&[ProviderId::Antigravity]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].monthly.as_ref().unwrap().percent, Some(99.0));
    }

    /// 告警去重键 = row_key：同 provider 两个账号各自独立计时/去重，
    /// A 到期重发不影响 B 的未到周期状态；文案带 A 的 email（截12字）。
    #[test]
    fn alerts_track_each_account_row_independently() {
        let settings = Settings::default(); // threshold=10, resend=30min
        let store = SnapshotStore::new();
        let mut state = BTreeMap::new();
        let a = agy_row(
            "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
            Some("alpha@example.com"),
            95.0,
        );
        let b = agy_row(
            "antigravity:bbbbbbbbbbbbbbbbbbbbbbbb",
            Some("bravo@example.com"),
            95.0,
        );

        // 首轮：两行都首次（last_sent=0）到期 → 状态各写一条
        evaluate_alerts(&[a.clone(), b.clone()], &settings, &mut state, &store);
        assert!(store.take_alert().is_some(), "首弹应出气泡");
        assert_eq!(state.len(), 2, "两账号各一条状态（键=row_key）");

        // 未到 resend 周期 → 都不重发
        evaluate_alerts(&[a.clone(), b.clone()], &settings, &mut state, &store);
        assert!(store.take_alert().is_none());

        // 只把 A 的状态拨回到期 → 只弹 A，文案带 A 的 email 截12字
        let key_a = a.row_key().to_string();
        state.get_mut(&key_a).unwrap().last_sent = now_sec()
            .saturating_sub(settings.resend_minutes * 60 + 1);
        evaluate_alerts(&[a.clone(), b.clone()], &settings, &mut state, &store);
        let alert = store.take_alert().expect("A 到期应重发");
        let email12: String = "alpha@example.com".chars().take(12).collect();
        assert!(
            alert.contains(&format!("·{email12}")),
            "文案应带账号后缀：{alert}"
        );
        // B 仍未到周期：状态未被 A 的重发改写
        let key_b = b.row_key().to_string();
        let b_last = state.get(&key_b).unwrap().last_sent;
        assert!(
            now_sec().saturating_sub(b_last) < settings.resend_minutes * 60,
            "B 的计时应保持未到周期"
        );
    }

    /// 邮箱缺失（损坏条目）：告警用账号 id 前 6 位兜底区分；单行无后缀保持旧行为。
    #[test]
    fn alert_falls_back_to_account_id_prefix_without_email() {
        let settings = Settings::default();
        let store = SnapshotStore::new();
        let mut state = BTreeMap::new();
        let broken = agy_row("antigravity:abcdef1234567890abcdef12", None, 95.0);
        evaluate_alerts(&[broken], &settings, &mut state, &store);
        let alert = store.take_alert().expect("首弹应出气泡");
        assert!(alert.contains("·abcdef"), "文案应带 id 前6位：{alert}");

        // 单账号归一行（row_key 空 → 无 ':'后缀）→ 无任何后缀，与旧文案逐字一致
        let plain = agy_row("", None, 95.0);
        evaluate_alerts(&[plain], &settings, &mut state, &store);
        let alert2 = store.take_alert().expect("另一行首弹");
        assert!(!alert2.contains('·'), "无账号信息不应有后缀：{alert2}");
    }

    // ================= 过期套餐隐藏（filter_expired_plans） =================

    /// 过期套餐整行隐去；未过期/状态未知的行一律保留（宁多勿藏）。
    #[test]
    fn expired_plan_does_not_hide_balance_only_provider() {
        let mut settings = Settings::default();
        settings.display_modes.insert("mimo".into(), crate::config::DisplayMode::Balance);
        let mut snap = ProviderSnapshot::new(ProviderId::Mimo);
        snap.plan_expired = true;
        snap.balance = Some(Money { amount: 20.0, unit: MoneyUnit::Cny });
        assert_eq!(filter_expired_plans(vec![snap.clone()], &settings), vec![snap]);
    }

    #[test]
    fn expired_plans_are_hidden_by_default() {
        let mut expired = snap_with(ProviderId::Mimo, 0.0);
        expired.plan_expired = true;
        let fresh = snap_with(ProviderId::OpenCodeGo, 10.0);
        let unknown = snap_with(ProviderId::Antigravity, 20.0); // plan_expired 默认 false
        let input = vec![expired.clone(), fresh.clone(), unknown.clone()];

        // 默认（hide_expired_plans = true）→ 过期行被滤
        let settings = Settings::default();
        assert!(settings.hide_expired_plans, "默认隐藏");
        let out = filter_expired_plans(input.clone(), &settings);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| !s.plan_expired));
        assert_eq!(out[0].provider, ProviderId::OpenCodeGo);
        assert_eq!(out[1].provider, ProviderId::Antigravity);

        // 关掉开关 → 原样返回（顺序/内容逐位一致）
        let mut keep_all = Settings::default();
        keep_all.hide_expired_plans = false;
        assert_eq!(filter_expired_plans(input.clone(), &keep_all), input);

        // 全是过期行 → 空表（条隐藏、托盘保留）
        assert!(filter_expired_plans(vec![expired], &settings).is_empty());
    }

    /// 过期行同时**不参与低额度告警**（过滤在 evaluate_alerts 之前）。
    #[test]
    fn expired_plan_rows_do_not_trigger_low_alerts() {
        let mut expired = snap_with(ProviderId::Mimo, 99.0); // 已用 99% → 本该告警
        expired.plan_expired = true;
        let settings = Settings::default();
        let snaps = filter_expired_plans(vec![expired], &settings);
        let store = SnapshotStore::new();
        let mut state = BTreeMap::new();
        evaluate_alerts(&snaps, &settings, &mut state, &store);
        assert!(store.take_alert().is_none(), "过期套餐不告警");
    }

    // ================= 凭据失效提醒（notify_credential_failures） =================

    /// 首次失效弹一次气泡；同状态不重复；恢复后再失效重新提醒。
    #[test]
    fn credential_failures_notify_once_until_recovery() {
        let store = SnapshotStore::new();
        let mut alerted = std::collections::BTreeSet::new();
        let shown = [ProviderId::Mimo, ProviderId::StepFun];
        let mut row = ProviderSnapshot::error(
            ProviderId::Mimo,
            "auth",
            "平台 Cookie 已失效：右键菜单 →「供应商管理 → 小米 MiMo → 添加账号…」",
        );
        row.row_key = "mimo:abc12345".into();

        // 首次 → 弹一次
        notify_credential_failures(std::slice::from_ref(&row), &shown, &mut alerted, &store);
        let first = store.take_alert().expect("首次失效应弹气泡");
        assert!(first.contains("小米 MiMo"), "{first}");
        assert!(first.contains("供应商管理"), "带上游指引：{first}");
        // 同状态再来一轮 → 不重复
        notify_credential_failures(std::slice::from_ref(&row), &shown, &mut alerted, &store);
        assert!(store.take_alert().is_none(), "同一失效状态不重复提醒");

        // 恢复（同一行的正常快照）→ 清记忆；再次失效 → 重新提醒
        let mut ok = snap_with(ProviderId::Mimo, 5.0);
        ok.row_key = "mimo:abc12345".into();
        notify_credential_failures(&[ok], &shown, &mut alerted, &store);
        assert!(store.take_alert().is_none(), "恢复不弹");
        assert!(alerted.is_empty(), "恢复后清掉记忆");
        notify_credential_failures(std::slice::from_ref(&row), &shown, &mut alerted, &store);
        assert!(store.take_alert().is_some(), "恢复后再失效应重新提醒");

        // 非凭据类错误（http/parse）不提醒
        let mut net = ProviderSnapshot::error(ProviderId::Mimo, "http", "upstream HTTP 500");
        net.row_key = "mimo:abc12345".into();
        let mut fresh_set = std::collections::BTreeSet::new();
        notify_credential_failures(&[net], &shown, &mut fresh_set, &store);
        assert!(store.take_alert().is_none(), "网络类错误不提醒");

        // **未显示**的 provider（启用但没添加）不提醒：用户还没配，催他更新凭据是噪音
        let mut unadded = ProviderSnapshot::error(ProviderId::StepFun, "no-cookie", "未配置");
        unadded.row_key = "stepfun:aaa".into();
        let mut set0 = std::collections::BTreeSet::new();
        notify_credential_failures(&[unadded], &[ProviderId::Mimo], &mut set0, &store);
        assert!(store.take_alert().is_none(), "未添加的 provider 不提醒");
        assert!(set0.is_empty(), "也不进记忆");

        // 多账号各自独立：两条不同 row_key 都进记忆；气泡是**单槽**（后到覆盖
        // 先到，与低额度告警同款语义）——面板里两行都能看到。
        let mut a = ProviderSnapshot::error(ProviderId::StepFun, "no-cookie", "未配置");
        a.row_key = "stepfun:aaa".into();
        let mut b = ProviderSnapshot::error(ProviderId::StepFun, "auth", "会话已失效");
        b.row_key = "stepfun:bbb".into();
        let mut set2 = std::collections::BTreeSet::new();
        notify_credential_failures(&[a, b], &shown, &mut set2, &store);
        assert_eq!(set2.len(), 2, "两个账号各自记一次");
        let last = store.take_alert().expect("至少弹一条");
        assert!(last.contains("会话已失效"), "单槽语义：后到覆盖先到：{last}");
        assert!(store.take_alert().is_none(), "单槽只留一条");
    }

    // ================= 账号显示过滤（filter_display_accounts） =================

    fn filter_input() -> Vec<ProviderSnapshot> {
        vec![
            agy_row(
                "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
                Some("alpha@example.com"),
                10.0,
            ),
            agy_row(
                "antigravity:bbbbbbbbbbbbbbbbbbbbbbbb",
                Some("bravo@example.com"),
                20.0,
            ),
            snap_with(ProviderId::Antigravity, 30.0), // 无后缀行（错误/占位形态）
            snap_with(ProviderId::Mimo, 40.0),
            snap_with(ProviderId::DeepSeek, 50.0),
        ]
    }

    /// None（默认）→ 全部原样通过（顺序、数量、内容逐位一致）。
    #[test]
    fn filter_display_accounts_none_returns_input_unchanged() {
        let settings = Settings::default();
        let input = filter_input();
        let out = filter_display_accounts(input.clone(), &settings);
        assert_eq!(out, input, "None 模式不过滤任何行");
    }

    /// Some(选中) → 只滤未选中的 AGY 账号行；无后缀 AGY 行保留；非 AGY 不动。
    #[test]
    fn filter_display_accounts_some_filters_unselected_agy_only() {
        let mut settings = Settings::default();
        settings.display_accounts = Some(vec!["aaaaaaaaaaaaaaaaaaaaaaaa".into()]);
        let out = filter_display_accounts(filter_input(), &settings);
        // 5 - 1（未选中的 bbb）= 4
        assert_eq!(out.len(), 4);
        let ids: Vec<Option<&str>> = out.iter().map(|s| s.row_account_id()).collect();
        // 选中的 aaa 保留
        assert!(ids.contains(&Some("aaaaaaaaaaaaaaaaaaaaaaaa")));
        // 未选中的 bbb 滤除
        assert!(!ids.contains(&Some("bbbbbbbbbbbbbbbbbbbbbbbb")));
        // 无后缀的 AGY 行（未登录错误/占位形态）恒保留
        assert!(out
            .iter()
            .any(|s| s.row_provider() == ProviderId::Antigravity && s.row_account_id().is_none()));
        // 非 AGY 行（Mimo/DeepSeek）一律不动
        assert!(out.iter().any(|s| s.row_provider() == ProviderId::Mimo));
        assert!(out
            .iter()
            .any(|s| s.row_provider() == ProviderId::DeepSeek));
    }

    /// Some(空) → AGY 账号行全滤；rows() 视角退化为**恰好一个**占位。
    #[test]
    fn filter_display_accounts_empty_some_degrades_to_placeholder_rows() {
        let mut settings = Settings::default();
        settings.display_accounts = Some(Vec::new());
        let out = filter_display_accounts(filter_input(), &settings);
        // 两条账号行被滤；无后缀 AGY + 两个非 AGY 还在 = 3
        assert_eq!(out.len(), 3);
        assert!(out
            .iter()
            .all(|s| s.row_provider() != ProviderId::Antigravity || s.row_account_id().is_none()));

        let store = SnapshotStore::new();
        store.replace_all(out);
        let rows = store.rows(&[ProviderId::Antigravity]);
        // 账号行全滤光 → 只剩 rows() 补的占位（行数稳定）
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row_key(), "antigravity");
        assert!(rows[0].row_account_id().is_none());
    }
}
