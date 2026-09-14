//! What a virtual user carries between requests, and how long it carries it.
//!
//! A session is the cookie jar plus the auth identity binding (design-engine §4.2),
//! and the two move together: a fresh session that reused a token would not be fresh
//! in any way the service can tell.
//!
//! | `session` | who that is |
//! |---|---|
//! | `fresh` | a first-time or logged-out user, every iteration |
//! | `reuse` | a returning user, working through a warm session |
//! | `pool` | a population, neither all-new nor all-one |
//!
//! The distinction matters more than it looks: `fresh` everywhere overstates login
//! load and destroys cache locality, `reuse` everywhere hides both, and the gap
//! between those two mistakes is easily a factor of two in apparent capacity.
//!
//! **What this jar is not.** One target, one scheme, one host — so `Domain` and
//! `Secure` decide nothing here and are not read. `Path` is matched as a prefix,
//! `Max-Age` is honoured because a service uses it to delete a cookie, and `Expires`
//! is not parsed: a load run is minutes long and a date-based expiry inside it is a
//! service telling a browser something this is not.

use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::HeaderMap;
use metrix_plan::SessionPolicy;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// One cookie, as much of it as matters here.
#[derive(Clone)]
struct Cookie {
    value: String,
    path: String,
    /// `None` for a session cookie, which is every cookie as far as a run is
    /// concerned unless the service says otherwise.
    until: Option<Instant>,
}

/// What one session has been told to remember.
#[derive(Default)]
pub(crate) struct Jar {
    cookies: BTreeMap<String, Cookie>,
}

impl Jar {
    /// Read every `Set-Cookie` in a response.
    pub fn absorb(&mut self, headers: &HeaderMap) {
        for value in headers.get_all(hyper::header::SET_COOKIE) {
            let Ok(text) = value.to_str() else { continue };
            let mut parts = text.split(';');
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let mut path = "/".to_owned();
            let mut max_age = None;
            for attribute in parts {
                let (key, argument) = attribute
                    .split_once('=')
                    .map_or((attribute.trim(), ""), |(k, v)| (k.trim(), v.trim()));
                match key.to_ascii_lowercase().as_str() {
                    "path" if !argument.is_empty() => path = argument.to_owned(),
                    "max-age" => max_age = argument.parse::<i64>().ok(),
                    _ => {}
                }
            }
            // A service deletes a cookie by setting it to expire, and a jar that kept
            // sending it would be a client that never logged out.
            if max_age.is_some_and(|seconds| seconds <= 0) {
                self.cookies.remove(name);
                continue;
            }
            self.cookies.insert(
                name.to_owned(),
                Cookie {
                    value: value.trim().to_owned(),
                    path,
                    until: max_age.map(|seconds| {
                        Instant::now() + std::time::Duration::from_secs(seconds as u64)
                    }),
                },
            );
        }
    }

    /// The `Cookie` header for a request to this path, or nothing to send.
    pub fn header(&self, path: &str) -> Option<String> {
        let now = Instant::now();
        let mut out = String::new();
        for (name, cookie) in &self.cookies {
            if cookie.until.is_some_and(|until| now >= until) {
                continue;
            }
            if !path.starts_with(&cookie.path) {
                continue;
            }
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(name);
            out.push('=');
            out.push_str(&cookie.value);
        }
        (!out.is_empty()).then_some(out)
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cookies.len()
    }
}

/// Which session an iteration is, and which jar that session keeps.
#[derive(Clone, Copy)]
pub(crate) struct Session {
    /// Who the service thinks this is. What the auth identity is bound to, so a fresh
    /// session gets a fresh identity and a pooled one comes back as somebody it has
    /// seen before.
    pub id: u64,
    /// Which jar to read and write. The same as the session for `reuse` and `pool`;
    /// for `fresh` it is the slot's own jar, emptied at the start of the iteration.
    jar: usize,
    /// True when this iteration starts the session rather than continuing one.
    starts: bool,
}

/// Every session the plan can be in, one jar each.
pub(crate) struct Sessions {
    policy: SessionPolicy,
    jars: Vec<Mutex<Jar>>,
}

impl Sessions {
    /// One set per chain, sized by that chain's policy.
    ///
    /// `fresh` still gets one jar per slot rather than one per iteration: a slot runs
    /// one iteration at a time, so emptying its jar at the start is exactly a new
    /// session, and it costs no allocation on the hot path.
    pub fn new(policy: SessionPolicy, concurrency: usize, pool_size: Option<u32>) -> Self {
        let count = match policy {
            SessionPolicy::Fresh | SessionPolicy::Reuse => concurrency,
            SessionPolicy::Pool => pool_size.unwrap_or(1).max(1) as usize,
        };
        let mut jars = Vec::new();
        jars.resize_with(count.max(1), || Mutex::new(Jar::default()));
        Self { policy, jars }
    }

