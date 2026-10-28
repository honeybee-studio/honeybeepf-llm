/// Model name without a snapshot date (`-2024-08-06`, `-20250929`, `@20251101`).
pub(super) fn without_date(name: &str) -> Option<&str> {
    let num = |s: &str, n: usize| -> Option<u32> {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let is_date = |y: &str, m: &str, d: &str| {
        num(y, 4).is_some()
            && matches!(num(m, 2), Some(1..=12))
            && matches!(num(d, 2), Some(1..=31))
    };
    if let Some((base, date)) = name.rsplit_once(['-', '@'])
        && let (Some(y), Some(m), Some(d)) = (date.get(..4), date.get(4..6), date.get(6..))
        && is_date(y, m, d)
    {
        return Some(base);
    }
    let (rest, dd) = name.rsplit_once('-')?;
    let (rest, mm) = rest.rsplit_once('-')?;
    let (base, yyyy) = rest.rsplit_once('-')?;
    is_date(yyyy, mm, dd).then_some(base)
}
