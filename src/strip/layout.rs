//! 条 / 面板的**内容语义**布局（额度语义，替换 blue 的蓝牙设备语义）：
//! `ProviderSnapshot` → 可绘制的段（条）/文本块（面板），以及 `+n` 尾部折叠
//! 的纯函数（宽度测量在调用方注入，因此折叠边界可以脱离真实 DC 单测）。

use std::sync::OnceLock;

use crate::config::StripStyle;
use crate::model::{ProviderId, ProviderSnapshot, WindowSlot};
use crate::strip::geometry::{MAX_STRIP_ERROR_CHARS, Metrics};
use crate::strip::paint::ellipsize;
use crate::theme::{self, Rgb, Theme, level_color};

// ---------------------------------------------------------------------------
// 条的两种视觉样式（菜单可切，默认 chips）：
// - **chips**（QuotaDock 血条形态，**底改透明**——不画深色容器盒）：每家一段，
//   左侧字母/真 logo tile（跨行垂直居中），右侧多行"标签 | 血条 | 数值"；
//   余额/错误/占位各自独立成段；段间纯间隙（`CHIP_GAP`），不画 ` · ` 文本
//   分隔——`chips_segments`；
// - **text**（平铺数字）：短名 + 三窗口裸数字 + 金额一行式——`text_segment`
//   （现状逻辑冻结不动，回归测试照旧全过）。
// 两者都把全部信息直接放在任务栏上，不用点开面板才看全。

/// chips 盒内左侧的字母图标块：填充 `theme.panel_row_alt`（#2A2A32），
/// 边框 = 状态色（窗口盒 = 最严重窗口分级；余额盒 = `theme::COIN` 蓝；
/// 错误/占位 = 灰），块内白粗字形（厂商首字符 / `¥` / `!`），
/// 绘制时在容器盒内**垂直居中**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileBadge {
    pub glyph: String,
    pub border: Rgb,
}

/// chips 段行区里的一行：
/// - **血条行**（窗口段，`5h`/`周`/`月`）：`label` 左、`bar_frac` 血条居中
///   弹性、`text` 数值右对齐；抓取时间只在详情面板显示；
/// - **纯文本行**（余额/错误/占位段）：`bar_frac = None`，整行排
///   `label` + `text`（label 可为空串）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipRow {
    pub bar_color: Rgb,
    pub label: String,
    pub label_color: Rgb,
    /// 血条填充比例：**万分比**（0..=10000 = 0%..100%，构建期 clamp）——
    /// 整数表示保住 `Eq` 派生（`f64` 无法 `Eq`）；`None` = 该行不填色
    /// （血条行画空轨道 / 纯文本行不画血条）。
    pub bar_frac: Option<u32>,
    pub text: String,
    pub text_color: Rgb,
}

/// chips 段（QuotaDock 血条形态，**底改透明**——不再画深色容器盒，标签/
/// 血条/tile 直接贴任务栏，段间只留 `CHIP_GAP` 间隙）：
/// 字母（或真 logo）tile + 2~3 行行区（窗口段）或单行（余额/错误/占位段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipBadge {
    pub tile: TileBadge,
    pub rows: Vec<ChipRow>,
    /// 贴真供应商 logo 还是画字母：窗口段/余额段 = true（有资产时）；
    /// Error and placeholder states also use provider artwork.
    pub use_logo: bool,
}

impl ChipBadge {
    /// 单行纯文本盒（余额/错误/占位）：行区按内容收窄并整体居中排。
    pub fn single_row(&self) -> bool {
        self.rows.len() == 1 && self.rows[0].bar_frac.is_none()
    }
}

/// 条上一个段的绘制描述。两种形态二选一：
/// - `chip = Some(..)`：chips 盒式段（内容全在 `chip.rows`，`spans` 留空）；
/// - `chip = None`：text 一行式段（色点 + `spans` 片段）。
/// 文案/颜色构建期定死；绘制期只测宽 + 画。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentText {
    pub provider: ProviderId,
    /// 状态色点（text 样式绘制用；chips 样式画容器盒但保留该值，
    /// `+n` 胶囊的严重度着色仍按它判断）。
    pub dot: Rgb,
    pub spans: Vec<(String, Rgb)>,
    pub chip: Option<ChipBadge>,
}

/// 条内容高 → 盒内画几行（纯函数，单测覆盖）：
/// `可用高 ≥ 3×行高 + 上下内边距` 画 3 行，否则保底 2 行
/// （5h/周优先，月窗进面板）。极小高度 / 行高非法也返回 2（下限）。
pub fn rows_for_height(avail: i32, row_h: i32, pad: i32) -> usize {
    if row_h <= 0 {
        return 2;
    }
    if avail >= 3 * row_h + 2 * pad { 3 } else { 2 }
}

/// 已用百分比 → 血条填充比例（**万分比** 0..=10000，clamp；纯函数，单测覆盖）。
/// `7% → 700`、`65% → 6500`；越界钳到 0/10000，NaN 落 0。
pub fn bar_frac(used: f64) -> u32 {
    (used * 100.0).round().clamp(0.0, 10_000.0) as u32
}

/// 按 `store.rows()` 的顺序（= `ProviderId::ALL` 顺序）构建条内容段。
/// - `style` 分支（chips 下"窗口 + 余额"拆成**两个独立盒段**，绘制层用
///   `CHIP_GAP` 纯间隙连接——对齐目标图"余额独立成盒"）；
/// - `avail_height` = 条内容高（chips 据此 + `Metrics` 算盒内行数，见
///   `rows_for_height`；text 样式不使用）。
/// - **多账号**（同 provider 多行）：各段**仍贴厂商 logo**（实机反馈：不要
///   邮箱首字母 tile）——账号辨识交给面板 / 悬浮提示（那里有邮箱）。
pub fn build_segments(
    rows: &[ProviderSnapshot],
    t: &Theme,
    low_threshold: u8,
    style: StripStyle,
    avail_height: i32,
    m: &Metrics,
) -> Vec<SegmentText> {
    let max_rows = rows_for_height(
        avail_height,
        m.st(crate::strip::geometry::CHIP_ROW_H),
        m.st(crate::strip::geometry::CHIP_PAD_Y),
    );
    rows.iter()
        .flat_map(|r| match style {
            StripStyle::Chips => chips_segments(r, t, low_threshold, max_rows),
            StripStyle::Text => vec![text_segment(r, t, low_threshold)],
        })
        .collect()
}

