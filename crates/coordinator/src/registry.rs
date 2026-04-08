use shared::protocol::{CoordServerMsg, PeerInfo};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub struct ServerEntry {
    pub server_id: u16,
    pub udp_port: u16,
    pub ws_port: u16,
    pub host: String,
    pub entity_count: u32,
    pub reply_tx: mpsc::UnboundedSender<String>,
    pub last_heartbeat: std::time::Instant,
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
        reply_tx: mpsc::UnboundedSender<String>,
    ) {
        self.servers.insert(
            server_id,
            ServerEntry {
                server_id,
                udp_port,
                ws_port,
                host: host.unwrap_or_else(|| "127.0.0.1".to_string()),
                entity_count: 0,
                reply_tx,
                last_heartbeat: std::time::Instant::now(),
            },
        );
    }

    pub fn unregister(&mut self, server_id: u16) {
        self.servers.remove(&server_id);
    }

    pub fn heartbeat(&mut self, server_id: u16, entity_count: u32) {
        if let Some(entry) = self.servers.get_mut(&server_id) {
            entry.entity_count = entity_count;
            entry.last_heartbeat = std::time::Instant::now();
        }
    }

    /// Detect servers that have missed heartbeats.
    pub fn detect_dead_servers(&self) -> Vec<u16> {
        let timeout = std::time::Duration::from_secs(5);
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
        let targets: Vec<u16> = self.servers.keys()
            .filter(|&&id| id != server_id && id > 0)
            .copied()
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
        let mut survivors: Vec<u16> = self.servers.keys()
            .filter(|&&id| id > 0)
            .copied()
            .collect();
        survivors.sort();

        let msg = CoordServerMsg::RecoverEntities { dead_server_id, survivors };
        if let Ok(json) = serde_json::to_string(&msg) {
            for entry in self.servers.values() {
                if entry.server_id > 0 {
                    let _ = entry.reply_tx.send(json.clone());
                }
            }
        }
    }

    /// Check load balance and issue rebalance directives if needed.
    /// Returns true if a rebalance was triggered.
    pub fn try_rebalance(&self) -> bool {
        let sim_servers: Vec<&ServerEntry> = self
            .servers
            .values()
            .filter(|s| s.server_id > 0)
            .collect();

        if sim_servers.len() < 2 {
            return false;
        }

        let total_entities: u32 = sim_servers.iter().map(|s| s.entity_count).sum();
        if total_entities == 0 {
            return false;
        }

        let counts: Vec<String> = sim_servers.iter()
            .map(|s| format!("S{}:{}", s.server_id, s.entity_count))
            .collect();
        tracing::debug!("Rebalance check: {} | total={total_entities}", counts.join(" "));

        let avg = total_entities as f32 / sim_servers.len() as f32;

        // Find most overloaded and most underloaded
        let overloaded = sim_servers.iter().max_by_key(|s| s.entity_count).unwrap();
        let underloaded = sim_servers.iter().min_by_key(|s| s.entity_count).unwrap();

        let diff = overloaded.entity_count as i32 - underloaded.entity_count as i32;

        // Rebalance if diff > 20% of average and at least 10 entities apart
        if diff < 10 || (diff as f32) < avg * 0.2 {
            return false;
        }

        // Transfer enough entities to bring closer to average (cap at 10% of overloaded)
        let transfer_count = (diff as u32 / 2).min(overloaded.entity_count / 10).max(1);

        tracing::info!(
            "Rebalancing: S{} ({} entities) -> S{} ({} entities), transferring {}",
            overloaded.server_id,
            overloaded.entity_count,
            underloaded.server_id,
            underloaded.entity_count,
            transfer_count,
        );

        // Tell the overloaded server to transfer N entities to the underloaded server.
        // The server will pick which entities to transfer (it knows positions for clustering).
        let msg = CoordServerMsg::RebalanceDirective {
            entity_ids: Vec::new(), // empty = server picks
            target_server: underloaded.server_id,
        };

        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = overloaded.reply_tx.send(json);
        }

        true
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
                server_id: s.server_id,
                udp_addr: format!("{}:{}", s.host, s.udp_port),
                ws_addr: format!("{}:{}", s.host, s.ws_port),
            })
            .collect();

        for entry in self.servers.values() {
            // Exclude self from peer list
            let peers: Vec<PeerInfo> = sim_peers
                .iter()
                .filter(|p| p.server_id != entry.server_id)
                .cloned()
                .collect();

            let msg = CoordServerMsg::PeerList { peers };
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = entry.reply_tx.send(json);
            }
        }
    }
}
