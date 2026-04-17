use shared::entity::ServerId;
use shared::protocol::{CoordServerMsg, PeerInfo};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub struct ServerEntry {
    pub server_id: u16,
    pub udp_port: u16,
    pub ws_port: u16,
    pub host: String,
    pub tls: bool,
    pub entity_count: u32,
    /// Weighted load: players count 3x, NPCs count 1x.
    /// Falls back to entity_count if not provided (backward compat).
    pub weighted_load: u32,
    pub reply_tx: mpsc::UnboundedSender<String>,
    pub last_heartbeat: std::time::Instant,
    /// When this server registered. Used for join grace period.
    pub registered_at: std::time::Instant,
    /// Number of in-flight transfers originating from this server.
    /// Incremented when a RebalanceDirective is sent, decremented on TransferResult.
    pub in_flight_transfers: u32,
}

pub struct Registry {
    servers: HashMap<u16, ServerEntry>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn register(
        &mut self,
        server_id: u16,
        udp_port: u16,
        ws_port: u16,
        host: Option<String>,
        tls: bool,
        reply_tx: mpsc::UnboundedSender<String>,
    ) {
        if let Some(existing) = self.servers.get(&server_id) {
            if existing.reply_tx.is_closed() {
                tracing::info!(
                    "S{server_id} re-registering (old connection dead) — replacing entry"
                );
            } else {
                tracing::warn!("S{server_id} already registered with live connection — ignoring duplicate");
                return;
            }
        }
        let now = std::time::Instant::now();
        self.servers.insert(
            server_id,
            ServerEntry {
                server_id,
                udp_port,
                ws_port,
                host: host.unwrap_or_else(|| "127.0.0.1".to_string()),
                tls,
                entity_count: 0,
                weighted_load: 0,
                reply_tx,
                last_heartbeat: now,
                registered_at: now,
                in_flight_transfers: 0,
            },
        );
    }

    pub fn unregister(&mut self, server_id: u16) {
        // Removing the entry drops reply_tx, closing the mpsc channel.
        // The sink task detects the closed channel, exits, and closes the
        // WebSocket. The sim-server detects the disconnect and reconnects.
        self.servers.remove(&server_id);
    }

    /// Unregister a server only if its current reply_tx matches the given sender.
    /// This prevents a stale Disconnected event from removing a newer re-registered
    /// connection. Returns true if the server was actually removed.
    pub fn unregister_if_sender_matches(
        &mut self,
        server_id: u16,
        reply_tx: &mpsc::UnboundedSender<String>,
    ) -> bool {
        if let Some(existing) = self.servers.get(&server_id) {
            if existing.reply_tx.same_channel(reply_tx) {
                // The connection is already closing (this is the Disconnected event),
                // Connection is already closing — just remove the entry.
                self.servers.remove(&server_id);
                return true;
            }
            tracing::debug!(
                "S{server_id} disconnect from stale connection — ignoring (new connection active)"
            );
            return false;
        }
        false
    }

    pub fn heartbeat(&mut self, server_id: u16, entity_count: u32, weighted_load: u32) {
        if let Some(entry) = self.servers.get_mut(&server_id) {
            entry.entity_count = entity_count;
            // Use weighted_load if provided (>0), otherwise fall back to entity_count
            entry.weighted_load = if weighted_load > 0 { weighted_load } else { entity_count };
            entry.last_heartbeat = std::time::Instant::now();
        } else {
            tracing::warn!(
                server_id,
                "Heartbeat from unregistered server S{server_id}, ignoring"
            );
        }
    }

    /// Detect servers that have missed heartbeats.
    /// Timeout is 15 seconds. With yield points in the game loop ensuring the
    /// heartbeat task can run, 15s gives ample margin while keeping crash
    /// detection responsive.
    pub fn detect_dead_servers(&self) -> Vec<u16> {
        let timeout = std::time::Duration::from_secs(15);
        let now = std::time::Instant::now();
        self.servers
            .values()
            .filter(|s| s.server_id > 0)
            .filter(|s| now.duration_since(s.last_heartbeat) > timeout)
            .map(|s| s.server_id)
            .collect()
    }