/// text 样式：单段平铺（现状逻辑原样保留）。
/// - **有窗口数据**：短名(fg) + 固定三窗口 ` {值}%`（`{:.0}`，每窗口独立分级），
///   缺失窗口 ` -` 暗色（绝不显示 0）；有 balance 追加 ` {金额}`(fg)；
/// - **金额-only**：短名 + ` {金额}`（不画 `-/-/-`）；
/// - **占位**：短名(fg) + ` --`(fg_dim)；
/// - **error**：`!短名` + ` {截断原因}`（都 fg_dim，原因截 16 字符——完整
///   message 面板里仍有）；色点灰。
fn text_segment(r: &ProviderSnapshot, t: &Theme, low_threshold: u8) -> SegmentText {
    let short = r.provider.short_name().to_string();

    // error 优先：把截断的失败原因直接带上条（面板里才是全文）。
    if r.error.is_some() {
        return SegmentText {
            provider: r.provider,
            dot: t.dot_off,
            spans: vec![
                (format!("!{short}"), t.fg_dim),
                (format!(" {}", error_msg(r)), t.fg_dim),
            ],
            chip: None,
        };
    }

    let has_windows = WindowSlot::ALL.iter().any(|s| r.window(*s).is_some());
    let mut spans: Vec<(String, Rgb)> = Vec::with_capacity(5);
    spans.push((short, t.fg));
    if has_windows {
        // 固定三窗口平铺：5h / 周 / 月（顺序 = WindowSlot::ALL）。
        for slot in WindowSlot::ALL {
            match r.window(slot).and_then(|w| w.pct()) {
                Some(p) => {
                    // 每个窗口单独分级——颜色也是信息。
                    let color = level_color(t, Some(p), low_threshold);
                    spans.push((format!(" {p:.0}%"), color));
                }
                // 窗口缺失 / 有窗口无百分比：` -` 暗色，绝不画成 0。
                None => spans.push((" -".to_string(), t.fg_dim)),
            }
        }
        if let Some(bal) = r.balance {
            spans.push((format!(" {}", bal.format()), t.fg));
        }
    } else if let Some(bal) = r.balance {
        // 金额-only provider：直接 短名 + 金额（不上 -/-/-）。
        spans.push((format!(" {}", bal.format()), t.fg));
    } else {
        // 占位（无数据无错误）：与旧版 ` --` 等价。
        spans.push((" --".to_string(), t.fg_dim));
    }

    SegmentText {
        provider: r.provider,
        dot: level_color(t, r.worst_used().map(|(_, p)| p), low_threshold),
        spans,
        chip: None,
    }
}

/// chips 样式（QuotaDock 血条形态，底改透明）：
/// - **窗口段**：字母/真 logo tile（跨行垂直居中）+ 行区 `5h`/`周`/`月`
///   （缺窗口整行跳过；行数受 `max_rows` 上限）——每行 标签 | 血条 | 数值；
/// - **余额段**：¥ tile（COIN 蓝边）+ 单行 `余额 {金额}`，**独立成段**；
/// - **金额-only**：只有余额段（不画空窗口段）；
/// - **error 段**：`!` 灰 tile + 单行截断原因；**占位段**：字母 tile + `--`。
/// （多账号各段**同样贴厂商 logo**——`use_logo` 恒 true；账号辨识在面板/tooltip。）
fn chips_segments(
    r: &ProviderSnapshot,
    t: &Theme,
    low_threshold: u8,
    max_rows: usize,
) -> Vec<SegmentText> {
    let short = r.provider.short_name();
    // 字母块字形 = 短名首字符（智谱 → '智' 原样）。
    let initial: String = short.chars().take(1).collect();

    // Error text carries the state; artwork keeps the provider recognizable.
    if r.error.is_some() {
        return vec![SegmentText {
            provider: r.provider,
            dot: t.dot_off,
            spans: Vec::new(),
            chip: Some(ChipBadge {
                tile: TileBadge {
                    glyph: "!".into(),
                    border: t.dot_off,
                },
                rows: vec![ChipRow {

                    bar_color: t.dot_off,
                    label: String::new(),
                    label_color: t.fg_dim,
                    bar_frac: None,
                    text: error_msg(r),
                    text_color: t.fg_dim,
                }],
                use_logo: true,
            }),
        }];
    }

    let mut out: Vec<SegmentText> = Vec::with_capacity(2);
    // 窗口行：**窗口缺失（None 或无百分比）→ 整行跳过**——没数据就不画那根
    // 条（Antigravity 只有 5h/周 → 只 2 根；面板的 `--` 行为不受影响）。
    // 行数上限照旧：`max_rows` 只对"存在的行"计数（5h/周优先，月进面板）。
    let mut rows = Vec::with_capacity(3);
    for slot in WindowSlot::ALL {
        if rows.len() >= max_rows {
            break;
        }
        let Some(p) = r.window(slot).and_then(|w| w.pct()) else {
            continue; // 该窗口没数据 → 整行跳过（不造 0、不画空轨道）
        };
        rows.push(ChipRow {

            bar_color: theme::strip_level_color(Some(p), low_threshold),
            label: format!("{}:", slot_tag(slot)),
            label_color: t.fg,
            // 血条填充 = 已用%/100（构建期 clamp）；填充与数值同色。
            bar_frac: Some(bar_frac(p)),
            text: format!("{p:.0}%"),
            text_color: t.fg,
        });
    }
    if !rows.is_empty() {
        let worst = level_color(t, r.worst_used().map(|(_, p)| p), low_threshold);
        out.push(SegmentText {
            provider: r.provider,
            dot: worst,
            spans: Vec::new(),
            // 底改透明后状态色只落在 tile 边框（黑底白字 + level 描边）。
            chip: Some(ChipBadge {
                tile: TileBadge {
                    // glyph 只是**无 logo 资产时的回退**（paint 优先贴真 logo；
                    // 多账号各段同样贴厂商 logo，账号辨识在面板/tooltip）；
                    // clone：`out.is_empty()` 兜底分支还要用原值（rustc 的
                    // 移动检查不认这种值相关可达性）。
                    glyph: initial.clone(),
                    border: worst,
                },
                rows,
                // 窗口段恒贴 logo（多账号亦然，见 build_segments 文档）。
                use_logo: true,
            }),
        });
    }
    if let Some(bal) = r.balance {
        // 余额独立段：`[¥ tile] 余额 ¥109.63`（label dim + 金额 fg）。
        out.push(SegmentText {
            provider: r.provider,
            dot: theme::COIN,
            spans: Vec::new(),
            chip: Some(ChipBadge {
                tile: TileBadge {
                    // glyph = ¥ 币标，仅作**无 logo 资产时的回退**
                    //（paint 有资产时贴厂商 logo，多账号亦然）。
                    glyph: "¥".into(),
                    border: theme::COIN,
                },
                rows: vec![ChipRow {

                    bar_color: t.dot_off,
                    label: "余额".into(),
                    label_color: t.fg_dim,
                    bar_frac: None,
                    text: bal.format(),
                    text_color: t.fg,
                }],
                // 余额段 provider = 该厂商 → 恒贴它的 logo（多账号也贴）。
                use_logo: true,
            }),
        });
    }
    if out.is_empty() {
        // 无任何可用窗口行且无余额（全无窗口，或窗口对象在但都无百分比）
        // → 占位段：字母 tile + `--` 单行（保持原占位行为，不贴 logo）。
        out.push(placeholder_segment(r, t, initial));
    }
    out
}