    /// Which session this iteration runs as.
    pub fn of(&self, vu: usize, iteration: u64) -> Session {
        let jars = self.jars.len() as u64;
        match self.policy {
            // A new identity every time, in the slot's own jar.
            SessionPolicy::Fresh => Session {
                id: iteration,
                jar: vu % self.jars.len(),
                starts: true,
            },
            // One session for the life of the virtual user.
            SessionPolicy::Reuse => Session {
                id: vu as u64,
                jar: vu % self.jars.len(),
                starts: false,
            },
            // A fixed set, cycled: a population rather than a person.
            SessionPolicy::Pool => Session {
                id: iteration % jars,
                jar: (iteration % jars) as usize,
                starts: false,
            },
        }
    }

    /// Take this iteration's jar, emptying it first when the session is new.
    pub async fn open(&self, session: Session) -> tokio::sync::MutexGuard<'_, Jar> {
        let mut jar = self.jars[session.jar].lock().await;
        if session.starts {
            jar.clear();
        }
        jar
    }
}

/// Every chain's sessions, in the chain's own order.
pub(crate) type PerChain = Arc<Vec<Sessions>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                hyper::header::SET_COOKIE,
                hyper::header::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_cookie_comes_back_on_the_next_request() {
        let mut jar = Jar::default();
        jar.absorb(&headers(&["sid=abc; Path=/; HttpOnly"]));
        assert_eq!(jar.header("/api/orders").as_deref(), Some("sid=abc"));
    }

    #[test]
    fn two_cookies_travel_together_in_one_header() {
        let mut jar = Jar::default();
        jar.absorb(&headers(&["a=1", "b=2"]));
        // One `Cookie` header with both, which is what a client sends.
        assert_eq!(jar.header("/").as_deref(), Some("a=1; b=2"));
    }

    #[test]
    fn a_later_value_replaces_an_earlier_one() {
        let mut jar = Jar::default();
        jar.absorb(&headers(&["sid=first"]));
        jar.absorb(&headers(&["sid=second"]));
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.header("/").as_deref(), Some("sid=second"));
    }

    #[test]
    fn a_path_decides_which_requests_carry_it() {
        let mut jar = Jar::default();
        jar.absorb(&headers(&["cart=1; Path=/api/cart"]));
        assert_eq!(jar.header("/api/cart/items").as_deref(), Some("cart=1"));
        // Sending it everywhere would be telling the service something about a
        // request the service never asked to be told.
        assert_eq!(jar.header("/api/search"), None);
    }

    #[test]
    fn a_service_deleting_a_cookie_is_a_client_that_logged_out() {
        let mut jar = Jar::default();
        jar.absorb(&headers(&["sid=abc"]));
        jar.absorb(&headers(&["sid=; Max-Age=0"]));
        assert_eq!(jar.header("/"), None);
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn a_fresh_session_is_a_different_person_every_iteration() {
        let sessions = Sessions::new(SessionPolicy::Fresh, 4, None);
        let first = sessions.of(2, 10);
        let second = sessions.of(2, 11);
        assert_ne!(first.id, second.id);
        // Same jar, emptied: a slot runs one iteration at a time, so this is a new
        // session without an allocation per iteration.
        assert!(first.starts && second.starts);
    }

    #[test]
    fn a_reused_session_is_the_same_person_all_run() {
        let sessions = Sessions::new(SessionPolicy::Reuse, 4, None);
        assert_eq!(sessions.of(2, 10).id, sessions.of(2, 99).id);
        assert_ne!(sessions.of(2, 10).id, sessions.of(3, 10).id);
        assert!(!sessions.of(2, 10).starts);
    }

    #[test]
    fn a_pool_cycles_a_fixed_population() {
        let sessions = Sessions::new(SessionPolicy::Pool, 64, Some(3));
        let seen: Vec<u64> = (0..7).map(|i| sessions.of(0, i).id).collect();
        // Neither all-new nor all-one, and the same three whichever slot runs them.
        assert_eq!(seen, [0, 1, 2, 0, 1, 2, 0]);
        assert_eq!(sessions.of(9, 4).id, sessions.of(1, 4).id);
    }
}
