// 简单 5 字段 cron 表达式解析器。
// Simple 5-field cron expression parser.
//
// 字段顺序：分(0-59) 时(0-23) 日(1-31) 月(1-12) 周(0-6, 0=周日)
// 支持：
//   *           任意值
//   n           具体值
//   a-b         范围
//   a,b,c       枚举
//   */n         步长
//   a-b/n       范围+步长
//
// 不支持：? L W # 等扩展语法（标准 Unix cron 子集）。

use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

#[derive(Debug, Clone)]
pub struct CronExpr {
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
    pub fn parse(expr: &str) -> Result<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            bail!("cron 表达式必须有 5 个字段（分 时 日 月 周），got {}", parts.len());
        }
        Ok(CronExpr {
            minutes: parse_field(parts[0], 0, 59)?,
            hours: parse_field(parts[1], 0, 23)?,
            days: parse_field(parts[2], 1, 31)?,
            months: parse_field(parts[3], 1, 12)?,
            weekdays: parse_field(parts[4], 0, 6)?,
        })
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
    pub fn next_after(&self, after: &DateTime<Local>) -> Option<DateTime<Local>> {
        let mut dt = after.clone() + chrono::Duration::minutes(1);
        // 最多向前查找 366 天，避免无限循环。
        for _ in 0..(366 * 24 * 60) {
            // 重置秒/纳秒到 0。
            dt = dt.with_second(0)?.with_nanosecond(0)?;

            let month = dt.month() as u8;
            if !self.months.matches(month) {
                // 跳到下个月 1 号 00:00。
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
                dt = dt.with_hour(0)?.with_minute(0)?;
                continue;
            }

            let hour = dt.hour() as u8;
            if !self.hours.matches(hour) {
                dt = dt + chrono::Duration::hours(1);
                dt = dt.with_minute(0)?;
                continue;
            }

            let minute = dt.minute() as u8;
            if self.minutes.matches(minute) {
                return Some(dt);
            }

            // 跳到下一个允许的分钟。
            let next_min = self.minutes.next(minute + 1);
            if next_min > minute {
                dt = dt.with_minute(next_min as u32)?;
            } else {
                // 下一小时。
                dt = dt + chrono::Duration::hours(1);
                dt = dt.with_minute(0)?;
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
    fn invalid_field_count() {
        assert!(CronExpr::parse("* * *").is_err());
    }
}
