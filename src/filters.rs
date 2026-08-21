use askama::Values;

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Appends the build's asset fingerprint to a `/static` URL.
///
/// Used by `templates/base.html` so that upgrading the server also invalidates
/// every cached stylesheet and script.
pub fn asset(path: &str, _: &dyn Values) -> askama::Result<String> {
    Ok(format!("{path}?v={}", crate::asset_version()))
}

/// Human-readable byte formatting for Askama templates, mirroring `formatBytes()` in
/// `static/app.js` so server-rendered and JS-rendered values agree.
pub fn format_bytes<T>(value: &T, _: &dyn Values) -> askama::Result<String>
where
    T: Copy,
    i128: From<T>,
{
    let bytes = i128::from(*value).max(0).min(u64::MAX as i128) as u64;
    Ok(render(bytes))
}

fn render(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let bytes_f = bytes as f64;
    let index = (bytes_f.log(1024.0).floor() as usize).min(UNITS.len() - 1);
    let scaled = bytes_f / 1024f64.powi(index as i32);
    if index == 0 {
        format!("{scaled:.0} {}", UNITS[index])
    } else {
        format!("{scaled:.1} {}", UNITS[index])
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn zero_is_zero_bytes() {
        assert_eq!(render(0), "0 B");
    }

    #[test]
    fn stays_in_bytes_below_the_kib_boundary() {
        assert_eq!(render(1023), "1023 B");
    }

    #[test]
    fn crosses_into_kib_at_the_boundary() {
        assert_eq!(render(1024), "1.0 KiB");
    }

    #[test]
    fn matches_the_client_side_formatter() {
        // static/app.js formatBytes(1536) === "1.5 KiB"
        assert_eq!(render(1536), "1.5 KiB");
    }

    #[test]
    fn formats_gib_scale_values() {
        assert_eq!(render(21_474_836_480), "20.0 GiB");
    }

    #[test]
    fn caps_at_tib() {
        assert_eq!(render(5 * 1024u64.pow(4)), "5.0 TiB");
    }
}
