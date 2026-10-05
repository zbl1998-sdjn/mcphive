//! The state of one shared MCP server: which client asked for what.
//!
//! Many clients talk to one server. A server speaks to one client, so this is
//! the part that makes the conversation consistent: every request gets an id of
//! its own on the way to the server and its original id back, the `initialize`
//! handshake happens once, progress goes to the client that asked for it, and a
//! request from the server goes to a client that is there to answer it.
//!
//! The router does no I/O. It takes an [`Event`] and returns the [`Action`]s to
//! carry out, which keeps all of this testable without a process or a pipe.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

/// A connection to the shared server.
pub type ClientId = u64;

/// Something that happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A client connected.
    ClientUp(ClientId),
    /// A client went away.
    ClientDown(ClientId),
    /// A message from a client.
    FromClient(ClientId, Value),
    /// A message from the server.
    FromServer(Value),
}

/// Something to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write a message to the server.
    ToServer(Value),
    /// Write a message to a client.
    ToClient(ClientId, Value),
}

struct Pending {
    client: ClientId,
    original_id: Value,
    token: Option<String>,
}

enum Init {
    Idle,
    InFlight {
        up_id: u64,
        waiting: Vec<(ClientId, Value)>,
    },
    Done(Value),
}

/// What kind of JSON-RPC message this is.
enum Kind<'a> {
    Request(&'a Value, &'a str),
    Notification(&'a str),
    Response(&'a Value),
    Invalid,
}

fn classify(message: &Value) -> Kind<'_> {
    let Some(object) = message.as_object() else {
        return Kind::Invalid;
    };
    let id = object.get("id").filter(|id| !id.is_null());
    match (object.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Kind::Request(id, method),
        (Some(method), None) => Kind::Notification(method),
        (None, Some(id)) if object.contains_key("result") || object.contains_key("error") => {
            Kind::Response(id)
        }
        _ => Kind::Invalid,
    }
}

