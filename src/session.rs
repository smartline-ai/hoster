use std::collections::HashSet;
use std::sync::Mutex;

/// In-memory session store. A session is a random 256-bit token; a hoster
/// restart clears the set (everyone re-logs-in). No persistence by design.
#[derive(Default)]
pub struct Sessions {
    tokens: Mutex<HashSet<String>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh session token from the OS CSPRNG and store it.
    pub fn create(&self) -> String {
        let mut buf = [0u8; 32];
        getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
        let mut token = String::with_capacity(64);
        for b in buf {
            use std::fmt::Write;
            let _ = write!(token, "{b:02x}");
        }
        self.tokens.lock().unwrap().insert(token.clone());
        token
    }

    pub fn validate(&self, token: &str) -> bool {
        self.tokens.lock().unwrap().contains(token)
    }

    pub fn remove(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
    }
}

/// Pull one cookie's value out of a `Cookie` header (`k=v; k2=v2`).
pub fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    let header = header?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=')
            && k == name
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Length-checked, byte-diff-accumulating comparison so a password check
/// doesn't leak how many leading bytes matched via timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_validate() {
        let s = Sessions::new();
        let t = s.create();
        assert!(s.validate(&t));
        assert!(!s.validate("nope"));
    }

    #[test]
    fn tokens_are_distinct_and_long() {
        let s = Sessions::new();
        let a = s.create();
        let b = s.create();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn remove_invalidates() {
        let s = Sessions::new();
        let t = s.create();
        s.remove(&t);
        assert!(!s.validate(&t));
    }

    #[test]
    fn parses_named_cookie() {
        assert_eq!(
            cookie_value(Some("a=1; hoster_session=xyz; b=2"), "hoster_session"),
            Some("xyz".to_string())
        );
        assert_eq!(
            cookie_value(Some("hoster_session=only"), "hoster_session"),
            Some("only".to_string())
        );
        assert_eq!(cookie_value(Some("other=1"), "hoster_session"), None);
        assert_eq!(cookie_value(None, "hoster_session"), None);
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
