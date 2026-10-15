//! Spec 909 — which media a capture ran on, as rows of `trace_media`.
//!
//! Two sources, both written by the daemon (`trx64-daemon/src/media_identity.rs`
//! documents the grammar):
//!
//! * the header's `media` object (version 1): one `start` row per medium in the
//!   machine when the trace started, at `cycle_start`;
//! * MARK records (0x01) whose label starts `media/1 `: `in` / `out` when a medium
//!   changed while the trace ran, `power-off` when the machine was switched off
//!   (which ends the trace).
//!
//! A media MARK goes into `trace_media`, not `trace_mark`. A reader built before
//! this module sees it as an ordinary mark.
//!
//! ```sql
//! trace_media (run_id, cycle, slot, action, sha256, format, source_name,
//!              hw_type, mapper_type, banks, crt_name)
//! ```
//! `slot` is `cart` or `drive<unit>` (NULL on a `power-off` row); `action` is
//! `start` | `in` | `out` | `power-off`. Columns a source does not carry are NULL:
//! an `out` row names only the hash, `crt_name` comes from the header only.

use serde_json::Value;

/// The label prefix of a media record.
pub const MEDIA_MARK_PREFIX: &str = "media/1 ";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaRow {
    pub cycle: u64,
    pub slot: Option<String>,
    pub action: String,
    pub sha256: Option<String>,
    pub format: Option<String>,
    pub source_name: Option<String>,
    pub hw_type: Option<i64>,
    pub mapper_type: Option<String>,
    pub banks: Option<i64>,
    pub crt_name: Option<String>,
}

/// The `start` rows of a header's `media` object; none for an absent or unknown version.
pub fn rows_from_header(media: Option<&Value>, cycle_start: u64) -> Vec<MediaRow> {
    let Some(m) = media.filter(|m| m["v"].as_u64() == Some(1)) else { return Vec::new() };
    let row = |slot: String, o: &Value| MediaRow {
        cycle: cycle_start,
        slot: Some(slot),
        action: "start".into(),
        sha256: o["sha256"].as_str().map(String::from),
        format: o["format"].as_str().map(String::from),
        source_name: o["sourceName"].as_str().filter(|s| !s.is_empty()).map(String::from),
        hw_type: o["hwType"].as_i64(),
        mapper_type: o["mapperType"].as_str().map(String::from),
        banks: o["banks"].as_i64(),
        crt_name: o["crtName"].as_str().filter(|s| !s.is_empty()).map(String::from),
    };
    let mut out = Vec::new();
    if m["cart"].is_object() {
        out.push(row("cart".into(), &m["cart"]));
    }
    if let Some(drives) = m["drives"].as_object() {
        let mut units: Vec<(&String, &Value)> = drives.iter().collect();
        units.sort_by_key(|(u, _)| u.parse::<u32>().unwrap_or(u32::MAX));
        for (unit, o) in units {
            out.push(row(format!("drive{unit}"), o));
        }
    }
    out
}

/// A media record's row, or `None` when the label is not one.
pub fn row_from_mark(cycle: u64, label: &str) -> Option<MediaRow> {
    let rest = label.strip_prefix(MEDIA_MARK_PREFIX)?;
    if rest == "power off" {
        return Some(MediaRow { cycle, action: "power-off".into(), ..Default::default() });
    }
    // `name=` is last and keeps its spaces.
    let (head, name) = match rest.find(" name=") {
        Some(i) => (&rest[..i], Some(rest[i + " name=".len()..].to_string())),
        None => (rest, None),
    };
    let mut toks = head.split(' ');
    let slot = toks.next()?.to_string();
    let action = toks.next()?;
    if action != "in" && action != "out" {
        return None;
    }
    let mut row = MediaRow { cycle, slot: Some(slot), action: action.into(), source_name: name, ..Default::default() };
    for kv in toks {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "sha256" => row.sha256 = Some(v.into()),
            "format" => row.format = Some(v.into()),
            "type" => row.hw_type = v.parse().ok(),
            "mapper" => row.mapper_type = Some(v.into()),
            "banks" => row.banks = v.parse().ok(),
            _ => {}
        }
    }
    Some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cart_in_record_parses_with_a_name_holding_spaces() {
        let r = row_from_mark(
            42,
            "media/1 cart in sha256=ab12 format=crt type=32 mapper=easyflash banks=64 name=Yeti Mountain.crt",
        )
        .unwrap();
        assert_eq!(r.cycle, 42);
        assert_eq!(r.slot.as_deref(), Some("cart"));
        assert_eq!(r.action, "in");
        assert_eq!(r.sha256.as_deref(), Some("ab12"));
        assert_eq!(r.hw_type, Some(32));
        assert_eq!(r.mapper_type.as_deref(), Some("easyflash"));
        assert_eq!(r.banks, Some(64));
        assert_eq!(r.source_name.as_deref(), Some("Yeti Mountain.crt"));
    }

    #[test]
    fn out_and_power_off_parse_and_a_user_mark_does_not() {
        let o = row_from_mark(7, "media/1 drive8 out sha256=ff").unwrap();
        assert_eq!((o.slot.as_deref(), o.action.as_str(), o.sha256.as_deref()), (Some("drive8"), "out", Some("ff")));
        let p = row_from_mark(9, "media/1 power off").unwrap();
        assert_eq!((p.slot, p.action.as_str()), (None, "power-off"));
        assert!(row_from_mark(1, "start of level 2").is_none());
        assert!(row_from_mark(1, "media/2 cart in sha256=ff").is_none());
    }

    #[test]
    fn header_rows_cover_the_cart_and_each_drive_in_unit_order() {
        let m = serde_json::json!({ "v": 1,
            "cart": { "sha256": "c", "sourceName": "y.crt", "format": "crt", "hwType": 32,
                      "mapperType": "easyflash", "banks": 64, "crtName": "YETI" },
            "drives": { "9": { "sha256": "d9", "sourceName": "b.d64", "format": "d64" },
                        "8": { "sha256": "d8", "sourceName": "a.d64", "format": "d64" } } });
        let rows = rows_from_header(Some(&m), 100);
        let slots: Vec<_> = rows.iter().map(|r| r.slot.clone().unwrap()).collect();
        assert_eq!(slots, ["cart", "drive8", "drive9"]);
        assert!(rows.iter().all(|r| r.action == "start" && r.cycle == 100));
        assert_eq!(rows[0].crt_name.as_deref(), Some("YETI"));
        assert!(rows_from_header(Some(&serde_json::json!({ "v": 2 })), 0).is_empty());
        assert!(rows_from_header(None, 0).is_empty());
    }
}