/// 占位段（无数据无错误，且没有任何可用窗口行/余额）：字母 tile + `--` 单行。
fn placeholder_segment(r: &ProviderSnapshot, t: &Theme, initial: String) -> SegmentText {
    SegmentText {
        provider: r.provider,
        dot: t.dot_off,
        spans: Vec::new(),
        chip: Some(ChipBadge {
            tile: TileBadge {
                glyph: initial,
                border: t.dot_off,
            },
            rows: vec![ChipRow {

                bar_color: t.dot_off,
                label: String::new(),
                label_color: t.fg_dim,
                bar_frac: None,
                text: "--".into(),
                text_color: t.fg_dim,
            }],
            use_logo: true,
        }),
    }
}

/// error 截断原因（两种样式共用）：截 16 字符（完整 message 面板里仍有，
/// 28 字符的 `MAX_ERROR_CHARS`）。text 样式自行补前导空格，chips 直接用。
fn error_msg(r: &ProviderSnapshot) -> String {
    let msg = r
        .message
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(r.error.as_deref())
        .unwrap_or("未知错误");
    ellipsize(msg, MAX_STRIP_ERROR_CHARS)
}

// ---------------------------------------------------------------------------
/// 本地时分（无 chrono 依赖）：`unix − bias×60` 的 UTC 时分，24h 不足补零。
/// `bias_west_min` 用 Windows `ActiveTimeBias` 语义（**西向分钟**，含夏令时
/// 修正，与 `providers/mimo.rs::format_tz` 同款）：中国 -480 → +8h、
/// 印度 -330 → +5:30；正 bias（西半球）相减出负值后 `rem_euclid(24h)`
/// 回绕取模——**只取时分**，换日/负值都不影响输出。
pub(crate) fn local_hm_core(unix: u64, bias_west_min: i32) -> String {
    let local = unix as i64 - i64::from(bias_west_min) * 60;
    let day = local.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, (day % 3600) / 60)
}

/// 当前时区 bias：注册表 `HKLM\...\TimeZoneInformation\ActiveTimeBias`
/// （读法照 `providers/mimo.rs::timezone_header`，winreg 已有依赖）。
/// **进程内读一次缓存**（改时区需重启生效，注明）；读不到（无权限/精简
/// 系统）→ `0` = 按 UTC 显示。
fn active_time_bias() -> i32 {
    static BIAS: OnceLock<i32> = OnceLock::new();
    *BIAS.get_or_init(|| {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"SYSTEM\CurrentControlSet\Control\TimeZoneInformation")
            .ok()
            .and_then(|k| k.get_value::<u32, _>("ActiveTimeBias").ok())
            .map(|b| b as i32)
            .unwrap_or(0)
    })
}

/// `unix` 秒 → 本地 `HH:MM`（外壳：注册表拿 bias 注入 `local_hm_core`）。
pub(crate) fn local_hm(unix: u64) -> String {
    local_hm_core(unix, active_time_bias())
}

/// 条的排版结果：放得下的段 + 折叠数 + 窗口期望宽度。
#[derive(Clone, Debug, Default)]
pub struct StripLayout {
    pub drag_targets: Vec<(ProviderId, windows::Win32::Foundation::RECT)>,
    pub segs: Vec<SegmentText>,
    pub overflow: usize,
    pub width: i32,
}

/// `+n` 尾部折叠（移植 blue 的宽度预算折叠模式，纯函数化便于边界测试）。
///
/// - `widths`：各段像素宽（不含分隔，含色点/内部间距）；
/// - `sep`：段间分隔 ` · ` 的实测宽度；
/// - `budget`：可用宽（已扣除条内左右留白）；
/// - `pill_w`：`+n` 胶囊预留宽（不含前导分隔，分隔按 `sep` 计）。
///
/// 返回 `(可见段数, 折叠段数, 内容总宽)`；总宽已含折叠时的分隔 + 胶囊。
/// **永不越界**：`(可见, 折叠)` 的总宽（+ 条留白）由调用方交给
/// `strip_placement_in_taskbar` 再钳一次。
pub fn fold_segments(widths: &[i32], sep: i32, budget: i32, pill_w: i32) -> (usize, usize, i32) {
    let mut w = 0i32;
    let mut n = 0usize;
    for (i, &seg) in widths.iter().enumerate() {
        let add = if i == 0 { seg } else { sep + seg };
        // 取了第 i 段之后若还会折掉后面的段，就为 "分隔 + 胶囊" 预留——
        // 这样折叠后的总宽永远不超预算（差 1px 也不行）。
        let reserve = if widths.len() - i > 1 { sep + pill_w } else { 0 };
        if w + add + reserve > budget {
            break;
        }
        w += add;
        n += 1;
    }
    let overflow = widths.len() - n;
    if overflow > 0 {
        if n > 0 {
            w += sep;
        }
        w += pill_w;
    }
    (n, overflow, w)
}

