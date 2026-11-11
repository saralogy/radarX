//! Byte-budgeted LRU for decompressed clusters.

use std::collections::HashMap;
use std::sync::Arc;

pub(crate) enum Cluster {
    /// Uncompressed on disk: blobs are read straight from the file.
    Stored { data_start: u64, offsets: Vec<u64> },
    /// Decompressed into memory. `data` starts at the offset table.
    Loaded { data: Vec<u8>, offsets: Vec<u64> },
}

impl Cluster {
    pub(crate) fn offsets(&self) -> &[u64] {
        match self {
            Cluster::Stored { offsets, .. } | Cluster::Loaded { offsets, .. } => offsets,
        }
    }
    fn cost(&self) -> usize {
        match self {
            Cluster::Stored { offsets, .. } => offsets.len() * 8,
            Cluster::Loaded { data, offsets } => data.len() + offsets.len() * 8,
        }
    }
}

pub(crate) struct ClusterCache {
    budget: usize,
    used: usize,
    tick: u64,
    map: HashMap<u32, (Arc<Cluster>, u64)>,
}

impl ClusterCache {
    pub(crate) fn new(budget: usize) -> Self {
        Self { budget, used: 0, tick: 0, map: HashMap::new() }
    }

    pub(crate) fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict(None);
    }

    pub(crate) fn get(&mut self, n: u32) -> Option<Arc<Cluster>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&n).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    pub(crate) fn put(&mut self, n: u32, c: Arc<Cluster>) {
        self.tick += 1;
        self.used += c.cost();
        if let Some((old, _)) = self.map.insert(n, (c, self.tick)) {
            self.used -= old.cost();
        }
        self.evict(Some(n));
    }

    /// Drop least-recently-used clusters until under budget. `keep` is never
    /// evicted, so a single cluster larger than the budget still works.
    fn evict(&mut self, keep: Option<u32>) {
        while self.used > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(k, _)| Some(**k) != keep)
                .min_by_key(|(_, v)| v.1)
                .map(|(k, _)| *k);
            match victim.and_then(|k| self.map.remove(&k)) {
                Some((c, _)) => self.used -= c.cost(),
                None => break,
            }
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used
    }
}
