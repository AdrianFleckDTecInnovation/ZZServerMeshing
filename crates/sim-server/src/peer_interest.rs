use crate::{peer_bounds::PEER_AOI_RADIUS, resources::PeerAddresses};
use glam::Vec3;
use shared::{entity::EntityState, interest::InterestMask};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

pub(crate) fn cell(p: Vec3) -> (i32, i32) {
    (
        (p.x / PEER_AOI_RADIUS).floor() as i32,
        (p.z / PEER_AOI_RADIUS).floor() as i32,
    )
}

pub(crate) struct Advert {
    pub source: SocketAddr,
    pub session: u64,
    pub tick: u64,
    pub cells: Box<InterestMask>,
}

#[derive(Default)]
pub(crate) struct Discovery {
    local: Option<InterestMask>,
    send: bool,
    peers: HashMap<u16, (Advert, u64)>,
}

impl Discovery {
    pub fn refresh(
        &mut self,
        tick: u64,
        peers: &PeerAddresses,
        incoming: &mut HashMap<u16, Advert>,
        entities: &[EntityState],
    ) {
        self.peers.retain(|id, (ad, received)| {
            peers.0.get(id) == Some(&ad.source) && tick.saturating_sub(*received) <= 60
        });
        for (id, ad) in incoming.drain() {
            if peers.0.get(&id) == Some(&ad.source)
                && self
                    .peers
                    .get(&id)
                    .is_none_or(|(old, _)| old.session != ad.session || ad.tick > old.tick)
            {
                self.peers.insert(id, (ad, tick));
            }
        }
        self.send = false;
        if peers.0.is_empty() || (self.local.is_some() && tick % 4 != 0) {
            return;
        }
        let mut interest = InterestMask::default();
        // Advertise an expanded cell once per occupied region. At most a 1 KiB
        // hint is sent even when a worker owns thousands of disjoint groups.
        let occupied: HashSet<_> = entities.iter().map(|e| cell(e.position)).collect();
        for (x, z) in occupied {
            for i in x.saturating_sub(1)..=x.saturating_add(1) {
                for j in z.saturating_sub(1)..=z.saturating_add(1) {
                    interest.observe(i, j);
                }
            }
        }
        self.send = self.local.as_ref() != Some(&interest) || tick % 20 == 0;
        self.local = Some(interest);
    }
    pub fn advertisement(&self) -> Option<&InterestMask> {
        self.local.as_ref().filter(|_| self.send)
    }
    pub fn remote(&self, peer: u16) -> Option<&InterestMask> {
        self.peers.get(&peer).map(|(ad, _)| &*ad.cells)
    }
}
