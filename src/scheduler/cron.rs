// 简单 cron 表达式解析器：标准 5 字段，或带前导“秒”字段的 6 字段形式。
// Simple cron expression parser: standard 5 fields, or 6 fields with a
// leading seconds field.
//
// 5 字段：分(0-59) 时(0-23) 日(1-31) 月(1-12) 周(0-6, 0=周日) —— 秒固定为 0
// 6 字段：秒(0-59) 分 时 日 月 周（Quartz 风格，不含年；指定年份的一次性任务请用 at）
// 5 fields: min hour day month weekday —— seconds pinned to 0
// 6 fields: sec min hour day month weekday (Quartz-style, no year; use `at`
// for one-time tasks pinned to a specific year)
//
// 字段语法 / Field syntax:
//   *           任意值
//   n           具体值
//   a-b         范围
//   a,b,c       枚举
//   */n         步长
//   a-b/n       范围+步长
//
// 不支持：? L W # 等扩展语法（标准 Unix cron 子集）。
//
// 注意：os 调度模式（crontab / schtasks）的心跳粒度为 1 分钟，秒级任务会在
// 到点后的第一次心跳触发（最大误差 1 分钟）。
// Note: the OS scheduling mode (crontab / schtasks) heartbeats once per
// minute, so second-level tasks fire at the first heartbeat at/after the
// target second (up to 1 minute late).

use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

#[derive(Debug, Clone)]
pub struct CronExpr {
    pub seconds: Field,
    pub minutes: Field,
    pub hours: Field,
    pub days: Field,
    pub months: Field,
    pub weekdays: Field,
}

#[derive(Debug, Clone)]
pub struct Field {
    /// 升序排列的允许值集合。
    pub values: Vec<u8>,
}

impl Field {
    pub fn matches(&self, val: u8) -> bool {
        self.values.binary_search(&val).is_ok()
    }

