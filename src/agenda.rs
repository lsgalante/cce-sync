//! The day's agenda, read from Google Calendar (the bridge, phase 6): which
//! of Google's events fall on a local day, and how they read as lines.
//!
//! Google owns events; the vault only shows them. A daily note holds an
//! empty fenced block whose language is `agenda`, which cce-notes and
//! cce-vault-web draw as [`Agenda::lines`] (from `GET /v1/agenda?date=`)
//! and anything else shows as an empty code block. Claude's `daily_note`
//! tool returns the same lines.
//!
//! No time zone database is needed: the server asks Google for a window
//! around the day with `timeZone` set to the calendar's zone, so every
//! `dateTime` comes back in local time with its offset, and the day is cut
//! by comparing the local `YYYY-MM-DDTHH:MM` text.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The language of the fenced block that stands for the agenda.
pub const BLOCK_LANG: &str = "agenda";

/// What `GET /v1/agenda` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agenda {
    /// The local day, `YYYY-MM-DD`.
    pub date: String,
    /// Whether the server holds a Google sign-in with calendar access.
    #[serde(default)]
    pub connected: bool,
    /// The zone the times are in (the primary calendar's).
    #[serde(default)]
    pub time_zone: String,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// `<calendar id>/<event id>`: what a note's `event:` property names.
    pub id: String,
    pub title: String,
    /// `HH:MM` local, clipped to the day (`00:00` for an event that began
    /// the day before); empty for an all-day event.
    pub start: String,
    /// `HH:MM` local, clipped to the day (`24:00`); empty when all day.
    pub end: String,
    pub all_day: bool,
    /// The calendar's name, when it is not the primary one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub calendar: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
}

impl Agenda {
    /// The agenda as text lines: all-day events first, then by start.
    pub fn lines(&self) -> Vec<String> {
        if !self.connected {
            return vec!["(Google Calendar is not connected)".into()];
        }
        if self.events.is_empty() {
            return vec!["No events".into()];
        }
        self.events
            .iter()
            .map(|e| {
                let when = if e.all_day { "All day".to_string() } else { format!("{}–{}", e.start, e.end) };
                let mut line = format!("{when:<11}  {}", e.title);
                if !e.location.is_empty() {
                    line.push_str(&format!(" · {}", e.location));
                }
                if !e.calendar.is_empty() {
                    line.push_str(&format!(" ({})", e.calendar));
                }
                line
            })
            .collect()
    }

    /// Order: all-day first, then by start, then title.
    pub fn sort(&mut self) {
        self.events.sort_by(|a, b| (!a.all_day, &a.start, &a.title).cmp(&(!b.all_day, &b.start, &b.title)));
    }
}

/// The window to ask Google for (`timeMin`, `timeMax`, UTC): the day
/// before to the day after, so any zone's local day is inside it.
pub fn window(date: &str) -> Option<(String, String)> {
    Some((format!("{}T00:00:00Z", add_days(date, -1)?), format!("{}T00:00:00Z", add_days(date, 2)?)))
}

/// The events of Google's `items` (from one calendar's `events.list`,
/// asked with `timeZone`) that fall on the local day `date`. Cancelled
/// events and ones the user declined are left out.
pub fn events_on(date: &str, calendar_id: &str, calendar_name: &str, items: &[Value]) -> Vec<Event> {
    let Some(next) = add_days(date, 1) else { return Vec::new() };
    let mut out = Vec::new();
    for it in items {
        let s = |k: &str| it.get(k).and_then(Value::as_str).unwrap_or_default();
        if s("status") == "cancelled" {
            continue;
        }
        let declined = it.get("attendees").and_then(Value::as_array).is_some_and(|a| {
            a.iter().any(|p| p.get("self").and_then(Value::as_bool) == Some(true) && p.get("responseStatus").and_then(Value::as_str) == Some("declined"))
        });
        if declined {
            continue;
        }
        let time = |k: &str, f: &str| it.get(k).and_then(|v| v.get(f)).and_then(Value::as_str).map(str::to_string);
        let mut ev = Event {
            id: format!("{calendar_id}/{}", s("id")),
            title: Some(s("summary")).filter(|t| !t.is_empty()).unwrap_or("(no title)").to_string(),
            calendar: calendar_name.to_string(),
            location: s("location").to_string(),
            ..Event::default()
        };
        if let (Some(sd), Some(ed)) = (time("start", "date"), time("end", "date")) {
            // All day; the end date is exclusive.
            if sd.as_str() <= date && date < ed.as_str() {
                ev.all_day = true;
                out.push(ev);
            }
            continue;
        }
        let (Some(st), Some(et)) = (time("start", "dateTime"), time("end", "dateTime")) else { continue };
        if st.len() < 16 || et.len() < 16 {
            continue;
        }
        let (day_start, day_end) = (format!("{date}T00:00"), format!("{next}T00:00"));
        let (st, et) = (&st[..16], &et[..16]);
        if st >= day_end.as_str() || et <= day_start.as_str() || (et == st && !st.starts_with(date)) {
            continue;
        }
        ev.start = if st < day_start.as_str() { "00:00".into() } else { st[11..].to_string() };
        ev.end = if et >= day_end.as_str() { "24:00".into() } else { et[11..].to_string() };
        out.push(ev);
    }
    out
}

