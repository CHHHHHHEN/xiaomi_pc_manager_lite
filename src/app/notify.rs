//! 托盘气泡通知的**触发判定**（纯决策，不触碰任何 UI API）。
//!
//! 从 `tray/notify.rs` 按职责切分（修订 1.48 整理）："该不该弹、弹什么文案"
//! 是纯逻辑（可单测、与展示解耦）；真正调用 `Shell_NotifyIconW` 的展示函数
//! 保留在 `tray::notify`。本模块供 `tray::worker`（判定）与 `tray::notify`
//! （文案）共用。

/// 纯决策：性能模式变化且非首次采样时是否需要弹通知。
///
/// 首次采样（last 为 None）不弹：启动时托盘首次拿到状态只是基线，并非用户
/// 操作导致的切换，弹通知会打扰。之后每次变化都视为真实切换（Fn+K/热键/
/// 电池自动切节能/托盘菜单），返回 true 由调用方在窗口隐藏时弹气泡。
pub fn should_notify_perf_change(last: Option<u8>, current: u8) -> bool {
    matches!(last, Some(prev) if prev != current)
}

/// 纯决策：电池养护状态变化且非上次采样时是否需要弹通知。
///
/// 与 `should_notify_perf_change` 语义一致（首次采样不弹、之后每次变化都
/// 视为真实切换）。
pub fn should_notify_care_change(last: Option<bool>, current: bool) -> bool {
    matches!(last, Some(prev) if prev != current)
}

/// 从上一次采样（携带判定时的上限）提取"当前上限对应的上次武装状态"。
///
/// 上限是**跨采样状态的键**：用户中途改上限（80→90、或关闭养护使 limit=100）
/// 后，旧上限下记录的"已武装"对新上限无效——否则改上限后"电池已充至新上限/
/// 已充满"永不弹。上限一致才复用上次状态；上限变化或未取基线返回 None
/// （触发决策的基线守卫，重新记录）。
pub fn armed_state_for_limit(stored: Option<(u8, bool)>, limit: u8) -> Option<bool> {
    match stored {
        Some((armed_at, armed)) if armed_at == limit => Some(armed),
        _ => None,
    }
}

/// 充电达到养护上限的判定（纯函数，便于单元测试）。
///
/// 输入：
/// - `prev_at_limit`：上一次采样是否已处于阈值（`None` = 尚未采到基线，
///   首个采样点不触发，只记录状态——与 perf/care 通知的首采样语义一致）；
/// - `pct`：当前电量百分比（`None` = 未知/未装电池）；
/// - `limit`：养护上限（`100` = 无养护，充满才算）；
/// - `on_ac`：是否交流供电；
/// - `enabled`：设置开关（默认关，不主动打扰）。
///
/// 返回 `(是否触发一次通知, 新的 at_limit 状态)`：
/// - 仅当 `enabled && on_ac && pct` 接近/达到上限（±1% 容差）且上次未达到
///   时触发（`at_limit=true`，此后保持不再触发）；**电量高于上限
///   （`pct > limit`）不触发**——此时电池在回落而非充电到达；
/// - **≥3% 迟滞**：电量回落到 `pct ≤ limit-3` 或断开电源时才重新武装；
/// - 中间带（`limit-2`）保持上一次状态：防止阈值附近波动反复重武装。
///
/// 基线守卫：`prev_at_limit = None`（启动后首个采样）时只记录状态不触发，
/// 否则刚开机电池已在养护上限会被位置性判定误报。
pub fn charge_limit_notification_decision(
    prev_at_limit: Option<bool>,
    pct: Option<u8>,
    limit: u8,
    on_ac: bool,
    enabled: bool,
) -> (bool, Option<bool>) {
    if !enabled || !on_ac {
        return (false, None);
    }
    let Some(pct) = pct else {
        return (false, None);
    };
    // ≥3% 迟滞：显著回落到 limit-3 及以下才重新武装。
    if pct + 3 <= limit {
        return (false, Some(false));
    }
    // ±1% 容差：EC 养护停在上限时 OS 可能报上限-1（本机实测 ~79/80）。
    let at_limit = pct + 1 >= limit;
    // 电量高于上限：非充电到达，不触发（仅记录为已到达状态，回落重武装前
    // 的波动不弹窗）。
    if at_limit && pct > limit {
        return (false, Some(true));
    }
    // 基线守卫：首个采样只记录，不触发。
    let Some(prev_at_limit) = prev_at_limit else {
        return (false, Some(at_limit));
    };
    if at_limit {
        return (!prev_at_limit, Some(true));
    }
    // 中间带（limit-2）：保持上次武装状态，阈值附近波动不反复弹窗。
    (false, Some(prev_at_limit))
}