    /// Tell a draining server which targets to distribute entities to.
    pub fn drain_server(&self, server_id: u16) {
        let targets: Vec<ServerId> = self.servers.keys()
            .filter(|&&id| id != server_id && id > 0)
            .map(|&id| ServerId(id))
            .collect();

        if targets.is_empty() {
            tracing::warn!("No servers available to drain S{server_id} into");
            return;
        }

        let msg = CoordServerMsg::DrainDirective { target_servers: targets };
        if let Some(entry) = self.servers.get(&server_id) {
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = entry.reply_tx.send(json);
            }
        }
    }

    /// Broadcast recovery directive for a dead server to all surviving servers.
    pub fn broadcast_recovery(&self, dead_server_id: u16) {
        let mut survivors: Vec<ServerId> = self.servers.keys()
            .filter(|&&id| id > 0)
            .map(|&id| ServerId(id))
            .collect();
        survivors.sort_by_key(|s| s.0);

        let msg = CoordServerMsg::RecoverEntities { dead_server_id: ServerId(dead_server_id), survivors };
        if let Ok(json) = serde_json::to_string(&msg) {
            for entry in self.servers.values() {
                if entry.server_id > 0 {
                    let _ = entry.reply_tx.send(json.clone());
                }
            }
        }
    }

    /// Record the result of an authority transfer.
    /// Decrements in-flight counter on the source server.
    pub fn record_transfer_result(&mut self, source_server: u16, target_server: u16, success: bool) {
        if let Some(entry) = self.servers.get_mut(&source_server) {
            entry.in_flight_transfers = entry.in_flight_transfers.saturating_sub(1);
        }
        if success {
            tracing::debug!(
                source = source_server,
                target = target_server,
                "Transfer completed successfully"
            );
        } else {
            tracing::warn!(
                source = source_server,
                target = target_server,
                "Transfer failed (entity kept on source)"
            );
        }
    }

    /// Grace period after registration before a server is eligible as a rebalance target.
    const JOIN_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

    /// Check load balance and issue rebalance directives if needed.
    /// Returns true if a rebalance was triggered.
    pub fn try_rebalance(&mut self) -> bool {
        let now = std::time::Instant::now();

        // Collect server snapshots to avoid holding borrows on self.servers
        struct ServerSnap {
            id: u16,
            entity_count: u32,
            weighted_load: u32,
            in_flight: u32,
            registered_at: std::time::Instant,
        }
        let snaps: Vec<ServerSnap> = self.servers.values()
            .filter(|s| s.server_id > 0)
            .map(|s| ServerSnap {
                id: s.server_id,
                entity_count: s.entity_count,
                weighted_load: s.weighted_load,
                in_flight: s.in_flight_transfers,
                registered_at: s.registered_at,
            })
            .collect();

        if snaps.len() < 2 {
            return false;
        }

        let total_load: u32 = snaps.iter().map(|s| s.weighted_load).sum();
        if total_load == 0 {
            return false;
        }

        let counts: Vec<String> = snaps.iter()
            .map(|s| format!("S{}:{}(w{})", s.id, s.entity_count, s.weighted_load))
            .collect();
        tracing::debug!("Rebalance check: {} | total_weighted={total_load}", counts.join(" "));

        let avg = total_load as f32 / snaps.len() as f32;

        // Find most overloaded (must not already have in-flight transfers)
        let overloaded = snaps.iter()
            .filter(|s| s.in_flight == 0)
            .max_by_key(|s| s.weighted_load);
        let Some(overloaded) = overloaded else { return false };

        // Find most underloaded (must have passed join grace period)
        let underloaded = snaps.iter()
            .filter(|s| now.duration_since(s.registered_at) >= Self::JOIN_GRACE_PERIOD)
            .min_by_key(|s| s.weighted_load);
        let Some(underloaded) = underloaded else { return false };

        if overloaded.id == underloaded.id {
            return false;
        }

        let diff = overloaded.weighted_load as i32 - underloaded.weighted_load as i32;

        // Rebalance if diff > 20% of average and at least 10 weighted units apart
        if diff < 10 || (diff as f32) < avg * 0.2 {
            return false;
        }

        // Transfer enough entities to bring closer to average (cap at 10% of overloaded)
        let transfer_count = (diff as u32 / 2).min(overloaded.entity_count / 10).max(1);

        tracing::info!(
            "Rebalancing: S{} ({} entities) -> S{} ({} entities), transferring {}",
            overloaded.id,
            overloaded.entity_count,
            underloaded.id,
            underloaded.entity_count,
            transfer_count,
        );

        let overloaded_id = overloaded.id;
        let underloaded_id = underloaded.id;

        // Tell the overloaded server to transfer N entities to the underloaded server.
        let msg = CoordServerMsg::RebalanceDirective {
            entity_ids: Vec::new(), // empty = server picks
            target_server: ServerId(underloaded_id),
        };

        if let Some(entry) = self.servers.get(&overloaded_id) {
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = entry.reply_tx.send(json);
            }
        }

        // Track in-flight transfers on the source server
        if let Some(entry) = self.servers.get_mut(&overloaded_id) {
            entry.in_flight_transfers += 1;
        }

        true
    }

    /// Returns the WebSocket URL for the least-loaded sim-server, or None.
    /// When the server's registered host is `127.0.0.1` (local dev), `client_host`
    /// overrides it so the browser can reach it. When the server advertises a
    /// real IP (e.g. Pi deployment), we use that directly.
    pub fn get_least_loaded_ws_addr(&self, client_host: Option<&str>) -> Option<String> {
        self.servers
            .values()
            .filter(|s| s.server_id > 0)
            .min_by_key(|s| s.weighted_load)
            .map(|s| {
                let host = if s.host == "127.0.0.1" {
                    client_host.unwrap_or(&s.host)
                } else {
                    &s.host
                };
                let scheme = if s.tls { "wss" } else { "ws" };
                format!("{scheme}://{host}:{}", s.ws_port)
            })
    }

    /// Returns a JSON-serializable list of all registered sim-servers.
    pub fn get_server_list(&self) -> Vec<serde_json::Value> {
        let mut list: Vec<serde_json::Value> = self
            .servers
            .values()
            .filter(|s| s.server_id > 0)
            .map(|s| {
                serde_json::json!({
                    "server_id": s.server_id,
                    "host": s.host,
                    "ws_port": s.ws_port,
                    "udp_port": s.udp_port,
                    "metrics_port": 8100 + s.server_id,
                    "entity_count": s.entity_count,
                })
            })
            .collect();
        list.sort_by_key(|s| s["server_id"].as_u64().unwrap_or(0));
        list
    }

    /// Returns sorted list of sim-server IDs (excluding gateway).
    #[cfg(test)]
    pub fn sim_server_ids(&self) -> Vec<u16> {
        let mut ids: Vec<u16> = self.servers.keys().filter(|&&id| id > 0).copied().collect();
        ids.sort();
        ids
    }

    /// Build a PeerList and send it to every connected server.
    /// Each server receives a list excluding itself.
    /// Server ID 0 (gateway) is excluded from sim-server peer lists.
    pub fn broadcast_peer_list(&self) {
        // Only real sim-servers (id > 0) go in the peer list
        let sim_peers: Vec<PeerInfo> = self
            .servers
            .values()
            .filter(|s| s.server_id > 0)
            .map(|s| PeerInfo {
                server_id: ServerId(s.server_id),
                udp_addr: format!("{}:{}", s.host, s.udp_port),
                ws_addr: format!("{}:{}", s.host, s.ws_port),
            })
            .collect();

        for entry in self.servers.values() {
            // Exclude self from peer list
            let peers: Vec<PeerInfo> = sim_peers
                .iter()
                .filter(|p| p.server_id.0 != entry.server_id)
                .cloned()
                .collect();

            let msg = CoordServerMsg::PeerList { peers };
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = entry.reply_tx.send(json);
            }
        }
    }

    /// Send a keepalive to every connected server. Prevents sim-server read
    /// timeouts on stable clusters where no topology changes occur.
    pub fn broadcast_keepalive(&self) {
        if let Ok(json) = serde_json::to_string(&CoordServerMsg::Keepalive) {
            for entry in self.servers.values() {
                let _ = entry.reply_tx.send(json.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol::CoordServerMsg;

    fn make_tx() -> (mpsc::UnboundedSender<String>, mpsc::UnboundedReceiver<String>) {
        mpsc::unbounded_channel()
    }

    fn register_server(reg: &mut Registry, id: u16) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = make_tx();
        reg.register(id, 9100 + id, 7878 + id - 1, None, false, tx);
        rx
    }

    #[test]
    fn register_and_count() {
        let mut reg = Registry::new();
        assert_eq!(reg.server_count(), 0);
        let _rx = register_server(&mut reg, 1);
        assert_eq!(reg.server_count(), 1);
        let _rx = register_server(&mut reg, 2);
        assert_eq!(reg.server_count(), 2);
    }

    #[test]
    fn unregister_removes_server() {
        let mut reg = Registry::new();
        let _rx = register_server(&mut reg, 1);
        reg.unregister(1);
        assert_eq!(reg.server_count(), 0);
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let mut reg = Registry::new();
        reg.unregister(99);
        assert_eq!(reg.server_count(), 0);
    }

    #[test]
    fn heartbeat_updates_entity_count() {
        let mut reg = Registry::new();
        let _rx = register_server(&mut reg, 1);
        reg.heartbeat(1, 42, 0);
        assert_eq!(reg.servers.get(&1).unwrap().entity_count, 42);
    }

    #[test]
    fn heartbeat_nonexistent_server_is_noop() {
        let mut reg = Registry::new();
        reg.heartbeat(99, 100, 0); // should not panic
    }

    #[test]
    fn detect_dead_servers_fresh_heartbeat() {
        let mut reg = Registry::new();
        let _rx = register_server(&mut reg, 1);
        // Just registered — heartbeat is fresh
        assert!(reg.detect_dead_servers().is_empty());
    }

    #[test]
    fn detect_dead_servers_skips_gateway() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(0, 0, 4322, None, false, tx); // gateway = server_id 0

        // Even if gateway heartbeat is stale, it shouldn't be detected as dead
        // (gateway uses server_id 0, which is filtered)
        // Manually set stale heartbeat (well past the 15s timeout)
        if let Some(entry) = reg.servers.get_mut(&0) {
            entry.last_heartbeat = std::time::Instant::now() - std::time::Duration::from_secs(45);
        }
        assert!(reg.detect_dead_servers().is_empty());
    }

    #[test]
    fn rebalance_requires_two_servers() {
        let mut reg = Registry::new();
        let _rx = register_server(&mut reg, 1);
        reg.heartbeat(1, 100, 0);
        assert!(!reg.try_rebalance()); // need at least 2 servers
    }

    #[test]
    fn rebalance_no_entities() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        // Both have 0 entities
        assert!(!reg.try_rebalance());
    }

    #[test]
    fn rebalance_balanced_servers() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        reg.heartbeat(1, 50, 0);
        reg.heartbeat(2, 50, 0);
        assert!(!reg.try_rebalance()); // balanced — no rebalance
    }

    #[test]
    fn rebalance_below_minimum_threshold() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        reg.heartbeat(1, 12, 0);
        reg.heartbeat(2, 5, 0);
        // diff = 7, which is less than 10 minimum
        assert!(!reg.try_rebalance());
    }

    /// Backdate all servers' registered_at to bypass the join grace period in tests.
    fn age_all_servers(reg: &mut Registry) {
        let past = std::time::Instant::now() - std::time::Duration::from_secs(30);
        for entry in reg.servers.values_mut() {
            entry.registered_at = past;
        }
    }

    #[test]
    fn rebalance_triggers_when_imbalanced() {
        let mut reg = Registry::new();
        let mut rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        age_all_servers(&mut reg);
        reg.heartbeat(1, 100, 0);
        reg.heartbeat(2, 20, 0);
        // diff = 80, avg = 60, 20% of avg = 12 — both thresholds met
        assert!(reg.try_rebalance());

        // Server 1 (overloaded) should have received a RebalanceDirective
        let msg_json = rx1.try_recv().unwrap();
        let msg: CoordServerMsg = serde_json::from_str(&msg_json).unwrap();
        match msg {
            CoordServerMsg::RebalanceDirective { target_server, .. } => {
                assert_eq!(target_server, ServerId(2)); // targets the underloaded server
            }
            _ => panic!("expected RebalanceDirective, got {:?}", msg),
        }
    }

    #[test]
    fn broadcast_peer_list_excludes_self() {
        let mut reg = Registry::new();
        let mut rx1 = register_server(&mut reg, 1);
        let mut rx2 = register_server(&mut reg, 2);
        reg.broadcast_peer_list();

        // Server 1 should receive peer list with only server 2
        let msg1: CoordServerMsg = serde_json::from_str(&rx1.try_recv().unwrap()).unwrap();
        match msg1 {
            CoordServerMsg::PeerList { peers } => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].server_id, ServerId(2));
            }
            _ => panic!("expected PeerList"),
        }

        // Server 2 should receive peer list with only server 1
        let msg2: CoordServerMsg = serde_json::from_str(&rx2.try_recv().unwrap()).unwrap();
        match msg2 {
            CoordServerMsg::PeerList { peers } => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].server_id, ServerId(1));
            }
            _ => panic!("expected PeerList"),
        }
    }

    #[test]
    fn broadcast_peer_list_excludes_gateway_from_sim_peers() {
        let mut reg = Registry::new();
        let (tx0, mut rx0) = make_tx();
        reg.register(0, 0, 4322, None, false, tx0); // gateway
        let mut rx1 = register_server(&mut reg, 1);
        reg.broadcast_peer_list();

        // Gateway should receive a peer list (with server 1)
        let msg0: CoordServerMsg = serde_json::from_str(&rx0.try_recv().unwrap()).unwrap();
        match msg0 {
            CoordServerMsg::PeerList { peers } => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].server_id, ServerId(1));
            }
            _ => panic!("expected PeerList"),
        }

        // Server 1's peer list should NOT include gateway (server_id 0)
        let msg1: CoordServerMsg = serde_json::from_str(&rx1.try_recv().unwrap()).unwrap();
        match msg1 {
            CoordServerMsg::PeerList { peers } => {
                assert!(peers.is_empty()); // only other server is gateway, which is filtered
            }
            _ => panic!("expected PeerList"),
        }
    }

    #[test]
    fn broadcast_recovery_sends_to_all_survivors() {
        let mut reg = Registry::new();
        let mut rx1 = register_server(&mut reg, 1);
        let mut rx2 = register_server(&mut reg, 2);
        // Server 3 is dead (not registered)

        reg.broadcast_recovery(3);

        for rx in [&mut rx1, &mut rx2] {
            let msg: CoordServerMsg = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
            match msg {
                CoordServerMsg::RecoverEntities { dead_server_id, survivors } => {
                    assert_eq!(dead_server_id, ServerId(3));
                    assert_eq!(survivors, vec![ServerId(1), ServerId(2)]); // sorted
                }
                _ => panic!("expected RecoverEntities"),
            }
        }
    }

    #[test]
    fn drain_server_sends_directive() {
        let mut reg = Registry::new();
        let mut rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        let _rx3 = register_server(&mut reg, 3);

        reg.drain_server(1);

        let msg: CoordServerMsg = serde_json::from_str(&rx1.try_recv().unwrap()).unwrap();
        match msg {
            CoordServerMsg::DrainDirective { target_servers } => {
                // Should include servers 2 and 3, but not 1 (self)
                assert!(target_servers.contains(&ServerId(2)));
                assert!(target_servers.contains(&ServerId(3)));
                assert!(!target_servers.contains(&ServerId(1)));
            }
            _ => panic!("expected DrainDirective"),
        }
    }

    #[test]
    fn drain_server_no_targets_available() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        reg.drain_server(1); // only server — no targets
        // Should not panic, just logs a warning
    }

    #[test]
    fn register_with_custom_host() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, Some("10.0.0.5".to_string()), false, tx);
        assert_eq!(reg.servers.get(&1).unwrap().host, "10.0.0.5");
    }

    #[test]
    fn register_host_defaults_to_localhost() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx);
        assert_eq!(reg.servers.get(&1).unwrap().host, "127.0.0.1");
    }

    #[test]
    fn assign_returns_wss_when_tls() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, Some("10.0.0.5".to_string()), true, tx);
        let url = reg.get_least_loaded_ws_addr(None).unwrap();
        assert!(url.starts_with("wss://"), "expected wss:// but got {url}");
        assert_eq!(url, "wss://10.0.0.5:7878");
    }

    #[test]
    fn assign_returns_ws_when_no_tls() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, Some("10.0.0.5".to_string()), false, tx);
        let url = reg.get_least_loaded_ws_addr(None).unwrap();
        assert!(url.starts_with("ws://"), "expected ws:// but got {url}");
        assert_eq!(url, "ws://10.0.0.5:7878");
    }

    // ====================================================================
    // Additional coordinator tests
    // ====================================================================

    #[test]
    fn assign_returns_none_when_no_servers() {
        let reg = Registry::new();
        assert!(reg.get_least_loaded_ws_addr(None).is_none());
    }

    #[test]
    fn assign_returns_least_loaded_server() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        let _rx3 = register_server(&mut reg, 3);

        reg.heartbeat(1, 100, 0);
        reg.heartbeat(2, 10, 0);  // least loaded
        reg.heartbeat(3, 50, 0);

        let url = reg.get_least_loaded_ws_addr(None).unwrap();
        // Server 2 has ws_port = 7878 + 2 - 1 = 7879
        assert!(url.contains("7879"), "expected ws://...7879 for least loaded S2, got {}", url);
    }

    #[test]
    fn assign_localhost_replaced_by_client_host() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx); // host defaults to 127.0.0.1

        let url = reg.get_least_loaded_ws_addr(Some("192.168.1.50")).unwrap();
        assert!(url.contains("192.168.1.50"), "expected client_host override, got {}", url);
        assert!(!url.contains("127.0.0.1"));
    }

    #[test]
    fn assign_real_host_not_replaced() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        reg.register(1, 9101, 7878, Some("10.0.0.5".to_string()), false, tx);

        let url = reg.get_least_loaded_ws_addr(Some("192.168.1.50")).unwrap();
        assert!(url.contains("10.0.0.5"), "real host should not be overridden, got {}", url);
        assert!(!url.contains("192.168.1.50"));
    }

    #[test]
    fn duplicate_register_ignored() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 1); // duplicate, should be ignored
        assert_eq!(reg.server_count(), 1);
    }

    #[test]
    fn server_list_sorted_by_id() {
        let mut reg = Registry::new();
        let _rx3 = register_server(&mut reg, 3);
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);

        let list = reg.get_server_list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["server_id"], 1);
        assert_eq!(list[1]["server_id"], 2);
        assert_eq!(list[2]["server_id"], 3);
    }

    #[test]
    fn server_list_excludes_gateway() {
        let mut reg = Registry::new();
        let (tx0, _rx0) = make_tx();
        reg.register(0, 0, 4322, None, false, tx0); // gateway
        let _rx1 = register_server(&mut reg, 1);

        let list = reg.get_server_list();
        assert_eq!(list.len(), 1, "gateway should not appear in server list");
        assert_eq!(list[0]["server_id"], 1);
    }

    #[test]
    fn server_list_includes_entity_count() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        reg.heartbeat(1, 42, 0);

        let list = reg.get_server_list();
        assert_eq!(list[0]["entity_count"], 42);
    }

    #[test]
    fn server_list_includes_metrics_port() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 3);

        let list = reg.get_server_list();
        assert_eq!(list[0]["metrics_port"], 8103); // 8100 + server_id
    }

    #[test]
    fn rebalance_three_servers_picks_most_and_least() {
        let mut reg = Registry::new();
        let mut rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        let _rx3 = register_server(&mut reg, 3);
        age_all_servers(&mut reg);

        reg.heartbeat(1, 200, 0); // most loaded
        reg.heartbeat(2, 100, 0);
        reg.heartbeat(3, 10, 0);  // least loaded

        assert!(reg.try_rebalance());

        // S1 (most loaded) should get the directive targeting S3 (least loaded)
        let msg: CoordServerMsg = serde_json::from_str(&rx1.try_recv().unwrap()).unwrap();
        match msg {
            CoordServerMsg::RebalanceDirective { target_server, .. } => {
                assert_eq!(target_server, ServerId(3));
            }
            _ => panic!("expected RebalanceDirective"),
        }
    }

    #[test]
    fn detect_dead_servers_stale_heartbeat() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);

        // Manually set stale heartbeat (20 seconds ago, timeout is 15s)
        reg.servers.get_mut(&1).unwrap().last_heartbeat =
            std::time::Instant::now() - std::time::Duration::from_secs(20);

        let dead = reg.detect_dead_servers();
        assert_eq!(dead, vec![1]);
    }

    #[test]
    fn detect_dead_servers_mixed_fresh_and_stale() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);
        let _rx3 = register_server(&mut reg, 3);

        // S1 and S3 are stale (past 15s timeout), S2 is fresh
        reg.servers.get_mut(&1).unwrap().last_heartbeat =
            std::time::Instant::now() - std::time::Duration::from_secs(20);
        reg.servers.get_mut(&3).unwrap().last_heartbeat =
            std::time::Instant::now() - std::time::Duration::from_secs(30);

        let mut dead = reg.detect_dead_servers();
        dead.sort();
        assert_eq!(dead, vec![1, 3]);
    }

    #[test]
    fn recovery_survivors_sorted() {
        let mut reg = Registry::new();
        let mut rx3 = register_server(&mut reg, 3);
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);

        reg.broadcast_recovery(4);

        // All survivors should receive sorted survivor list
        let msg: CoordServerMsg = serde_json::from_str(&rx3.try_recv().unwrap()).unwrap();
        match msg {
            CoordServerMsg::RecoverEntities { survivors, .. } => {
                assert_eq!(survivors, vec![ServerId(1), ServerId(2), ServerId(3)], "survivors should be sorted");
            }
            _ => panic!("expected RecoverEntities"),
        }
    }

    #[test]
    fn sim_server_ids_excludes_gateway() {
        let mut reg = Registry::new();
        let (tx0, _rx0) = make_tx();
        reg.register(0, 0, 4322, None, false, tx0); // gateway
        let _rx1 = register_server(&mut reg, 1);
        let _rx2 = register_server(&mut reg, 2);

        let ids = reg.sim_server_ids();
        assert_eq!(ids, vec![1, 2]);
        assert!(!ids.contains(&0));
    }

    #[test]
    fn heartbeat_refreshes_stale_server() {
        let mut reg = Registry::new();
        let _rx1 = register_server(&mut reg, 1);

        // Make it stale (past the 15s timeout)
        reg.servers.get_mut(&1).unwrap().last_heartbeat =
            std::time::Instant::now() - std::time::Duration::from_secs(20);
        assert_eq!(reg.detect_dead_servers().len(), 1);

        // Heartbeat refreshes it
        reg.heartbeat(1, 50, 0);
        assert!(reg.detect_dead_servers().is_empty());
    }

    #[test]
    fn re_register_replaces_dead_connection() {
        let mut reg = Registry::new();
        let (tx1, rx1) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx1.clone());
        assert_eq!(reg.server_count(), 1);

        // Drop the receiver to simulate the old connection dying
        drop(rx1);
        assert!(tx1.is_closed());

        // Re-register with the same server_id should succeed
        let (tx2, _rx2) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx2.clone());
        assert_eq!(reg.server_count(), 1);

        // The new connection should be active
        assert!(!tx2.is_closed());
        // Verify the new tx is the one stored
        assert!(reg.servers.get(&1).unwrap().reply_tx.same_channel(&tx2));
    }

    #[test]
    fn duplicate_register_with_live_connection_rejected() {
        let mut reg = Registry::new();
        let (tx1, _rx1) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx1.clone());

        // Try to register again while the first connection is still alive
        let (tx2, _rx2) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx2.clone());
        assert_eq!(reg.server_count(), 1);

        // The original tx should still be the one stored
        assert!(reg.servers.get(&1).unwrap().reply_tx.same_channel(&tx1));
    }

    #[test]
    fn unregister_if_sender_matches_current() {
        let mut reg = Registry::new();
        let (tx1, _rx1) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx1.clone());

        // Unregister with matching sender should succeed
        assert!(reg.unregister_if_sender_matches(1, &tx1));
        assert_eq!(reg.server_count(), 0);
    }

    #[test]
    fn unregister_if_sender_matches_stale_ignored() {
        let mut reg = Registry::new();
        let (tx_old, rx_old) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx_old.clone());

        // Simulate re-registration: drop old rx, register new connection
        drop(rx_old);
        let (tx_new, _rx_new) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx_new.clone());

        // Now a stale Disconnected event arrives with the old tx
        // It should NOT unregister the new connection
        assert!(!reg.unregister_if_sender_matches(1, &tx_old));
        assert_eq!(reg.server_count(), 1);

        // The new connection should still be stored
        assert!(reg.servers.get(&1).unwrap().reply_tx.same_channel(&tx_new));
    }

    #[test]
    fn unregister_if_sender_matches_nonexistent() {
        let mut reg = Registry::new();
        let (tx, _rx) = make_tx();
        assert!(!reg.unregister_if_sender_matches(99, &tx));
    }

    #[test]
    fn unregister_drops_channel() {
        let mut reg = Registry::new();
        let (tx, mut rx) = make_tx();
        reg.register(1, 9101, 7878, None, false, tx);

        reg.unregister(1);
        assert_eq!(reg.server_count(), 0);

        // Channel should be closed after entry is dropped
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn heartbeat_from_unregistered_server_is_noop() {
        // After a server is unregistered via detect_dead_servers, heartbeats
        // from it should be silently handled (logged, not panicked).
        let mut reg = Registry::new();
        reg.heartbeat(99, 100, 0); // server 99 was never registered
        assert_eq!(reg.server_count(), 0);
    }

}
