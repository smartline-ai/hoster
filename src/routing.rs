use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// Whether a route is ready to receive traffic.
///
/// `Starting` exists so a branch URL can answer "starting" instead of refusing
/// the connection while its containers boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteState {
    Starting,
    Ready,
}

/// Where one hostname's traffic goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub upstream: SocketAddr,
    pub state: RouteState,
}

/// An immutable snapshot of every live hostname.
///
/// Built whole and swapped in; never mutated while readers hold it.
#[derive(Debug, Default)]
pub struct RoutingTable {
    routes: HashMap<String, Route>,
}

impl RoutingTable {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn insert(&mut self, host: impl Into<String>, route: Route) {
        self.routes.insert(normalize_host(&host.into()), route);
    }

    pub fn lookup(&self, host: &str) -> Option<&Route> {
        self.routes.get(&normalize_host(host))
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Canonical form of a `Host` header for lookup.
///
/// A `Host` header may carry a port (`example.com:443`) and may be fully
/// qualified with a trailing dot. DNS names are case-insensitive. All three
/// must collapse to the same key or lookups miss for reasons nobody enjoys
/// debugging.
///
/// IPv6 literal hosts (`[::1]:80`) are not supported: hoster routes DNS names.
pub fn normalize_host(host: &str) -> String {
    host.split(':')
        .next()
        .unwrap_or(host)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The handoff between the deploy engine (writer) and the proxy (readers).
///
/// Readers take a pointer snapshot with no lock. The writer builds a fresh
/// table and stores it. In-flight requests finish against the table they
/// loaded, so a deploy going live is one atomic pointer swap.
#[derive(Clone)]
pub struct SharedRoutes(Arc<ArcSwap<RoutingTable>>);

impl SharedRoutes {
    pub fn new(table: RoutingTable) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(table)))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<RoutingTable>> {
        self.0.load()
    }

    pub fn swap(&self, table: RoutingTable) {
        self.0.store(Arc::new(table));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(port: u16) -> Route {
        Route {
            upstream: format!("127.0.0.1:{port}").parse().unwrap(),
            state: RouteState::Ready,
        }
    }

    #[test]
    fn normalizes_case() {
        assert_eq!(
            normalize_host("Backend-Foo.Dev.Example.Com"),
            "backend-foo.dev.example.com"
        );
    }

    #[test]
    fn normalizes_strips_port() {
        assert_eq!(
            normalize_host("backend-foo.dev.example.com:443"),
            "backend-foo.dev.example.com"
        );
    }

    #[test]
    fn normalizes_strips_trailing_dot() {
        assert_eq!(
            normalize_host("backend-foo.dev.example.com."),
            "backend-foo.dev.example.com"
        );
    }

    #[test]
    fn lookup_finds_inserted_route() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(
            table.lookup("backend-foo.dev.example.com"),
            Some(&route(8080))
        );
    }

    #[test]
    fn lookup_normalizes_the_query() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(
            table.lookup("Backend-Foo.dev.example.com:443"),
            Some(&route(8080))
        );
    }

    #[test]
    fn lookup_normalizes_the_insert() {
        let mut table = RoutingTable::new();
        table.insert("Backend-Foo.Dev.Example.Com.", route(8080));
        assert_eq!(
            table.lookup("backend-foo.dev.example.com"),
            Some(&route(8080))
        );
    }

    #[test]
    fn unknown_host_is_none() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(table.lookup("backend-bar.dev.example.com"), None);
    }

    #[test]
    fn two_branches_same_port_do_not_collide() {
        let mut table = RoutingTable::new();
        table.insert("backend-branch1.dev.example.com", route(8080));
        table.insert("backend-branch2.dev.example.com", route(8080));
        assert_eq!(
            table
                .lookup("backend-branch1.dev.example.com")
                .unwrap()
                .upstream
                .port(),
            8080
        );
        assert_eq!(
            table
                .lookup("backend-branch2.dev.example.com")
                .unwrap()
                .upstream
                .port(),
            8080
        );
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn swap_replaces_the_whole_table() {
        let mut first = RoutingTable::new();
        first.insert("a.example.com", route(1));
        let shared = SharedRoutes::new(first);
        assert!(shared.load().lookup("a.example.com").is_some());

        let mut second = RoutingTable::new();
        second.insert("b.example.com", route(2));
        shared.swap(second);

        assert!(shared.load().lookup("a.example.com").is_none());
        assert!(shared.load().lookup("b.example.com").is_some());
    }
}
