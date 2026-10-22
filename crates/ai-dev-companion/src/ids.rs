//! Random identifiers.

/// A random (version 4) UUID in its canonical lowercase form, like
/// `crypto.randomUUID()`.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the OS random source is unavailable");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$` (lowercase only).
pub fn is_lower_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_well_formed_and_distinct() {
        let (a, b) = (uuid_v4(), uuid_v4());
        assert!(is_lower_uuid(&a) && is_lower_uuid(&b));
        assert_ne!(a, b);
        assert_eq!(a.as_bytes()[14], b'4', "version 4");
        assert!(matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "RFC 4122 variant");
    }

    #[test]
    fn the_checker_is_strict() {
        assert!(is_lower_uuid("3f2b8c1e-1111-4222-8333-444455556666"));
        for bad in ["not-a-uuid", "3F2B8C1E-1111-4222-8333-444455556666", "3f2b8c1e-1111-4222-8333-44445555666", "", "3f2b8c1e-1111-4222-8333-444455556666\n"] {
            assert!(!is_lower_uuid(bad), "{bad:?}");
        }
    }
}