/// `date` (`YYYY-MM-DD`) moved by `days`.
pub fn add_days(date: &str, days: i64) -> Option<String> {
    let (y, m, d) = parse_date(date)?;
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) + days);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Whether `date` is a `YYYY-MM-DD` date.
pub fn parse_date_ok(date: &str) -> bool {
    parse_date(date).is_some()
}

pub(crate) fn parse_date(date: &str) -> Option<(i64, u32, u32)> {
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = date[..4].parse().ok()?;
    let m: u32 = date[5..7].parse().ok()?;
    let d: u32 = date[8..10].parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// Milliseconds since the epoch of an RFC 3339 time (`2026-10-13T09:30:00.000Z`,
/// or with an offset), `None` when it is not one.
pub fn rfc3339_ms(t: &str) -> Option<i64> {
    let (y, m, d) = parse_date(t.get(..10)?)?;
    if t.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let hh: i64 = t.get(11..13)?.parse().ok()?;
    let mm: i64 = t.get(14..16)?.parse().ok()?;
    let ss: i64 = t.get(17..19)?.parse().ok()?;
    let mut rest = &t[19..];
    let mut ms = 0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        ms = format!("{:0<3}", &digits[..digits.len().min(3)]).parse().ok()?;
        rest = &r[digits.len()..];
    }
    let offset_min = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 60 + o[4..6].parse::<i64>().ok()?)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset_min * 60;
    Some(secs * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dates() {
        assert_eq!(add_days("2026-10-13", 1).unwrap(), "2026-10-14");
        assert_eq!(add_days("2026-12-31", 1).unwrap(), "2027-01-01");
        assert_eq!(add_days("2024-03-01", -1).unwrap(), "2024-02-29");
        assert_eq!(add_days("2026-13-01", 1), None);
        assert_eq!(window("2026-10-13").unwrap(), ("2026-10-12T00:00:00Z".into(), "2026-10-15T00:00:00Z".into()));
        assert_eq!(rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(rfc3339_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(rfc3339_ms("2026-10-13"), None);
    }

    #[test]
    fn the_local_day_is_cut_from_local_times() {
        let items = vec![
            json!({"id":"a","summary":"Standup","start":{"dateTime":"2026-10-13T09:00:00+02:00"},"end":{"dateTime":"2026-10-13T09:15:00+02:00"}}),
            json!({"id":"b","summary":"Yesterday","start":{"dateTime":"2026-10-12T09:00:00+02:00"},"end":{"dateTime":"2026-10-12T10:00:00+02:00"}}),
            json!({"id":"c","summary":"Overnight","start":{"dateTime":"2026-10-12T23:00:00+02:00"},"end":{"dateTime":"2026-10-13T01:00:00+02:00"}}),
            json!({"id":"d","summary":"Holiday","start":{"date":"2026-10-12"},"end":{"date":"2026-10-14"}}),
            json!({"id":"e","summary":"Tomorrow all day","start":{"date":"2026-10-14"},"end":{"date":"2026-10-15"}}),
            json!({"id":"f","summary":"Cancelled","status":"cancelled","start":{"dateTime":"2026-10-13T12:00:00+02:00"},"end":{"dateTime":"2026-10-13T13:00:00+02:00"}}),
            json!({"id":"g","summary":"Declined","attendees":[{"self":true,"responseStatus":"declined"}],"start":{"dateTime":"2026-10-13T14:00:00+02:00"},"end":{"dateTime":"2026-10-13T15:00:00+02:00"}}),
            json!({"id":"h","summary":"Late","location":"Studio","start":{"dateTime":"2026-10-13T23:30:00+02:00"},"end":{"dateTime":"2026-10-14T00:30:00+02:00"}}),
            json!({"id":"i","summary":"Midnight","start":{"dateTime":"2026-10-14T00:00:00+02:00"},"end":{"dateTime":"2026-10-14T01:00:00+02:00"}}),
        ];
        let mut a = Agenda { date: "2026-10-13".into(), connected: true, time_zone: "Europe/Berlin".into(), events: events_on("2026-10-13", "primary", "", &items) };
        a.sort();
        let got: Vec<(&str, &str, &str)> = a.events.iter().map(|e| (e.title.as_str(), e.start.as_str(), e.end.as_str())).collect();
        assert_eq!(got, vec![("Holiday", "", ""), ("Overnight", "00:00", "01:00"), ("Standup", "09:00", "09:15"), ("Late", "23:30", "24:00")]);
        assert_eq!(a.events[2].id, "primary/a");
        assert_eq!(
            a.lines(),
            vec!["All day      Holiday", "00:00–01:00  Overnight", "09:00–09:15  Standup", "23:30–24:00  Late · Studio"]
        );
        assert_eq!(Agenda::default().lines(), vec!["(Google Calendar is not connected)"]);
    }
}