fn success(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The conversation between many clients and one server.
pub struct Router {
    next_up: u64,
    next_down: u64,
    pending: HashMap<u64, Pending>,
    /// Progress tokens as the server knows them, and who they belong to.
    tokens: HashMap<String, (ClientId, Value)>,
    init: Init,
    /// Requests of the server that a client has to answer: its id for them.
    server_requests: HashMap<u64, (ClientId, Value)>,
    clients: BTreeSet<ClientId>,
    last_active: Option<ClientId>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_up: 1,
            next_down: 1,
            pending: HashMap::new(),
            tokens: HashMap::new(),
            init: Init::Idle,
            server_requests: HashMap::new(),
            clients: BTreeSet::new(),
            last_active: None,
        }
    }

    /// How many clients are connected.
    #[must_use]
    pub fn clients(&self) -> usize {
        self.clients.len()
    }

    /// Whether the server has completed its handshake.
    #[must_use]
    pub fn initialized(&self) -> bool {
        matches!(self.init, Init::Done(_))
    }

    /// Take in an event and say what to do about it.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::ClientUp(client) => {
                self.clients.insert(client);
                self.last_active.get_or_insert(client);
            }
            Event::ClientDown(client) => self.client_down(client, &mut out),
            Event::FromClient(client, message) => self.client_message(client, message, &mut out),
            Event::FromServer(message) => self.server_message(message, &mut out),
        }
        out
    }

    fn alloc_up(&mut self) -> u64 {
        let id = self.next_up;
        self.next_up += 1;
        id
    }

    fn client_message(&mut self, client: ClientId, mut message: Value, out: &mut Vec<Action>) {
        match classify(&message) {
            Kind::Request(id, method) => {
                let id = id.clone();
                let method = method.to_owned();
                self.last_active = Some(client);
                match method.as_str() {
                    "initialize" => self.initialize(client, id, message, out),
                    // Answered here: the client is asking whether *we* are alive.
                    "ping" => out.push(Action::ToClient(client, success(&id, &json!({})))),
                    _ => self.forward_request(client, id, message, out),
                }
            }
            Kind::Notification(method) => match method {
                // The server has had its `initialized` from us already, and the
                // other two are about one client and would mislead the server.
                "notifications/initialized"
                | "notifications/roots/list_changed"
                | "notifications/progress" => {}
                "notifications/cancelled" => self.cancel(client, message, out),
                _ => out.push(Action::ToServer(message)),
            },
            Kind::Response(id) => {
                let Some(down) = id.as_u64() else { return };
                let owned = self
                    .server_requests
                    .get(&down)
                    .is_some_and(|(owner, _)| *owner == client);
                if owned {
                    let (_, original) = self.server_requests.remove(&down).expect("checked");
                    message["id"] = original;
                    out.push(Action::ToServer(message));
                }
            }
            Kind::Invalid => out.push(Action::ToClient(
                client,
                failure(&Value::Null, -32600, "Invalid Request"),
            )),
        }
    }

    fn initialize(
        &mut self,
        client: ClientId,
        id: Value,
        mut message: Value,
        out: &mut Vec<Action>,
    ) {
        match &mut self.init {
            Init::Done(result) => out.push(Action::ToClient(client, success(&id, result))),
            Init::InFlight { waiting, .. } => waiting.push((client, id)),
            Init::Idle => {
                let up_id = self.alloc_up();
                self.init = Init::InFlight {
                    up_id,
                    waiting: vec![(client, id)],
                };
                message["id"] = json!(up_id);
                out.push(Action::ToServer(message));
            }
        }
    }

    fn forward_request(
        &mut self,
        client: ClientId,
        id: Value,
        mut message: Value,
        out: &mut Vec<Action>,
    ) {
        let up_id = self.alloc_up();
        let mut token = None;
        if let Some(original) = message.pointer("/params/_meta/progressToken").cloned() {
            let key = format!("hive-{up_id}");
            message["params"]["_meta"]["progressToken"] = json!(key);
            self.tokens.insert(key.clone(), (client, original));
            token = Some(key);
        }
        message["id"] = json!(up_id);
        self.pending.insert(
            up_id,
            Pending {
                client,
                original_id: id,
                token,
            },
        );
        out.push(Action::ToServer(message));
    }

    fn cancel(&mut self, client: ClientId, mut message: Value, out: &mut Vec<Action>) {
        let Some(request) = message.pointer("/params/requestId").cloned() else {
            return;
        };
        let found = self
            .pending
            .iter()
            .find(|(_, pending)| pending.client == client && pending.original_id == request)
            .map(|(up_id, _)| *up_id);
        if let Some(up_id) = found {
            message["params"]["requestId"] = json!(up_id);
            out.push(Action::ToServer(message));
        }
    }

    fn server_message(&mut self, mut message: Value, out: &mut Vec<Action>) {
        match classify(&message) {
            Kind::Response(id) => {
                let Some(up_id) = id.as_u64() else { return };
                if matches!(&self.init, Init::InFlight { up_id: waiting_for, .. } if *waiting_for == up_id)
                {
                    self.initialized_by_server(&message, out);
                    return;
                }
                let Some(pending) = self.pending.remove(&up_id) else {
                    return;
                };
                if let Some(token) = pending.token {
                    self.tokens.remove(&token);
                }
                message["id"] = pending.original_id;
                out.push(Action::ToClient(pending.client, message));
            }
            Kind::Notification(method) => match method {
                "notifications/progress" => {
                    let Some(key) = message
                        .pointer("/params/progressToken")
                        .and_then(Value::as_str)
                    else {
                        return;
                    };
                    if let Some((client, original)) = self.tokens.get(key) {
                        let client = *client;
                        message["params"]["progressToken"] = original.clone();
                        out.push(Action::ToClient(client, message));
                    }
                }
                "notifications/cancelled" => {}
                _ => {
                    for client in &self.clients {
                        out.push(Action::ToClient(*client, message.clone()));
                    }
                }
            },
            Kind::Request(id, method) => {
                let id = id.clone();
                if method == "ping" {
                    out.push(Action::ToServer(success(&id, &json!({}))));
                    return;
                }
                // Whoever spoke last is the one waiting for something.
                let target = self
                    .last_active
                    .filter(|client| self.clients.contains(client))
                    .or_else(|| self.clients.iter().next().copied());
                let Some(client) = target else {
                    out.push(Action::ToServer(failure(
                        &id,
                        -32603,
                        "no client is connected",
                    )));
                    return;
                };
                let down = self.next_down;
                self.next_down += 1;
                self.server_requests.insert(down, (client, id));
                message["id"] = json!(down);
                out.push(Action::ToClient(client, message));
            }
            Kind::Invalid => {}
        }
    }

    fn initialized_by_server(&mut self, message: &Value, out: &mut Vec<Action>) {
        let Init::InFlight { waiting, .. } = std::mem::replace(&mut self.init, Init::Idle) else {
            return;
        };
        if let Some(result) = message.get("result") {
            self.init = Init::Done(result.clone());
            // Sent here, as the server's client, so that a client that comes
            // later does not have to wait for another one to say it.
            out.push(Action::ToServer(
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            ));
            for (client, original) in waiting {
                out.push(Action::ToClient(client, success(&original, result)));
            }
        } else {
            let error = message
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!({"code": -32603, "message": "initialize failed"}));
            for (client, original) in waiting {
                out.push(Action::ToClient(
                    client,
                    json!({"jsonrpc": "2.0", "id": original, "error": error.clone()}),
                ));
            }
        }
    }

    fn client_down(&mut self, client: ClientId, out: &mut Vec<Action>) {
        self.clients.remove(&client);
        if self.last_active == Some(client) {
            self.last_active = self.clients.iter().next().copied();
        }
        // Tell the server to stop work that nobody is waiting for.
        let mut abandoned: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.client == client)
            .map(|(up_id, _)| *up_id)
            .collect();
        abandoned.sort_unstable();
        for up_id in abandoned {
            if let Some(pending) = self.pending.remove(&up_id) {
                if let Some(token) = pending.token {
                    self.tokens.remove(&token);
                }
                out.push(Action::ToServer(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": up_id, "reason": "the client disconnected"},
                })));
            }
        }
        // And answer for the client what it can no longer answer.
        let mut unanswered: Vec<u64> = self
            .server_requests
            .iter()
            .filter(|(_, (owner, _))| *owner == client)
            .map(|(down, _)| *down)
            .collect();
        unanswered.sort_unstable();
        for down in unanswered {
            if let Some((_, original)) = self.server_requests.remove(&down) {
                out.push(Action::ToServer(failure(
                    &original,
                    -32000,
                    "the client disconnected",
                )));
            }
        }
        if let Init::InFlight { waiting, .. } = &mut self.init {
            waiting.retain(|(waiter, _)| *waiter != client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::needless_pass_by_value,
        reason = "reads better at the call sites"
    )]
    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn server_ids(actions: &[Action]) -> Vec<Value> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ToServer(m) => m.get("id").cloned(),
                Action::ToClient(..) => None,
            })
            .collect()
    }

    /// A router with the handshake done through client 1, and client 2 connected.
    fn ready() -> Router {
        let mut router = Router::new();
        router.handle(Event::ClientUp(1));
        let sent = router.handle(Event::FromClient(
            1,
            request(
                json!(0),
                "initialize",
                json!({"protocolVersion": "2025-06-18"}),
            ),
        ));
        let Action::ToServer(init) = &sent[0] else {
            panic!("no initialize sent")
        };
        let reply = json!({"jsonrpc": "2.0", "id": init["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {}}});
        router.handle(Event::FromServer(reply));
        router.handle(Event::ClientUp(2));
        router
    }

    #[test]
    fn the_handshake_happens_once_and_later_clients_get_the_cached_answer() {
        let mut router = Router::new();
        router.handle(Event::ClientUp(1));
        router.handle(Event::ClientUp(2));
        let first = router.handle(Event::FromClient(
            1,
            request(json!("a"), "initialize", json!({})),
        ));
        assert_eq!(first.len(), 1, "one initialize goes to the server");
        // The second client asks while the first is still waiting.
        let second = router.handle(Event::FromClient(
            2,
            request(json!(7), "initialize", json!({})),
        ));
        assert!(second.is_empty(), "it waits for the same answer");
        let Action::ToServer(sent) = &first[0] else {
            panic!()
        };
        let answer =
            json!({"jsonrpc": "2.0", "id": sent["id"], "result": {"serverInfo": {"name": "x"}}});
        let done = router.handle(Event::FromServer(answer));
        // `initialized` for the server, and an answer for each client with its own id.
        assert!(
            matches!(&done[0], Action::ToServer(m) if m["method"] == "notifications/initialized")
        );
        assert_eq!(
            done[1],
            Action::ToClient(
                1,
                json!({"jsonrpc": "2.0", "id": "a", "result": {"serverInfo": {"name": "x"}}})
            )
        );
        assert_eq!(
            done[2],
            Action::ToClient(
                2,
                json!({"jsonrpc": "2.0", "id": 7, "result": {"serverInfo": {"name": "x"}}})
            )
        );
        // A third client later gets it from the cache, with nothing sent to the server.
        router.handle(Event::ClientUp(3));
        let third = router.handle(Event::FromClient(
            3,
            request(json!(1), "initialize", json!({})),
        ));
        assert_eq!(
            third,
            vec![Action::ToClient(
                3,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"serverInfo": {"name": "x"}}})
            )]
        );
        assert!(router.initialized());
    }

    #[test]
    fn a_failed_handshake_reaches_everyone_waiting_and_can_be_tried_again() {
        let mut router = Router::new();
        router.handle(Event::ClientUp(1));
        let sent = router.handle(Event::FromClient(
            1,
            request(json!(1), "initialize", json!({})),
        ));
        let Action::ToServer(init) = &sent[0] else {
            panic!()
        };
        let out = router.handle(Event::FromServer(json!({"jsonrpc": "2.0", "id": init["id"], "error": {"code": -32602, "message": "bad version"}})));
        assert_eq!(
            out,
            vec![Action::ToClient(
                1,
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "bad version"}})
            )]
        );
        assert!(!router.initialized());
        let again = router.handle(Event::FromClient(
            1,
            request(json!(2), "initialize", json!({})),
        ));
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn requests_with_the_same_id_from_two_clients_stay_apart() {
        let mut router = ready();
        let a = router.handle(Event::FromClient(
            1,
            request(json!(1), "tools/list", json!({})),
        ));
        let b = router.handle(Event::FromClient(
            2,
            request(json!(1), "tools/list", json!({})),
        ));
        let ids = [server_ids(&a), server_ids(&b)].concat();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "the server must see two different ids");
        // Answers come back in the other order and still reach the right client with the original id.
        let to_b = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": ids[1], "result": {"tools": ["b"]}}),
        ));
        let to_a = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": ids[0], "result": {"tools": ["a"]}}),
        ));
        assert_eq!(
            to_b,
            vec![Action::ToClient(
                2,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": ["b"]}})
            )]
        );
        assert_eq!(
            to_a,
            vec![Action::ToClient(
                1,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": ["a"]}})
            )]
        );
    }

    #[test]
    fn string_ids_come_back_as_strings() {
        let mut router = ready();
        let sent = router.handle(Event::FromClient(
            2,
            request(json!("req-9"), "tools/call", json!({"name": "echo"})),
        ));
        let back = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": server_ids(&sent)[0], "result": {}}),
        ));
        assert_eq!(
            back,
            vec![Action::ToClient(
                2,
                json!({"jsonrpc": "2.0", "id": "req-9", "result": {}})
            )]
        );
    }

    #[test]
    fn progress_goes_to_the_client_that_asked_with_its_own_token() {
        let mut router = ready();
        let a = router.handle(Event::FromClient(
            1,
            request(
                json!(1),
                "tools/call",
                json!({"_meta": {"progressToken": "t"}}),
            ),
        ));
        let b = router.handle(Event::FromClient(
            2,
            request(
                json!(1),
                "tools/call",
                json!({"_meta": {"progressToken": "t"}}),
            ),
        ));
        let (Action::ToServer(ma), Action::ToServer(mb)) = (&a[0], &b[0]) else {
            panic!()
        };
        let (ta, tb) = (
            ma["params"]["_meta"]["progressToken"].clone(),
            mb["params"]["_meta"]["progressToken"].clone(),
        );
        assert_ne!(
            ta, tb,
            "two clients that both used `t` must not share a token"
        );
        let out = router.handle(Event::FromServer(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": tb, "progress": 5}})));
        assert_eq!(
            out,
            vec![Action::ToClient(
                2,
                json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "t", "progress": 5}})
            )]
        );
        // Once the request is answered, a late progress message goes nowhere.
        router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": mb["id"], "result": {}}),
        ));
        let late = router.handle(Event::FromServer(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": tb, "progress": 9}})));
        assert!(late.is_empty());
    }

    #[test]
    fn notifications_of_the_server_go_to_every_client() {
        let mut router = ready();
        let out = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
        ));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_cancel_names_the_servers_id_and_only_for_the_clients_own_request() {
        let mut router = ready();
        let a = router.handle(Event::FromClient(
            1,
            request(json!(5), "tools/call", json!({})),
        ));
        let up = server_ids(&a)[0].clone();
        let cancel = |client, id| {
            Event::FromClient(
                client,
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}),
            )
        };
        // Client 2 has no request 5, so its cancel is dropped.
        assert!(router.handle(cancel(2, json!(5))).is_empty());
        let out = router.handle(cancel(1, json!(5)));
        assert_eq!(
            out,
            vec![Action::ToServer(
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": up}})
            )]
        );
    }

    #[test]
    fn a_client_that_leaves_has_its_work_cancelled() {
        let mut router = ready();
        let a = router.handle(Event::FromClient(
            1,
            request(
                json!(1),
                "tools/call",
                json!({"_meta": {"progressToken": 1}}),
            ),
        ));
        let up = server_ids(&a)[0].clone();
        let out = router.handle(Event::ClientDown(1));
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Action::ToServer(m) if m["method"] == "notifications/cancelled" && m["params"]["requestId"] == up)
        );
        // The late answer is dropped.
        assert!(
            router
                .handle(Event::FromServer(
                    json!({"jsonrpc": "2.0", "id": up, "result": {}})
                ))
                .is_empty()
        );
        assert_eq!(router.clients(), 1);
    }

    #[test]
    fn a_request_of_the_server_goes_to_the_client_that_spoke_last_and_the_answer_goes_back() {
        let mut router = ready();
        router.handle(Event::FromClient(
            2,
            request(json!(1), "tools/call", json!({})),
        ));
        let out = router.handle(Event::FromServer(json!({"jsonrpc": "2.0", "id": "srv-1", "method": "sampling/createMessage", "params": {}})));
        let [Action::ToClient(2, asked)] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_ne!(asked["id"], json!("srv-1"));
        // The wrong client cannot answer for it.
        assert!(
            router
                .handle(Event::FromClient(
                    1,
                    json!({"jsonrpc": "2.0", "id": asked["id"], "result": {}})
                ))
                .is_empty()
        );
        let back = router.handle(Event::FromClient(
            2,
            json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"role": "assistant"}}),
        ));
        assert_eq!(
            back,
            vec![Action::ToServer(
                json!({"jsonrpc": "2.0", "id": "srv-1", "result": {"role": "assistant"}})
            )]
        );
    }

    #[test]
    fn a_request_of_the_server_that_its_client_never_answers_is_answered_with_an_error() {
        let mut router = ready();
        router.handle(Event::FromClient(
            2,
            request(json!(1), "tools/call", json!({})),
        ));
        router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": 4, "method": "roots/list"}),
        ));
        let out = router.handle(Event::ClientDown(2));
        assert!(out.iter().any(
            |a| matches!(a, Action::ToServer(m) if m["id"] == 4 && m["error"]["code"] == -32000)
        ));
    }

    #[test]
    fn a_request_of_the_server_with_nobody_connected_is_refused() {
        let mut router = Router::new();
        let out = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": 1, "method": "roots/list"}),
        ));
        assert!(matches!(&out[0], Action::ToServer(m) if m["error"]["code"] == -32603));
    }

    #[test]
    fn ping_is_answered_in_both_directions_without_bothering_the_other_side() {
        let mut router = ready();
        let out = router.handle(Event::FromClient(1, request(json!(3), "ping", json!({}))));
        assert_eq!(
            out,
            vec![Action::ToClient(
                1,
                json!({"jsonrpc": "2.0", "id": 3, "result": {}})
            )]
        );
        let out = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}),
        ));
        assert_eq!(
            out,
            vec![Action::ToServer(
                json!({"jsonrpc": "2.0", "id": 8, "result": {}})
            )]
        );
    }

    #[test]
    fn the_initialized_notifications_of_clients_never_reach_the_server() {
        let mut router = ready();
        let out = router.handle(Event::FromClient(
            2,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn other_notifications_of_a_client_are_passed_on_and_junk_is_refused() {
        let mut router = ready();
        let note = json!({"jsonrpc": "2.0", "method": "notifications/custom", "params": {"a": 1}});
        assert_eq!(
            router.handle(Event::FromClient(1, note.clone())),
            vec![Action::ToServer(note)]
        );
        let out = router.handle(Event::FromClient(1, json!([1, 2])));
        assert!(matches!(&out[0], Action::ToClient(1, m) if m["error"]["code"] == -32600));
    }

    #[test]
    fn a_client_that_leaves_during_the_handshake_is_not_answered() {
        let mut router = Router::new();
        router.handle(Event::ClientUp(1));
        router.handle(Event::ClientUp(2));
        let sent = router.handle(Event::FromClient(
            1,
            request(json!(1), "initialize", json!({})),
        ));
        router.handle(Event::FromClient(
            2,
            request(json!(1), "initialize", json!({})),
        ));
        router.handle(Event::ClientDown(1));
        let Action::ToServer(init) = &sent[0] else {
            panic!()
        };
        let out = router.handle(Event::FromServer(
            json!({"jsonrpc": "2.0", "id": init["id"], "result": {}}),
        ));
        let to_clients: Vec<_> = out
            .iter()
            .filter_map(|a| {
                if let Action::ToClient(c, _) = a {
                    Some(*c)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(to_clients, vec![2]);
    }
}