    /// 返回 >= val 的最小值；若没有则返回 values[0]（循环）。
    pub fn next(&self, val: u8) -> u8 {
        match self.values.binary_search(&val) {
            Ok(_) => val,
            Err(i) => {
                if i < self.values.len() {
                    self.values[i]
                } else {
                    self.values[0]
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn min(&self) -> u8 {
        self.values[0]
    }

    #[allow(dead_code)]
    pub fn max(&self) -> u8 {
        *self.values.last().unwrap()
    }
}

impl CronExpr {
    /// 解析 cron 表达式：5 字段（分 时 日 月 周）或 6 字段（秒 分 时 日 月 周）。
    pub fn parse(expr: &str) -> Result<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        match parts.len() {
            // 5 字段：秒固定为 0（与标准 Unix cron 一致，在整分钟触发）。
            5 => Ok(CronExpr {
                seconds: Field { values: vec![0] },
                minutes: parse_field(parts[0], 0, 59)?,
                hours: parse_field(parts[1], 0, 23)?,
                days: parse_field(parts[2], 1, 31)?,
                months: parse_field(parts[3], 1, 12)?,
                weekdays: parse_field(parts[4], 0, 6)?,
            }),
            // 6 字段：前导秒字段。
            6 => Ok(CronExpr {
                seconds: parse_field(parts[0], 0, 59)?,
                minutes: parse_field(parts[1], 0, 59)?,
                hours: parse_field(parts[2], 0, 23)?,
                days: parse_field(parts[3], 1, 31)?,
                months: parse_field(parts[4], 1, 12)?,
                weekdays: parse_field(parts[5], 0, 6)?,
            }),
            n => bail!(
                "cron 表达式必须有 5 个字段（分 时 日 月 周）或 6 个字段（秒 分 时 日 月 周），got {n}"
            ),
        }
    }

    /// 判断在 [since, now] 区间内是否有触发点。
    /// 策略：计算 since 之后的下一次触发时间，若 <= now 则认为已触发。
    pub fn has_triggered_since(&self, since: &DateTime<Local>, now: &DateTime<Local>) -> bool {
        match self.next_after(since) {
            Some(next) => next <= *now,
            None => false,
        }
    }

    /// 计算 after 之后的下一个触发时间（严格 > after）。
    /// 分钟以下粒度：只在分钟匹配的那一分钟内扫描秒字段，
    /// 因此迭代上界与 5 字段形式相同（按分钟步进）。
    pub fn next_after(&self, after: &DateTime<Local>) -> Option<DateTime<Local>> {
        let mut dt = after.clone() + chrono::Duration::seconds(1);
        // 最多向前查找 366 天，避免无限循环。
        for _ in 0..(366 * 24 * 60) {
            // 重置纳秒（秒字段参与匹配，不能清零）。
            dt = dt.with_nanosecond(0)?;

            let month = dt.month() as u8;
            if !self.months.matches(month) {
                // 跳到下个月 1 号 00:00:00。
                dt = next_month_start(&dt)?;
                continue;
            }

            let day = dt.day() as u8;
            let wd = dt.weekday().num_days_from_sunday() as u8;
            // cron 语义：日与周为 OR 关系（标准 vixie cron）。
            // 但为简化，我们用 AND：日和周都要匹配（更常见的用户预期）。
            // 若需要 OR 可在此扩展。
            if !self.days.matches(day) || !self.weekdays.matches(wd) {
                dt = dt + chrono::Duration::days(1);
                dt = dt.with_hour(0)?.with_minute(0)?.with_second(0)?;
                continue;
            }

            let hour = dt.hour() as u8;
            if !self.hours.matches(hour) {
                dt = dt + chrono::Duration::hours(1);
                dt = dt.with_minute(0)?.with_second(0)?;
                continue;
            }

            let minute = dt.minute() as u8;
            if !self.minutes.matches(minute) {
                // 跳到下一个允许的分钟。
                let next_min = self.minutes.next(minute + 1);
                if next_min > minute {
                    dt = dt.with_minute(next_min as u32)?.with_second(0)?;
                } else {
                    // 下一小时。
                    dt = dt + chrono::Duration::hours(1);
                    dt = dt.with_minute(0)?.with_second(0)?;
                }
                continue;
            }

            // 分钟匹配：在该分钟内找 >= 当前秒的最小允许秒。
            // dt 始终严格递增，因此找到的候选必然 > after。
            let sec = dt.second() as u8;
            match self.seconds.values.iter().find(|&&s| s >= sec) {
                Some(&s) => return dt.with_second(s as u32),
                None => {
                    // 本分钟没有可用秒 → 下一分钟 0 秒。
                    dt = dt + chrono::Duration::minutes(1);
                    dt = dt.with_second(0)?;
                    continue;
                }
            }
        }
        None
    }
}

fn next_month_start(dt: &DateTime<Local>) -> Option<DateTime<Local>> {
    let y = dt.year();
    let m = dt.month();
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let nd = chrono::NaiveDate::from_ymd_opt(ny, nm, 1)?;
    let nt = chrono::NaiveTime::from_hms_opt(0, 0, 0)?;
    let ndt = chrono::NaiveDateTime::new(nd, nt);
    Local.from_local_datetime(&ndt).single()
}

fn parse_field(s: &str, min: u8, max: u8) -> Result<Field> {
    let mut values = Vec::new();
    for part in s.split(',') {
        parse_part(part, min, max, &mut values)?;
    }
    values.retain(|&v| v >= min && v <= max);
    values.sort();
    values.dedup();
    if values.is_empty() {
        bail!("field '{s}' resolves to no values in [{min},{max}]");
    }
    Ok(Field { values })
}

fn parse_part(s: &str, min: u8, max: u8, out: &mut Vec<u8>) -> Result<()> {
    let (range_part, step) = if let Some((r, st)) = s.split_once('/') {
        let step: u8 = st.parse().map_err(|_| anyhow::anyhow!("invalid step: {st}"))?;
        if step == 0 { bail!("step must be > 0"); }
        (r, Some(step))
    } else {
        (s, None)
    };

    let (start, end) = if range_part == "*" {
        (min, max)
    } else if let Some((a, b)) = range_part.split_once('-') {
        let a: u8 = a.parse().map_err(|_| anyhow::anyhow!("invalid range start: {a}"))?;
        let b: u8 = b.parse().map_err(|_| anyhow::anyhow!("invalid range end: {b}"))?;
        (a, b)
    } else {
        let v: u8 = range_part.parse().map_err(|_| anyhow::anyhow!("invalid value: {range_part}"))?;
        (v, v)
    };

    if start > end || start < min || end > max {
        bail!("range {start}-{end} out of [{min},{max}]");
    }

    match step {
        None => {
            for v in start..=end { out.push(v); }
        }
        Some(step) => {
            let mut v = start;
            while v <= end {
                out.push(v);
                v += step;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_every_minute() {
        let c = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(c.minutes.values.len(), 60);
        assert_eq!(c.hours.values.len(), 24);
    }

    #[test]
    fn parse_daily_3am() {
        let c = CronExpr::parse("0 3 * * *").unwrap();
        assert_eq!(c.minutes.values, vec![0]);
        assert_eq!(c.hours.values, vec![3]);
    }

    #[test]
    fn parse_step() {
        let c = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(c.minutes.values, vec![0, 15, 30, 45]);
    }

    #[test]
    fn parse_range_step() {
        let c = CronExpr::parse("0 9-17/2 * * *").unwrap();
        assert_eq!(c.hours.values, vec![9, 11, 13, 15, 17]);
    }

    #[test]
    fn parse_list() {
        let c = CronExpr::parse("0 9,12,18 * * *").unwrap();
        assert_eq!(c.hours.values, vec![9, 12, 18]);
    }

    #[test]
    fn next_after_basic() {
        let c = CronExpr::parse("0 3 * * *").unwrap();
        let t = Local.with_ymd_and_hms(2025, 1, 1, 2, 30, 0).unwrap();
        let n = c.next_after(&t).unwrap();
        assert_eq!(n.hour(), 3);
        assert_eq!(n.minute(), 0);
    }

    #[test]
    fn parse_six_fields_with_seconds() {
        let c = CronExpr::parse("*/20 * * * * *").unwrap();
        assert_eq!(c.seconds.values, vec![0, 20, 40]);
        assert_eq!(c.minutes.values.len(), 60);
        // 5 字段形式秒固定为 0。
        let c5 = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(c5.seconds.values, vec![0]);
    }

    #[test]
    fn next_after_seconds_within_minute() {
        let c = CronExpr::parse("*/20 * * * * *").unwrap();
        let t = Local.with_ymd_and_hms(2025, 1, 1, 10, 0, 5).unwrap();
        let n = c.next_after(&t).unwrap();
        assert_eq!((n.hour(), n.minute(), n.second()), (10, 0, 20));
    }

    #[test]
    fn next_after_seconds_rolls_to_next_minute() {
        let c = CronExpr::parse("*/20 * * * * *").unwrap();
        let t = Local.with_ymd_and_hms(2025, 1, 1, 10, 0, 55).unwrap();
        let n = c.next_after(&t).unwrap();
        assert_eq!((n.hour(), n.minute(), n.second()), (10, 1, 0));
    }

    #[test]
    fn next_after_five_field_still_lands_on_second_zero() {
        // 回归：5 字段形式在整分钟（秒=0）触发，即使 after 带着非零秒。
        let c = CronExpr::parse("* * * * *").unwrap();
        let t = Local.with_ymd_and_hms(2025, 1, 1, 10, 0, 30).unwrap();
        let n = c.next_after(&t).unwrap();
        assert_eq!((n.minute(), n.second()), (1, 0));
    }

    #[test]
    fn specific_second_cron() {
        // 每天 09:30:15。
        let c = CronExpr::parse("15 30 9 * * *").unwrap();
        let t = Local.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let n = c.next_after(&t).unwrap();
        assert_eq!((n.hour(), n.minute(), n.second()), (9, 30, 15));
    }

    #[test]
    fn invalid_field_count() {
        assert!(CronExpr::parse("* * *").is_err());
    }
}