/// 充电达到上限通知文案（展示层据此弹气泡）。
pub fn charge_limit_notification_text(limit: u8) -> String {
    if limit >= 100 {
        "电池已充满".to_string()
    } else {
        format!("电池已充至 {}% 养护上限", limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 性能模式变化通知的纯决策：
    /// - 首次采样（None → 值）不弹（启动基线，非用户操作）；
    /// - 后续值变化弹；值相同不弹。
    #[test]
    fn test_should_notify_perf_change() {
        assert!(
            !should_notify_perf_change(None, 0x09),
            "first sample must not notify"
        );
        assert!(
            !should_notify_perf_change(Some(0x09), 0x09),
            "no change must not notify"
        );
        assert!(
            should_notify_perf_change(Some(0x09), 0x02),
            "perf change must notify"
        );
        assert!(
            should_notify_perf_change(Some(0x02), 0x04),
            "another perf change must notify"
        );
    }

    /// 电池养护状态变化通知的决策逻辑（与性能模式同语义）。
    #[test]
    fn test_should_notify_care_change() {
        assert!(
            !should_notify_care_change(None, false),
            "first sample must not notify"
        );
        assert!(
            !should_notify_care_change(Some(false), false),
            "no change must not notify"
        );
        assert!(
            should_notify_care_change(Some(false), true),
            "care enabled must notify"
        );
        assert!(
            should_notify_care_change(Some(true), false),
            "care disabled must notify"
        );
    }

    /// 充电达到养护上限的判定全矩阵：
    /// - 开关关闭：不通知、无状态；
    /// - 达到阈值且未在状态：触发 + 进入状态；
    /// - 已处于阈值附近（容忍 79→80 的 OS 报低 1%）：不再触发；
    /// - 断开电源：复位（不触发、不武装、清状态）；
    /// - 显著回落（≥3% 迟滞）→ 重新武装；
    /// - 基线守卫：首采样（None）即使已在上限也只记录状态不触发；
    /// - 电量高于上限：不触发（电池在回落而非充电到达）。
    #[test]
    fn test_charge_limit_notification_decision() {
        // 开关关闭：不通知、无状态。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(80), 80, true, false),
            (false, None)
        );
        // 达到阈值且未在状态：触发 + 进入状态。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(80), 80, true, true),
            (true, Some(true))
        );
        // 已处于阈值附近（容忍 79→80 的 OS 报低 1%）：不再触发。
        assert_eq!(
            charge_limit_notification_decision(Some(true), Some(79), 80, true, true),
            (false, Some(true))
        );
        // 断开电源：复位。
        assert_eq!(
            charge_limit_notification_decision(Some(true), Some(80), 80, false, true),
            (false, None)
        );
        // 显著回落到阈值以下：重新武装。
        assert_eq!(
            charge_limit_notification_decision(Some(true), Some(76), 80, true, true),
            (false, Some(false))
        );
        // 阈值附近但未达到（78 vs 80，+1 容差后 79 < 80）：不触发、不武装。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(78), 80, true, true),
            (false, Some(false))
        );
        // 迟滞中间带（limit-2）：已武装后回落到 78（2% 以内）**保持**武装。
        assert_eq!(
            charge_limit_notification_decision(Some(true), Some(78), 80, true, true),
            (false, Some(true))
        );
        // 电量未知：不触发、无状态。
        assert_eq!(
            charge_limit_notification_decision(Some(false), None, 80, true, true),
            (false, None)
        );
        // 无养护（limit=100）：充到 99 时已算"接近充满"。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(99), 100, true, true),
            (true, Some(true))
        );
        // 基线守卫：首采样（None）即使已在上限也只记录状态不触发。
        assert_eq!(
            charge_limit_notification_decision(None, Some(80), 80, true, true),
            (false, Some(true))
        );
        // 高于上限不触发：电池在回落而非充电到达。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(95), 80, true, true),
            (false, Some(true))
        );
        // 无养护（limit=100）+ 满电：充到 100 时算充满。
        assert_eq!(
            charge_limit_notification_decision(Some(false), Some(100), 100, true, true),
            (true, Some(true))
        );
    }

    /// 武装状态按上限键控：上限一致复用上次状态；上限变化（改上限/关闭
    /// 养护→100）或未取基线返回 None——让决策的基线守卫重新记录。
    #[test]
    fn test_armed_state_for_limit_keys_by_limit() {
        assert_eq!(armed_state_for_limit(Some((80, true)), 80), Some(true));
        assert_eq!(armed_state_for_limit(Some((80, false)), 80), Some(false));
        assert_eq!(armed_state_for_limit(Some((80, true)), 90), None);
        assert_eq!(armed_state_for_limit(Some((80, true)), 100), None);
        assert_eq!(armed_state_for_limit(None, 80), None);
    }

    /// 上限变化的完整决策流：80% 已武装后改上限到 90，恢复充电并在 90% 前
    /// 1% 容差处重新到达——旧上限的武装不得压制新上限的触发。
    #[test]
    fn reached_charge_limit_reaches_new_limit_after_change() {
        let prev = armed_state_for_limit(Some((80, true)), 90);
        assert_eq!(prev, None);
        let (_, base) = charge_limit_notification_decision(prev, Some(60), 90, true, true);
        assert_eq!(base, Some(false));
        assert_eq!(
            charge_limit_notification_decision(base, Some(89), 90, true, true),
            (true, Some(true))
        );
        assert_eq!(
            charge_limit_notification_decision(Some(true), Some(90), 90, true, true),
            (false, Some(true))
        );
    }

    /// 通知文案：养护上限与充满两种情况。
    #[test]
    fn test_charge_limit_notification_text() {
        assert_eq!(
            charge_limit_notification_text(80),
            "电池已充至 80% 养护上限"
        );
        assert_eq!(charge_limit_notification_text(100), "电池已充满");
    }
}