// ---------------------------------------------------------------------------
pub fn slot_tag(slot: WindowSlot) -> &'static str {
    match slot {
        WindowSlot::Rolling => "5h",
        WindowSlot::Weekly => "周",
        WindowSlot::Monthly => "月",
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Money, MoneyUnit, UsageWindow};

    fn theme() -> Theme {
        crate::theme::detect()
    }

    #[test]
    fn selected_category_controls_chips_text_and_panel() {
        use crate::config::DisplayMode;
        let mut raw = ProviderSnapshot::new(ProviderId::Mimo);
        raw.monthly = Some(UsageWindow::new_percent(35.0));
        raw.balance = Some(Money { amount: 12.34, unit: MoneyUnit::Cny });
        let t = theme();
        let metrics = Metrics::new(96);
        let balance = DisplayMode::Balance.project(raw.clone());
        let plan = DisplayMode::Plan.project(raw);
        for (row, expected, forbidden) in [(balance, "12.34", "35%"), (plan, "35%", "12.34")] {
            for style in [StripStyle::Chips, StripStyle::Text] {
                let segments = build_segments(&[row.clone()], &t, 10, style, 48, &metrics);
                let text = format!("{segments:?}");
                assert!(text.contains(expected), "{text}");
                assert!(!text.contains(forbidden), "{text}");
            }
            let text = format!("{:?}", super::super::graphite::rows(&[row], 10));
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains(forbidden), "{text}");
        }
    }

    #[test]
    fn fold_exact_fit_keeps_all_segments() {
        // 三段各 100，分隔 10，胶囊 44：总 = 100 + 110 + 100 = 320。
        // 预算正好 320 → 全部放得下，零折叠、零胶囊。
        let (n, ov, w) = fold_segments(&[100, 100, 100], 10, 320, 44);
        assert_eq!((n, ov), (3, 0));
        assert_eq!(w, 320);
    }

    #[test]
    fn fold_one_pixel_short_folds_last_segment() {
        // 差 1px：第 3 段塞不下 → 折叠成 "+1" 胶囊。
        let (n, ov, w) = fold_segments(&[100, 100, 100], 10, 319, 44);
        assert_eq!((n, ov), (2, 1));
        // 210(两段) + 10(分隔) + 44(胶囊) = 264，绝不越界
        assert_eq!(w, 264);
        assert!(w <= 319);
    }

    #[test]
    fn fold_empty_and_single() {
        assert_eq!(fold_segments(&[], 10, 300, 44), (0, 0, 0));
        // 单段且放得下：没有"之后还会折"的段，不预留胶囊
        assert_eq!(fold_segments(&[200], 10, 200, 44), (1, 0, 200));
        // 单段放不下：可见 0，只剩胶囊（无前导分隔）
        assert_eq!(fold_segments(&[200], 10, 199, 44), (0, 1, 44));
    }

    #[test]
    fn fold_first_segment_too_wide_collapses_to_pill() {
        // 第一段就比预算宽：0 段可见，条缩成 "+n" 胶囊仍不越界
        let (n, ov, w) = fold_segments(&[500, 10, 10], 10, 100, 44);
        assert_eq!((n, ov), (0, 3));
        assert_eq!(w, 44);
        assert!(w <= 100);
    }

    #[test]
    fn fold_never_exceeds_budget_across_randomish_inputs() {
        for budget in 50..400 {
            let widths: Vec<i32> = (0..7).map(|i| 30 + i * 17).collect();
            let (_, _, w) = fold_segments(&widths, 12, budget, 44);
            let clamped = w.max(0);
            // 可见段总能放下；折叠时总宽 = 内容 + 分隔 + 胶囊 ≤ 预算
            // （预算极小时内容为纯胶囊 44，可能略超 44 的预算——此时条会被
            //  placement 钳到 MIN_STRIP_WIDTH，这里只校验非负）
            if clamped > 44 {
                assert!(clamped <= budget, "budget={budget} w={clamped}");
            }
        }
    }

    /// chips 下 fold 的 `sep` 换成盒间纯间隙 `CHIP_GAP`（不画 ` · `）：
    /// 边界语义不变——正好放得下零折叠、差 1px 折叠且总宽不越预算。
    #[test]
    fn fold_with_chip_gap_separators_never_exceeds_budget() {
        let sep = crate::strip::geometry::CHIP_GAP; // 96dpi: physical gap equals the design value
        // Three complete segments, including the actual inter-provider gaps.
        let (n, ov, w) = fold_segments(&[120, 120, 120], sep, 120 * 3 + sep * 2, 44);
        assert_eq!((n, ov), (3, 0));
        assert_eq!(w, 120 * 3 + sep * 2);
        // One pixel short folds the last segment, preserving the budget.
        let (n2, ov2, w2) = fold_segments(&[120, 120, 120], sep, 120 * 3 + sep * 2 - 1, 44);
        assert_eq!((n2, ov2), (2, 1));
        assert_eq!(w2, 120 * 2 + sep * 2 + 44);
        assert!(w2 <= 120 * 3 + sep * 2 - 1);
    }

    #[test]
    fn segments_tile_windows_balance_and_placeholder() {
        let t = theme();
        let mut ok = ProviderSnapshot::new(ProviderId::Mimo);
        ok.monthly = Some(UsageWindow::new_percent(63.4));
        ok.balance = Some(Money {
            amount: 12.34,
            unit: MoneyUnit::Cny,
        });
        let miss = ProviderSnapshot::new(ProviderId::OpenCodeGo); // 占位：无数据无错误
        let err = ProviderSnapshot::error(ProviderId::Antigravity, "no-key", "未配置 API Key");
        let segs = build_segments(&[ok, miss, err], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        assert_eq!(segs.len(), 3);

        // 有窗口数据：短名(fg) + 固定三窗口（缺失 = ` -` 暗色，绝不画 0）
        // + 余额(fg)；色点按最严重窗口（63 ≥50 → 黄）。
        assert_eq!(segs[0].dot, t.dot_mid);
        assert_eq!(
            segs[0].spans,
            vec![
                ("MiMo".to_string(), t.fg),
                (" -".to_string(), t.fg_dim),    // 5h 缺失
                (" -".to_string(), t.fg_dim),    // 周 缺失
                (" 63%".to_string(), t.dot_mid), // 月 63（每窗口独立分级）
                (" ¥12.34".to_string(), t.fg),
            ]
        );

        // 无数据无错误（占位）：短名(fg) + ` --`(暗)，色点灰
        assert_eq!(segs[1].dot, t.dot_off);
        assert_eq!(
            segs[1].spans,
            vec![
                ("GO".to_string(), t.fg),
                (" --".to_string(), t.fg_dim),
            ]
        );

        // error：`!短名` + 空格 + 截断原因（都暗），色点灰
        assert_eq!(segs[2].dot, t.dot_off);
        assert_eq!(
            segs[2].spans,
            vec![
                ("!AGY".to_string(), t.fg_dim),
                (" 未配置 API Key".to_string(), t.fg_dim),
            ]
        );
    }

    /// 三窗口全平铺；缺失的那个显示 ` -` 暗色（真实 0% 仍画 ` 0%` 绿）。
    #[test]
    fn all_three_windows_render_and_missing_one_is_dim_dash() {
        let t = theme();
        let mut s = ProviderSnapshot::new(ProviderId::Mimo);
        s.rolling = Some(UsageWindow::new_percent(0.0)); // 真实 0%
        s.monthly = Some(UsageWindow::new_percent(63.0));
        // 周缺失
        let segs = build_segments(&[s], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        let spans = &segs[0].spans;
        assert_eq!(spans.len(), 4); // 短名 + 三窗口
        assert_eq!(spans[0].0, "MiMo");
        assert_eq!(spans[0].1, t.fg);
        assert_eq!(spans[1].0, " 0%");
        assert_eq!(spans[1].1, t.dot_ok);
        assert_eq!(spans[2].0, " -"); // 缺失 ≠ 0
        assert_eq!(spans[2].1, t.fg_dim);
        assert_eq!(spans[3].0, " 63%");
        assert_eq!(spans[3].1, t.dot_mid);
    }

    /// 金额-only provider（DeepSeek 这类）：不上 `-/-/-`，直接 短名 + 金额。
    #[test]
    fn amount_only_provider_skips_window_dashes() {
        let t = theme();
        let mut ds = ProviderSnapshot::new(ProviderId::DeepSeek);
        ds.balance = Some(Money {
            amount: 3.5,
            unit: MoneyUnit::Usd,
        });
        let segs = build_segments(&[ds], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        assert_eq!(segs[0].spans.len(), 2);
        assert_eq!(segs[0].spans[0].0, "DS");
        assert_eq!(segs[0].spans[0].1, t.fg);
        assert_eq!(segs[0].spans[1].0, " $3.50");
        assert_eq!(segs[0].spans[1].1, t.fg);
        // 没有窗口 → 色点灰（worst 不存在）
        assert_eq!(segs[0].dot, t.dot_off);
    }

    /// 错误段带上截断的失败原因（条上 16 字符，完整 message 面板里仍有）。
    #[test]
    fn error_segment_carries_truncated_reason() {
        let t = theme();
        let long = "请求超时：连接额度服务失败（timeout after 20s）";
        let err = ProviderSnapshot::error(ProviderId::DeepSeek, "http", long);
        let segs = build_segments(&[err], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        assert_eq!(segs[0].spans.len(), 2);
        assert_eq!(segs[0].spans[0].0, "!DS");
        assert_eq!(segs[0].spans[0].1, t.fg_dim);
        let reason = &segs[0].spans[1].0;
        assert!(reason.starts_with(' '));
        // 前导空格 + 截到 16 字符（末位是省略号）
        assert!(reason.chars().count() <= 1 + MAX_STRIP_ERROR_CHARS);
        assert!(reason.ends_with('…'));
        assert_eq!(segs[0].spans[1].1, t.fg_dim);
        assert_eq!(segs[0].dot, t.dot_off);
    }

    /// 颜色也是信息：每个窗口数字按**自己**的 used 分级着色；
    /// 色点仍按最严重窗口。
    #[test]
    fn each_window_number_is_colored_by_its_own_level() {
        let t = theme();
        let mut s = ProviderSnapshot::new(ProviderId::Claude);
        s.rolling = Some(UsageWindow::new_percent(10.0)); // 低已用 → 绿
        s.weekly = Some(UsageWindow::new_percent(60.0)); // 中 → 黄
        s.monthly = Some(UsageWindow::new_percent(95.0)); // 高 → 红
        let segs = build_segments(&[s], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        let spans = &segs[0].spans;
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[1].0, " 10%");
        assert_eq!(spans[1].1, t.dot_ok);
        assert_eq!(spans[2].0, " 60%");
        assert_eq!(spans[2].1, t.dot_mid);
        assert_eq!(spans[3].0, " 95%");
        assert_eq!(spans[3].1, t.dot_low);
        // 色点 = 最严重窗口（95% 剩 5 ≤ 阈值 → 红）
        assert_eq!(segs[0].dot, t.dot_low);
    }

    // ========= chips（QuotaDock 盒式血条形态）=========

    /// 盒内行结构 + **缺窗口整行跳过**：有数据的窗口各一根条（label dim +
    /// 每行独立 level 色 + 血条万分比）；rolling 缺失 → 根本不出 5h 行（不造 0）；
    /// tile 边框 = 最严重窗口；内容全在 `chip.rows`（`spans` 留空）。
    #[test]
    fn chips_box_rows_labels_and_per_window_colors() {
        let t = theme();
        let m = Metrics::new(96);
        let mut s = ProviderSnapshot::new(ProviderId::Mimo);
        s.weekly = Some(UsageWindow::new_percent(7.0));
        s.monthly = Some(UsageWindow::new_percent(65.0));
        // rolling 缺失 → 该行整行跳过
        let segs = build_segments(&[s], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].spans.is_empty());
        let chip = segs[0].chip.as_ref().unwrap();
        // 高度够 3 行，但只有两个窗口有数据 → 只有 2 根条
        assert_eq!(chip.rows.len(), 2);
        assert!(!chip.single_row());
        assert!(
            chip.rows.iter().all(|r| r.label != "5h:"),
            "缺失窗口不出行"
        );
        // 行1 周 7% → 绿，血条 7%
        assert_eq!(chip.rows[0].label, "周:");
        assert_eq!(chip.rows[0].label_color, t.fg);
        assert_eq!(chip.rows[0].bar_frac, Some(700));
        assert_eq!(chip.rows[0].text, "7%");
        assert_eq!(chip.rows[0].text_color, t.fg);
        assert_eq!(chip.rows[0].bar_color, theme::strip_level_color(Some(7.0), 10));
        // 行2 月 65% → 黄，血条 65%
        assert_eq!(chip.rows[1].label, "月:");
        assert_eq!(chip.rows[1].bar_frac, Some(6500));
        assert_eq!(chip.rows[1].text, "65%");
        assert_eq!(chip.rows[1].text_color, t.fg);
        assert_eq!(chip.rows[1].bar_color, theme::strip_level_color(Some(65.0), 10));
        // tile 边框 = level_color(最严重窗口 65%) → 黄
        assert_eq!(chip.tile.border, t.dot_mid);
    }

    /// 两窗口 provider（Antigravity 只有 5h/周）→ 不出月行（高度给足也不出）。
    #[test]
    fn chips_two_window_provider_has_no_month_row() {
        let t = theme();
        let m = Metrics::new(96);
        let mut agy = ProviderSnapshot::new(ProviderId::Antigravity);
        agy.rolling = Some(UsageWindow::new_percent(5.0));
        agy.weekly = Some(UsageWindow::new_percent(41.0));
        let segs = build_segments(&[agy], &t, 10, StripStyle::Chips, 48, &m);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.rows.len(), 2);
        assert_eq!(chip.rows[0].label, "5h:");
        assert_eq!(chip.rows[1].label, "周:");
        assert!(chip.rows.iter().all(|r| r.label != "月:"));
    }

    /// 全三窗口有数据 + 高度充足 → 仍三行（行数上限逻辑照旧）。
    #[test]
    fn chips_all_three_windows_keep_three_rows() {
        let t = theme();
        let m = Metrics::new(96);
        let mut s = ProviderSnapshot::new(ProviderId::Mimo);
        s.rolling = Some(UsageWindow::new_percent(1.0));
        s.weekly = Some(UsageWindow::new_percent(3.0));
        s.monthly = Some(UsageWindow::new_percent(63.0));
        let segs = build_segments(&[s], &t, 10, StripStyle::Chips, 48, &m);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.rows.len(), 3);
        assert_eq!(
            chip.rows
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["5h:", "周:", "月:"]
        );
        assert!(chip.rows.iter().all(|r| r.bar_frac.is_some()));
    }

    /// 单窗口 provider → 只一根条（仍是血条行，不是纯文本单行）。
    #[test]
    fn chips_single_window_renders_one_bar_row() {
        let t = theme();
        let m = Metrics::new(96);
        let mut s = ProviderSnapshot::new(ProviderId::DeepSeek);
        s.monthly = Some(UsageWindow::new_percent(95.0));
        let segs = build_segments(&[s], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 1);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.rows.len(), 1);
        assert_eq!(chip.rows[0].label, "月:");
        assert_eq!(chip.rows[0].bar_frac, Some(9500));
        // 有血条 → single_row()（要求 bar_frac=None）为 false，走血条行布局
        assert!(!chip.single_row());
        assert_eq!(chip.rows[0].text_color, t.fg);
        assert_eq!(chip.rows[0].bar_color, theme::strip_level_color(Some(95.0), 10)); // 95 ≥ 90 → 红
    }

    // ================= 多账号仍贴厂商 logo（实机反馈回退） =================

    /// 构造多账号行（上游契约：provider=AGY + `row_key` 带 24hex +
    /// `account_label` = 该账号 email）。
    fn agy_row(row_key: &str, email: Option<&str>, monthly: f64) -> ProviderSnapshot {
        let mut s = ProviderSnapshot::new(ProviderId::Antigravity);
        s.row_key = row_key.into();
        s.account_label = email.map(|e| e.to_string());
        s.monthly = Some(UsageWindow::new_percent(monthly));
        s
    }

    /// 同 provider 两行（不同 email）→ **两段都继续贴厂商 logo**
    /// （use_logo=true、glyph=短名首字母回退值）；边框/行结构/血条全不变。
    /// 账号辨识交给面板 / 悬浮提示（那里有邮箱）。
    #[test]
    fn chips_multi_account_rows_keep_vendor_logo() {
        let t = theme();
        let m = Metrics::new(96);
        let rows = vec![
            agy_row(
                "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
                Some("ye267@gmail.com"),
                10.0,
            ),
            agy_row(
                "antigravity:bbbbbbbbbbbbbbbbbbbbbbbb",
                Some("jdoe@gmail.com"),
                20.0,
            ),
        ];
        let segs = build_segments(&rows, &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 2);
        let c0 = segs[0].chip.as_ref().unwrap();
        let c1 = segs[1].chip.as_ref().unwrap();
        // 多账号仍贴 logo（不换邮箱首字母）
        assert!(c0.use_logo, "多账号各段仍贴厂商 logo");
        assert!(c1.use_logo);
        assert_eq!(c0.tile.glyph, "A"); // glyph = 短名 AGY 首字母（无资产回退值）
        assert_eq!(c1.tile.glyph, "A");
        // 边框仍按最严重窗口分级（10%/20% → 绿）
        assert_eq!(c0.tile.border, t.dot_ok);
        assert_eq!(c1.tile.border, t.dot_ok);
        // 行结构不变：各一根 月 血条行
        assert_eq!(c0.rows.len(), 1);
        assert_eq!(c0.rows[0].label, "月:");
        assert_eq!(c0.rows[0].bar_frac, Some(1000));
        assert!(!c0.single_row());
    }

    /// 单账号 provider（行数=1，即便带 row_key/email）保持 logo 现状。
    #[test]
    fn chips_single_account_row_keeps_logo() {
        let t = theme();
        let m = Metrics::new(96);
        let row = agy_row(
            "antigravity:983960aba4e3fcf0bd4b2221",
            Some("ye267@gmail.com"),
            33.0,
        );
        let segs = build_segments(&[row], &t, 10, StripStyle::Chips, 48, &m);
        let chip = segs[0].chip.as_ref().unwrap();
        assert!(chip.use_logo, "单账号保持贴 logo");
        assert_eq!(chip.tile.glyph, "A"); // 短名 AGY 首字母（未走邮箱规则）
    }

    /// 有无 email 都不影响 logo 决策（邮箱只进面板/tooltip，不上 tile）。
    #[test]
    fn chips_multi_account_email_presence_does_not_change_logo() {
        let t = theme();
        let m = Metrics::new(96);
        let rows = vec![
            agy_row(
                "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
                Some("ye267@gmail.com"),
                10.0,
            ),
            agy_row("antigravity:cccccccccccccccccccccccc", None, 20.0),
        ];
        let segs = build_segments(&rows, &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 2);
        for s in &segs {
            let chip = s.chip.as_ref().unwrap();
            assert!(chip.use_logo, "无论有无 email 都贴 logo");
            assert_eq!(chip.tile.glyph, "A"); // 恒为短名首字母（回退值）
        }
    }

    /// 多账号下 error 行规则不变：仍是 `!` 灰 tile + 不贴 logo；
    /// 好行照常贴 logo。
    #[test]
    fn chips_error_row_keeps_bang_glyph_under_multi_account() {
        let t = theme();
        let m = Metrics::new(96);
        let good = agy_row(
            "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
            Some("ye267@gmail.com"),
            10.0,
        );
        let mut err = ProviderSnapshot::error(ProviderId::Antigravity, "no-key", "未配置 API Key");
        err.row_key = "antigravity:eeeeeeeeeeeeeeeeeeeeeeee".into();
        err.account_label = Some("coyote@example.com".into());
        let segs = build_segments(&[good, err], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 2);
        // 好行：照常贴厂商 logo、glyph=短名首字母
        let ok_chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(ok_chip.tile.glyph, "A");
        assert!(ok_chip.use_logo);
        // Errors retain provider identity; the message carries the error state.
        let err_chip = segs[1].chip.as_ref().unwrap();
        assert_eq!(err_chip.tile.glyph, "!");
        assert_eq!(err_chip.tile.border, t.dot_off);
        assert!(err_chip.use_logo);
        assert!(err_chip.single_row());
    }

    /// 多账号下余额段照常：glyph 保留 ¥（无资产回退值）、贴 logo、COIN 边框不变。
    #[test]
    fn chips_multi_account_balance_segment_keeps_coin_and_logo() {
        let t = theme();
        let m = Metrics::new(96);
        let mut a = agy_row(
            "antigravity:aaaaaaaaaaaaaaaaaaaaaaaa",
            Some("ye267@gmail.com"),
            10.0,
        );
        a.balance = Some(Money {
            amount: 12.34,
            unit: MoneyUnit::Cny,
        });
        let b = agy_row(
            "antigravity:bbbbbbbbbbbbbbbbbbbbbbbb",
            Some("jdoe@gmail.com"),
            20.0,
        );
        let segs = build_segments(&[a, b], &t, 10, StripStyle::Chips, 48, &m);
        // A 窗口段 + A 余额段 + B 窗口段
        assert_eq!(segs.len(), 3);
        let bal = segs[1].chip.as_ref().unwrap();
        assert_eq!(bal.tile.glyph, "¥"); // 保留 ¥（回退值；有资产时被 logo 覆盖）
        assert_eq!(bal.tile.border, theme::COIN); // COIN 蓝边不变
        assert!(bal.use_logo, "多账号余额段照常贴 logo");
        assert!(bal.single_row());
        assert_eq!(bal.rows[0].label, "余额");
        assert!(segs[0].chip.as_ref().unwrap().use_logo);
        assert!(segs[2].chip.as_ref().unwrap().use_logo);
    }

    /// 条高不足 3 行 → 只画 2 行（5h/周优先，月进面板）。
    #[test]
    fn chips_rows_collapse_to_two_when_height_tight() {
        let t = theme();
        let m = Metrics::new(96);
        let mut s = ProviderSnapshot::new(ProviderId::Mimo);
        s.rolling = Some(UsageWindow::new_percent(10.0));
        s.weekly = Some(UsageWindow::new_percent(20.0));
        s.monthly = Some(UsageWindow::new_percent(65.0));
        // 40px 高（浮动条 96dpi 设计值）< 44 → 2 行
        let segs = build_segments(&[s], &t, 10, StripStyle::Chips, 40, &m);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.rows.len(), 2);
        assert_eq!(chip.rows[0].label, "5h:");
        assert_eq!(chip.rows[1].label, "周:");
        assert!(chip.rows.iter().all(|r| r.label != "月:"));
    }

    /// `rows_for_height` 纯函数边界：正好 3 行 / 差 1px → 2 / 极小保底 2 /
    /// 行高非法防御。
    #[test]
    fn rows_for_height_boundary() {
        let rh = 12;
        let pad = 4; // 3 行需 3*12 + 2*4 = 44
        assert_eq!(rows_for_height(44, rh, pad), 3); // 正好放得下
        assert_eq!(rows_for_height(43, rh, pad), 2); // 差 1px
        assert_eq!(rows_for_height(48, rh, pad), 3); // 充足
        assert_eq!(rows_for_height(8, rh, pad), 2); // 极小 → 保底 2 行
        assert_eq!(rows_for_height(1000, 0, pad), 2); // 行高非法 → 防御
    }

    /// 血条万分比计算与 clamp（0..=10000）。
    #[test]
    fn bar_frac_computes_and_clamps_basis_points() {
        assert_eq!(bar_frac(0.0), 0);
        assert_eq!(bar_frac(7.0), 700);
        assert_eq!(bar_frac(73.0), 7300);
        assert_eq!(bar_frac(100.0), 10_000);
        assert_eq!(bar_frac(-5.0), 0); // 负值钳 0
        assert_eq!(bar_frac(350.0), 10_000); // 越界钳满（100% 全填充）
    }

    /// tile 字形 = short_name 首字符（智谱 → '智' 原样）；边框随状态色。
    #[test]
    fn chips_tile_glyph_is_short_name_initial() {
        let t = theme();
        let m = Metrics::new(96);
        let mut mimo = ProviderSnapshot::new(ProviderId::Mimo);
        mimo.rolling = Some(UsageWindow::new_percent(10.0));
        let mut go = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        go.weekly = Some(UsageWindow::new_percent(10.0));
        let mut zp = ProviderSnapshot::new(ProviderId::GlmZhipu);
        zp.monthly = Some(UsageWindow::new_percent(95.0));
        let segs = build_segments(&[mimo, go, zp], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs[0].chip.as_ref().unwrap().tile.glyph, "M"); // MiMo
        assert_eq!(segs[1].chip.as_ref().unwrap().tile.glyph, "G"); // GO
        let zp_chip = segs[2].chip.as_ref().unwrap();
        assert_eq!(zp_chip.tile.glyph, "智"); // 智谱 → '智' 原样
        assert_eq!(zp_chip.tile.border, t.dot_low); // 95% → 红边
        assert_eq!(segs[2].dot, t.dot_low);
    }

    /// 窗口 + 余额并存 → 窗口盒之后跟**独立**的 ¥ 余额盒（绘制层以
    /// `CHIP_GAP` 纯间隙连接，不画 ` · `——目标图"余额独立成盒"）。
    #[test]
    fn chips_balance_is_its_own_boxed_segment() {
        let t = theme();
        let m = Metrics::new(96);
        let mut go = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        go.rolling = Some(UsageWindow::new_percent(0.0));
        go.weekly = Some(UsageWindow::new_percent(2.0));
        go.monthly = Some(UsageWindow::new_percent(60.0));
        go.balance = Some(Money {
            amount: 18.80,
            unit: MoneyUnit::Cny,
        });
        let segs = build_segments(&[go], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 2); // 窗口盒 + 余额盒
        // 盒1：字母 tile + 三窗口血条行，金额不混入
        let win = segs[0].chip.as_ref().unwrap();
        assert_eq!(win.tile.glyph, "G");
        assert_eq!(win.rows.len(), 3);
        assert!(win.rows.iter().all(|r| !r.text.contains('¥')));
        assert!(win.rows.iter().any(|r| r.bar_frac.is_some()));
        // 盒2：¥ tile（COIN 蓝边）+ 单行 余额 + 金额
        let bal = segs[1].chip.as_ref().unwrap();
        assert_eq!(bal.tile.glyph, "¥");
        assert_eq!(bal.tile.border, theme::COIN);
        assert_eq!(segs[1].dot, theme::COIN);
        assert!(bal.single_row());
        let row = &bal.rows[0];
        assert_eq!(row.label, "余额");
        assert_eq!(row.label_color, t.fg_dim);
        assert_eq!(row.bar_frac, None);
        assert_eq!(row.text, "¥18.80");
        assert_eq!(row.text_color, t.fg);
        assert!(segs[1].spans.is_empty());
    }

    /// money-only（DeepSeek 类）：只有余额盒，不画空窗口盒。
    #[test]
    fn chips_money_only_has_only_coin_box() {
        let t = theme();
        let m = Metrics::new(96);
        let mut ds = ProviderSnapshot::new(ProviderId::DeepSeek);
        ds.balance = Some(Money {
            amount: 109.63,
            unit: MoneyUnit::Cny,
        });
        let segs = build_segments(&[ds], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 1);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.tile.glyph, "¥");
        assert_eq!(chip.tile.border, theme::COIN);
        assert!(chip.single_row());
        assert_eq!(chip.rows[0].text, "¥109.63");
        // 没有任何窗口行
        assert!(chip.rows.iter().all(|r| !r.label.contains("5h")));
    }

    /// error 盒：灰边 `!` tile + 单行截断原因（`!` 由 tile 承载，不画 !短名）。
    #[test]
    fn chips_error_box_shows_bang_tile_with_reason() {
        let t = theme();
        let m = Metrics::new(96);
        let err = ProviderSnapshot::error(
            ProviderId::DeepSeek,
            "http",
            "连接额度服务超时（timeout after 20s）",
        );
        let segs = build_segments(&[err], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 1);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.tile.glyph, "!");
        assert_eq!(chip.tile.border, t.dot_off);
        assert_eq!(segs[0].dot, t.dot_off);
        assert!(chip.single_row());
        assert!(segs[0].spans.is_empty());
        let row = &chip.rows[0];
        assert_eq!(row.label, ""); // 无标签，整行只有原因
        assert_eq!(row.bar_frac, None);
        assert!(row.text.chars().count() <= MAX_STRIP_ERROR_CHARS);
        assert!(row.text.ends_with('…'));
        assert_eq!(row.text_color, t.fg_dim);
    }

    /// 占位盒（无数据无错误）：字母 tile（保留身份）+ 单行 `--`。
    #[test]
    fn chips_placeholder_keeps_tile_and_dash_box() {
        let t = theme();
        let m = Metrics::new(96);
        let ph = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        let segs = build_segments(&[ph], &t, 10, StripStyle::Chips, 48, &m);
        assert_eq!(segs.len(), 1);
        let chip = segs[0].chip.as_ref().unwrap();
        assert_eq!(chip.tile.glyph, "G");
        assert_eq!(chip.tile.border, t.dot_off);
        assert_eq!(segs[0].dot, t.dot_off);
        assert!(chip.single_row());
        assert_eq!(chip.rows[0].text, "--");
        assert_eq!(chip.rows[0].text_color, t.fg_dim);
    }

    // ================= 更新时间（fetched_at → 本地 HH:MM） =================

    /// 本地时分换算（bias 西向分钟语义）：0 点 / 23:59 / +8 时区 / 半小时区。
    #[test]
    fn local_hm_core_converts_bias_to_local_hm() {
        // UTC 本体
        assert_eq!(local_hm_core(0, 0), "00:00");
        assert_eq!(local_hm_core(86_399, 0), "23:59");
        // 中国 bias=-480 → +8h
        assert_eq!(local_hm_core(0, -480), "08:00");
        // 印度 bias=-330 → +5:30（半小时区）
        assert_eq!(local_hm_core(0, -330), "05:30");
        assert_eq!(local_hm_core(1_800, -330), "06:00"); // 00:30 UTC + 5:30
    }

    /// 正 bias（西半球）：负秒 `rem_euclid` 回绕取模到前一日。
    #[test]
    fn local_hm_core_positive_bias_wraps_modulo_day() {
        // 00:00 UTC − 5h → 前日 19:00（bias=+300 是 Windows 西向语义 UTC-5）
        assert_eq!(local_hm_core(0, 300), "19:00");
        assert_eq!(local_hm_core(7_200, 300), "21:00"); // 02:00 − 5h
        assert_eq!(local_hm_core(54_000, 600), "05:00"); // 15:00 − 10h
    }

    /// 只取时分：跨多日 / 换日偏移不影响输出（恒为 `HH:MM` 五字符）。
    #[test]
    fn local_hm_core_only_hm_regardless_of_day_count() {
        // 100 天 + 3661s → 当日 01:01（多日整块被取模掉）
        assert_eq!(local_hm_core(86_400 * 100 + 3_661, 0), "01:01");
        // +8 跨日：21:00 UTC → 次日 05:00（换日不影响取时分）
        assert_eq!(local_hm_core(3_600 * 21, -480), "05:00");
        // 输出格式恒定
        assert_eq!(local_hm_core(7, -480).len(), 5);
    }

    /// Update time belongs to the quota value, independent of account metadata.
    #[test]
    fn panel_quota_keeps_own_update_time() {
        let mut s=ProviderSnapshot::new(ProviderId::Mimo);
        s.level=Some("标准".into()); s.account_label=Some("a@x.com".into());
        s.monthly=Some(UsageWindow::new_percent(63.0)); s.fetched_at=1_000_000;
        let rows=super::super::graphite::rows(&[s],10);
        assert_eq!(rows[0].level,"标准"); assert_eq!(rows[0].account,"a@x.com");
        assert_eq!(rows[0].details[0].updated,format!("{} 更新",local_hm(1_000_000)));
    }

    /// fetched_at == 0 → 不加时间 span（容错）。
    #[test]
    fn panel_quota_omits_unknown_fetch_time() {
        let mut s=ProviderSnapshot::new(ProviderId::Mimo);
        s.monthly=Some(UsageWindow::new_percent(63.0)); s.fetched_at=0;
        let rows=super::super::graphite::rows(&[s],10);
        assert!(rows[0].details[0].updated.is_empty());
    }

    #[test]
    fn segment_dot_color_follows_low_threshold() {
        let t = theme();
        let mut low = ProviderSnapshot::new(ProviderId::Mimo);
        low.weekly = Some(UsageWindow::new_percent(95.0)); // 剩 5% ≤ 10 → 红
        let mut mid = ProviderSnapshot::new(ProviderId::Claude);
        mid.weekly = Some(UsageWindow::new_percent(60.0));
        let segs = build_segments(&[low, mid], &t, 10, StripStyle::Text, 48, &Metrics::new(96));
        assert_eq!(segs[0].dot, t.dot_low);
        assert_eq!(segs[1].dot, t.dot_mid);
        // 段间分隔常量就是 ` · `
        assert_eq!(crate::strip::geometry::SEP_TEXT, " · ");
    }

    #[test]
    fn panel_block_lines_match_spec_example() {
        let mut s=ProviderSnapshot::new(ProviderId::Mimo);
        s.rolling=Some(UsageWindow::new_percent(0.0));s.weekly=Some(UsageWindow::new_percent(3.0));
        s.monthly=Some(UsageWindow::new_percent(63.0));
        s.balance=Some(Money{amount:12.34,unit:MoneyUnit::Cny});
        let rows=super::super::graphite::rows(&[s],10);
        let values:Vec<_>=rows[0].details.iter().map(|d|d.value.as_str()).collect();
        assert_eq!(values,vec!["0%","3%","63%","¥12.34"]);
    }

    #[test]
    fn panel_openocode_balance_with_use_balance_suffix() {
        let mut s=ProviderSnapshot::new(ProviderId::OpenCodeGo);
        s.balance=Some(Money{amount:5.0,unit:MoneyUnit::Usd}); s.use_balance=true;
        let rows=super::super::graphite::rows(&[s],10);
        assert_eq!(rows[0].details[0].value,"$5.00");
        assert_eq!(rows[0].details[0].reset,"超量扣余额");
    }

    #[test]
    fn panel_account_level_and_error_lines() {
        let mut s=ProviderSnapshot::new(ProviderId::Antigravity);
        let account=format!("ye267{}15@gmail.com","x".repeat(20));
        s.account_label=Some(account.clone());s.level=Some("标准".into());
        s.weekly=Some(UsageWindow::new_percent(2.0));
        let message="超长错误信息".repeat(20);
        let error=ProviderSnapshot::error(ProviderId::DeepSeek,"http",&message);
        let rows=super::super::graphite::rows(&[s,error],10);
        assert_eq!(rows[0].account,account); assert_eq!(rows[0].level,"标准");
        assert_eq!(rows[1].details[0].value,message);assert!(rows[1].details[0].error);
    }

    #[test]
    fn panel_reset_countdown_uses_injected_now() {
        let now=1_700_000_000i64;
        let mut s=ProviderSnapshot::new(ProviderId::Mimo);
        s.rolling=Some(UsageWindow{percent:Some(50.0),resets_at:Some(now+7200),..Default::default()});
        s.weekly=Some(UsageWindow{percent:Some(1.0),resets_at:Some(now+999),reset_in_sec:Some(120.0),..Default::default()});
        let rows=super::super::graphite::rows_at(&[s],10,now);
        assert_eq!(rows[0].details[0].reset,"2h后重置");
        assert_eq!(rows[0].details[1].reset,"2m后重置");
    }

    #[test]
    fn slot_tags_match_spec_example() {
        assert_eq!(slot_tag(WindowSlot::Rolling), "5h");
        assert_eq!(slot_tag(WindowSlot::Weekly), "周");
        assert_eq!(slot_tag(WindowSlot::Monthly), "月");
    }
}
