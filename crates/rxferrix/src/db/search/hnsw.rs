//! A Hierarchical Navigable Small World graph (Malkov & Yashunin, 2018) for cosine similarity: the approximate
//! nearest-neighbour structure behind `#[vector_index(.., kind = "hnsw")]`. The bookkeeping around it (which document
//! each node is, filters, tombstones, catching up with commits, persistence) is in [`super::ann`].
//!
//! ## Why not a crate
//!
//! The candidates were `hnsw_rs` (maintained, pure Rust), `instant-distance` and `usearch`. None fit:
//! `instant-distance` builds immutable indexes (no inserts after the build) and has had no release since 2023;
//! `usearch` is C++ (it needs a C++ toolchain, so the binary is no longer pure Rust plus SQLite); `hnsw_rs` has no
//! deletion, its dump format is native-endian and its loader `assert!`s (panics) on a malformed file, and a reloaded
//! index borrows the loader. RxFerrix needs tombstones, a persisted form it can validate and simply discard when wrong,
//! and searches running concurrently with inserts. The algorithm is small, so it lives here (~500 lines with tests).
//!
//! ## The structure
//!
//! Vectors are normalized when inserted, so the cosine distance is `1 − a·b`. Each node has a level drawn from an
//! exponential distribution (`⌊−ln U · 1/ln M⌋`); on each layer up to its level it links to at most `M` (layer 0:
//! `2M`) neighbours chosen with the paper's heuristic (a candidate is kept only if it is closer to the new node than
//! to every neighbour kept so far), and back-links are pruned with the same heuristic when a list overflows. Searches
//! descend greedily from the entry point and run a best-first search of width `ef` on layer 0.
//!
//! Every node's link lists sit behind their own `RwLock`, so inserts of nodes that are already allocated
//! ([`Graph::link`]) can run on many threads at once (the initial build uses all cores) and alongside searches; only
//! allocating new nodes ([`Graph::push`]) needs `&mut`. Deleted nodes are not removed: they stay as routing nodes
//! (tombstones are tracked by the caller, who filters them out of results) until the caller rebuilds the graph.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::{Read, Write};
use std::sync::{Mutex, RwLock};

use anyhow::{Context, bail, ensure};

/// Graph parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Links per node on layers above 0 (layer 0 allows `2 * m`).
    pub m: usize,
    /// Width of the candidate list when inserting.
    pub ef_construction: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params { m: 16, ef_construction: 128 }
    }
}

/// Levels are capped (a level above ~6 needs billions of nodes with M = 16).
const MAX_LEVEL: u8 = 16;

/// An HNSW graph over `dims`-dimensional unit vectors. Nodes are numbered from 0 in insertion order.
pub struct Graph {
    dims: usize,
    params: Params,
    /// Normalized vectors, node after node.
    vectors: Vec<f32>,
    levels: Vec<u8>,
    /// `links[node][layer]`: the node's neighbours on each layer up to its level.
    links: Vec<RwLock<Vec<Vec<u32>>>>,
    /// The entry point and its level.
    entry: Mutex<Option<(u32, u8)>>,
    /// Reusable visited sets for searches.
    visited: Mutex<Vec<Visited>>,
    rng: u64,
}

impl Graph {
    pub fn new(dims: usize, params: Params, seed: u64) -> Graph {
        Graph {
            dims,
            params,
            vectors: Vec::new(),
            levels: Vec::new(),
            links: Vec::new(),
            entry: Mutex::new(None),
            visited: Mutex::new(Vec::new()),
            rng: seed | 1,
        }
    }

    /// Build a graph over `vectors` (`dims` floats each, normalized here), inserting on up to `threads` threads.
    /// Node `i` is `vectors[i]`.
    pub fn build(dims: usize, params: Params, seed: u64, vectors: &[Vec<f32>], threads: usize) -> Graph {
        let mut g = Graph::new(dims, params, seed);
        for v in vectors {
            g.push(v);
        }
        g.link_all(0, threads);
        g
    }

    /// Link the allocated nodes from `from` on, on up to `threads` threads, then [reconnect](Graph::reconnect) any
    /// node the concurrent inserts left without a way in.
    pub fn link_all(&self, from: usize, threads: usize) {
        let n = self.len();
        if from >= n {
            return;
        }
        let threads = threads.clamp(1, 64).min(n - from);
        if threads == 1 {
            for i in from..n {
                self.link(i as u32);
            }
            return;
        }
        // The first node of an empty graph becomes the entry point; the others need one to start from.
        self.link(from as u32);
        let next = std::sync::atomic::AtomicUsize::new(from + 1);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        self.link(i as u32);
                    }
                });
            }
        });
        self.reconnect();
    }

    /// Give every node without an incoming link on layer 0 (the entry point aside) one, so searches can reach it.
    ///
    /// Concurrent inserts leave a few such nodes (3 in 288,000 with 64 threads on 4 cores in a debug build). Two close
    /// nodes inserted at the same time don't see each other and choose the same neighbours; when one of those lists
    /// overflows, the heuristic keeps the first node and prunes the second, which is closer to the first than to the
    /// neighbour. The heuristic counts on the first node linking to the second, but it doesn't: it chose its own links
    /// before the second one was reachable. A sequential insert would have found the first node and linked to it.
    fn reconnect(&self) {
        // Forcing a link into a full list drops another one, which may leave that node without a way in: a few rounds.
        for _ in 0..4 {
            let orphans = self.orphans();
            if orphans.is_empty() {
                return;
            }
            let mut visited = self.take_visited();
            for node in orphans {
                let q = self.vector(node).to_vec();
                let mut found = self.nearest(&q, self.params.ef_construction, &mut visited, Some(node));
                if found.is_empty() {
                    // The descent ended on the node itself (it is linked on an upper layer): its own neighbours.
                    found = self.neighbours(node, 0).into_iter().map(|n| (self.distance(&q, n), n)).collect();
                    found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                }
                // The closest node with room for one more link, else the closest one, dropping its farthest link.
                let max = self.max_links(0);
                let Some(&(_, closest)) = found.first() else { continue };
                let room = found.iter().take(self.params.m).find(|&&(_, n)| self.neighbours(n, 0).len() < max);
                let target = room.map_or(closest, |&(_, n)| n);
                let base = self.vector(target);
                let mut links = self.links[target as usize].write().unwrap_or_else(|e| e.into_inner());
                let list = &mut links[0];
                if list.len() >= max {
                    let farthest = (0..list.len())
                        .max_by(|&a, &b| {
                            let (da, db) =
                                (1.0 - dot(base, self.vector(list[a])), 1.0 - dot(base, self.vector(list[b])));
                            da.total_cmp(&db)
                        })
                        .expect("a full list");
                    list.swap_remove(farthest);
                }
                list.push(node);
            }
            self.put_visited(visited);
        }
    }

    /// The nodes no layer-0 list links to, except the entry point.
    fn orphans(&self) -> Vec<u32> {
        let mut linked = vec![false; self.len()];
        for links in &self.links {
            for &n in &links.read().unwrap_or_else(|e| e.into_inner())[0] {
                linked[n as usize] = true;
            }
        }
        if let Some((ep, _)) = *self.entry.lock().unwrap_or_else(|e| e.into_inner()) {
            linked[ep as usize] = true;
        }
        (0..self.len() as u32).filter(|&n| !linked[n as usize]).collect()
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn params(&self) -> Params {
        self.params
    }

    /// Number of nodes (deleted ones included).
    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// The normalized vector of `node`.
    pub fn vector(&self, node: u32) -> &[f32] {
        let i = node as usize * self.dims;
        &self.vectors[i..i + self.dims]
    }

    /// Allocate a node for `vector` (normalized here) with a random level, not linked yet: call [`Graph::link`].
    pub fn push(&mut self, vector: &[f32]) -> u32 {
        assert_eq!(vector.len(), self.dims, "vector dimensions");
        let node = self.levels.len() as u32;
        self.vectors.extend_from_slice(&normalized(vector));
        let level = self.random_level();
        self.levels.push(level);
        self.links.push(RwLock::new(vec![Vec::new(); level as usize + 1]));
        node
    }

    /// Allocate and link a node (single-threaded insert).
    pub fn insert(&mut self, vector: &[f32]) -> u32 {
        let node = self.push(vector);
        self.link(node);
        node
    }

    fn random_level(&mut self) -> u8 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let u = ((z >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let level = -u.ln() / (self.params.m as f64).ln();
        (level as u64).min(MAX_LEVEL as u64) as u8
    }

    fn distance(&self, q: &[f32], node: u32) -> f32 {
        1.0 - dot(q, self.vector(node))
    }

    fn neighbours(&self, node: u32, layer: usize) -> Vec<u32> {
        let links = self.links[node as usize].read().unwrap_or_else(|e| e.into_inner());
        links.get(layer).cloned().unwrap_or_default()
    }

    fn max_links(&self, layer: usize) -> usize {
        if layer == 0 { 2 * self.params.m } else { self.params.m }
    }

    /// Link an allocated node into the graph. Safe to call for different nodes on several threads at once.
    ///
    /// The node's own link lists are written for every layer (top-down) *before* any neighbour links back to it, so
    /// it only becomes reachable once it is fully linked. Adding the back-links layer by layer instead (as the paper's
    /// sequential algorithm does) made concurrent inserts fragile: another insert could reach the node on an upper
    /// layer while its lower-layer lists were still empty, descend into it, find nothing else, and link itself almost
    /// only to it. Under CPU contention (a thread preempted in the middle of a link) that left a fraction of a percent
    /// of the nodes practically unreachable, so even a search for a node's own vector missed it.
    pub fn link(&self, node: u32) {
        let level = self.levels[node as usize];
        let q = self.vector(node).to_vec();
        let entry = *self.entry.lock().unwrap_or_else(|e| e.into_inner());
        let Some((ep, top)) = entry else {
            let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if entry.is_none() {
                *entry = Some((node, level));
                return;
            }
            drop(entry);
            return self.link(node);
        };
        let mut visited = self.take_visited();
        let mut cur = (self.distance(&q, ep), ep);
        for layer in (level as usize + 1..=top as usize).rev() {
            cur = self.greedy(&q, cur, layer);
        }
        let mut eps = vec![cur];
        let mut back_links: Vec<(usize, Vec<(f32, u32)>)> = Vec::new();
        for layer in (0..=(level.min(top) as usize)).rev() {
            let found = self.search_layer(&q, &eps, self.params.ef_construction, layer, &mut visited, Some(node));
            let chosen = self.select_with(&found, self.params.m, true);
            {
                // Keep links other inserts may already have added (none should: the node isn't reachable before
                // its back-links below), pruning if the list overflows.
                let mut links = self.links[node as usize].write().unwrap_or_else(|e| e.into_inner());
                let mut list: Vec<u32> = chosen.iter().map(|&(_, n)| n).collect();
                for n in std::mem::take(&mut links[layer]) {
                    if !list.contains(&n) {
                        list.push(n);
                    }
                }
                if list.len() > self.max_links(layer) {
                    let mut cands: Vec<(f32, u32)> = list.iter().map(|&n| (self.distance(&q, n), n)).collect();
                    cands.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                    list = self.select(&cands, self.max_links(layer)).into_iter().map(|(_, n)| n).collect();
                }
                links[layer] = list;
            }
            back_links.push((layer, chosen));
            eps = found;
        }
        for (layer, chosen) in back_links {
            for (d, n) in chosen {
                self.add_link(n, node, d, layer);
            }
        }
        self.put_visited(visited);
        if level > top {
            let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if entry.is_none_or(|(_, l)| level > l) {
                *entry = Some((node, level));
            }
        }
    }

    /// Add `new` (at distance `d`) to `node`'s links on `layer`, pruning with the heuristic when the list is full.
    fn add_link(&self, node: u32, new: u32, d: f32, layer: usize) {
        let max = self.max_links(layer);
        let mut links = self.links[node as usize].write().unwrap_or_else(|e| e.into_inner());
        let Some(list) = links.get_mut(layer) else { return };
        if list.contains(&new) {
            return;
        }
        if list.len() < max {
            list.push(new);
            return;
        }
        let base = self.vector(node);
        let mut cands: Vec<(f32, u32)> = list.iter().map(|&n| (1.0 - dot(base, self.vector(n)), n)).collect();
        cands.push((d, new));
        cands.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        *list = self.select(&cands, max).into_iter().map(|(_, n)| n).collect();
    }

    /// The neighbour-selection heuristic: walk candidates from the closest, keep one only if it is closer to the base
    /// than to every candidate kept so far. `cands` is sorted by distance to the base. With `fill`, the closest
    /// discarded candidates then fill the list up to `m` (the paper's "keep pruned connections"), which keeps new
    /// nodes well connected.
    fn select_with(&self, cands: &[(f32, u32)], m: usize, fill: bool) -> Vec<(f32, u32)> {
        let mut kept: Vec<(f32, u32)> = Vec::with_capacity(m);
        let mut pruned: Vec<(f32, u32)> = Vec::new();
        for &(d, c) in cands {
            if kept.len() >= m {
                break;
            }
            let v = self.vector(c);
            if kept.iter().all(|&(_, k)| 1.0 - dot(v, self.vector(k)) > d) {
                kept.push((d, c));
            } else if fill {
                pruned.push((d, c));
            }
        }
        for p in pruned {
            if kept.len() >= m {
                break;
            }
            kept.push(p);
        }
        kept
    }

    fn select(&self, cands: &[(f32, u32)], m: usize) -> Vec<(f32, u32)> {
        self.select_with(cands, m, false)
    }

    /// Greedy walk on one layer towards `q`.
    fn greedy(&self, q: &[f32], mut cur: (f32, u32), layer: usize) -> (f32, u32) {
        loop {
            let mut changed = false;
            for n in self.neighbours(cur.1, layer) {
                let d = self.distance(q, n);
                if d < cur.0 {
                    cur = (d, n);
                    changed = true;
                }
            }
            if !changed {
                return cur;
            }
        }
    }

    /// Best-first search of width `ef` on one layer, from `eps`. Returns the closest nodes found, closest first
    /// (never `skip`, the node being inserted).
    fn search_layer(
        &self,
        q: &[f32],
        eps: &[(f32, u32)],
        ef: usize,
        layer: usize,
        visited: &mut Visited,
        skip: Option<u32>,
    ) -> Vec<(f32, u32)> {
        visited.reset(self.len());
        let mut candidates: BinaryHeap<std::cmp::Reverse<Cand>> = BinaryHeap::new();
        let mut results: BinaryHeap<Cand> = BinaryHeap::new();
        for &(d, n) in eps {
            if visited.insert(n) && Some(n) != skip {
                candidates.push(std::cmp::Reverse(Cand(d, n)));
                results.push(Cand(d, n));
            }
        }
        if let Some(s) = skip {
            visited.insert(s);
        }
        while let Some(std::cmp::Reverse(Cand(d, c))) = candidates.pop() {
            if results.len() >= ef && results.peek().is_some_and(|w| d > w.0) {
                break;
            }
            for n in self.neighbours(c, layer) {
                if !visited.insert(n) {
                    continue;
                }
                let dn = self.distance(q, n);
                if results.len() < ef || results.peek().is_some_and(|w| dn < w.0) {
                    candidates.push(std::cmp::Reverse(Cand(dn, n)));
                    results.push(Cand(dn, n));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
        results.into_sorted_vec().into_iter().map(|Cand(d, n)| (d, n)).collect()
    }

    /// The (up to) `k` nodes closest to `query` among those `accept` keeps, closest first, as `(distance, node)`,
    /// searching with width `max(ef, k)`. Rejected nodes are still traversed (deleted nodes route searches).
    pub fn search(&self, query: &[f32], k: usize, ef: usize, accept: impl Fn(u32) -> bool) -> Vec<(f32, u32)> {
        let q = normalized(query);
        let mut visited = self.take_visited();
        let found = self.nearest(&q, ef.max(k), &mut visited, None);
        self.put_visited(visited);
        found.into_iter().filter(|&(_, n)| accept(n)).take(k).collect()
    }

    /// The `ef` nodes closest to `q` (normalized) found on layer 0 after descending from the entry point, closest
    /// first, never `skip`.
    fn nearest(&self, q: &[f32], ef: usize, visited: &mut Visited, skip: Option<u32>) -> Vec<(f32, u32)> {
        let Some((ep, top)) = *self.entry.lock().unwrap_or_else(|e| e.into_inner()) else { return Vec::new() };
        let mut cur = (self.distance(q, ep), ep);
        for layer in (1..=top as usize).rev() {
            cur = self.greedy(q, cur, layer);
        }
        self.search_layer(q, &[cur], ef, 0, visited, skip)
    }

    fn take_visited(&self) -> Visited {
        self.visited.lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap_or_default()
    }

    fn put_visited(&self, v: Visited) {
        let mut pool = self.visited.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < 64 {
            pool.push(v);
        }
    }

    // ------------------------------------------------------------------------------------------------- persistence

    /// Write the graph (parameters, levels, links, entry point and vectors) in a little-endian binary format.
    pub fn write_to(&self, w: &mut impl Write) -> anyhow::Result<()> {
        let n = self.len();
        w.write_all(&(self.dims as u32).to_le_bytes())?;
        w.write_all(&(self.params.m as u32).to_le_bytes())?;
        w.write_all(&(self.params.ef_construction as u32).to_le_bytes())?;
        w.write_all(&self.rng.to_le_bytes())?;
        w.write_all(&(n as u64).to_le_bytes())?;
        match *self.entry.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((node, level)) => {
                w.write_all(&[1, level])?;
                w.write_all(&node.to_le_bytes())?;
            }
            None => w.write_all(&[0, 0, 0, 0, 0, 0])?,
        }
        for node in 0..n {
            let links = self.links[node].read().unwrap_or_else(|e| e.into_inner());
            w.write_all(&[self.levels[node]])?;
            for layer in links.iter() {
                w.write_all(&(layer.len() as u16).to_le_bytes())?;
                for &l in layer {
                    w.write_all(&l.to_le_bytes())?;
                }
            }
        }
        for chunk in self.vectors.chunks(4096) {
            let bytes: Vec<u8> = chunk.iter().flat_map(|f| f.to_le_bytes()).collect();
            w.write_all(&bytes)?;
        }
        Ok(())
    }

    /// Read a graph written by [`Graph::write_to`], checking its structure (an error for anything malformed).
    pub fn read_from(r: &mut impl Read) -> anyhow::Result<Graph> {
        let dims = read_u32(r)? as usize;
        let m = read_u32(r)? as usize;
        let ef_construction = read_u32(r)? as usize;
        let rng = read_u64(r)?;
        let n = read_u64(r)? as usize;
        ensure!((1..=super::MAX_DIMENSIONS).contains(&dims) && (2..=256).contains(&m), "bad graph parameters");
        ensure!(n <= u32::MAX as usize, "too many nodes");
        let mut head = [0u8; 2];
        r.read_exact(&mut head)?;
        let entry_node = read_u32(r)?;
        let mut g = Graph::new(dims, Params { m, ef_construction }, rng);
        g.rng = rng;
        let mut levels = Vec::with_capacity(n);
        let mut links = Vec::with_capacity(n);
        for _ in 0..n {
            let mut level = [0u8; 1];
            r.read_exact(&mut level)?;
            ensure!(level[0] <= MAX_LEVEL, "bad level");
            let mut layers = Vec::with_capacity(level[0] as usize + 1);
            for layer in 0..=level[0] as usize {
                let count = read_u16(r)? as usize;
                ensure!(count <= if layer == 0 { 2 * m } else { m }, "too many links");
                let mut list = Vec::with_capacity(count);
                for _ in 0..count {
                    list.push(read_u32(r)?);
                }
                layers.push(list);
            }
            levels.push(level[0]);
            links.push(layers);
        }
        // Links must point at existing nodes that live on that layer.
        for layers in &links {
            for (layer, list) in layers.iter().enumerate() {
                for &l in list {
                    ensure!((l as usize) < n && levels[l as usize] as usize >= layer, "dangling link");
                }
            }
        }
        let entry = match head[0] {
            0 => {
                ensure!(n == 0, "missing entry point");
                None
            }
            1 => {
                ensure!((entry_node as usize) < n && levels[entry_node as usize] == head[1], "bad entry point");
                ensure!(levels.iter().all(|&l| l <= head[1]), "entry point is not on the top layer");
                Some((entry_node, head[1]))
            }
            _ => bail!("bad entry point flag"),
        };
        let mut vectors = vec![0f32; n * dims];
        let mut buf = vec![0u8; 4 * 4096];
        for chunk in vectors.chunks_mut(4096) {
            let bytes = &mut buf[..chunk.len() * 4];
            r.read_exact(bytes).context("truncated vectors")?;
            for (f, b) in chunk.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *f = f32::from_le_bytes(*b);
            }
        }
        ensure!(vectors.iter().all(|f| f.is_finite()), "non-finite vector");
        g.vectors = vectors;
        g.levels = levels;
        g.links = links.into_iter().map(RwLock::new).collect();
        g.entry = Mutex::new(entry);
        Ok(g)
    }
}

fn read_u16(r: &mut impl Read) -> anyhow::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> anyhow::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> anyhow::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// `v / |v|` (a zero vector stays zero).
pub fn normalized(v: &[f32]) -> Vec<f32> {
    let norm = dot(v, v).sqrt();
    if norm > 0.0 && norm.is_finite() { v.iter().map(|x| x / norm).collect() } else { v.to_vec() }
}

/// The dot product, with AVX2/FMA when the CPU has them.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static FAST: OnceLock<bool> = OnceLock::new();
        if *FAST.get_or_init(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma")) {
            // SAFETY: the CPU supports AVX2 and FMA (checked above).
            return unsafe { dot_avx2(a, b) };
        }
    }
    dot_portable(a, b)
}

fn dot_portable(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        sum += x * y;
    }
    sum
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let mut i = 0;
    // SAFETY: every load reads 8 floats at `i..i + 8 <= n` from slices of at least `n` floats.
    unsafe {
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        while i + 16 <= n {
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), acc0);
            acc1 =
                _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 8)), _mm256_loadu_ps(b.as_ptr().add(i + 8)), acc1);
            i += 16;
        }
        while i + 8 <= n {
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), acc0);
            i += 8;
        }
        let acc = _mm256_add_ps(acc0, acc1);
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
        let mut sum: f32 = lanes.iter().sum();
        while i < n {
            sum += a[i] * b[i];
            i += 1;
        }
        sum
    }
}

/// A search candidate ordered by distance (then node, for determinism).
#[derive(Clone, Copy, PartialEq)]
struct Cand(f32, u32);

impl Eq for Cand {}

impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// A visited set reset in O(1) by bumping an epoch.
#[derive(Default)]
struct Visited {
    marks: Vec<u32>,
    epoch: u32,
}

impl Visited {
    fn reset(&mut self, n: usize) {
        if self.marks.len() < n {
            self.marks.resize(n, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.marks.iter_mut().for_each(|m| *m = 0);
            self.epoch = 1;
        }
    }

    /// Mark `n`; false if it was already marked.
    fn insert(&mut self, n: u32) -> bool {
        let i = n as usize;
        if i >= self.marks.len() {
            self.marks.resize(i + 1, 0);
        }
        if self.marks[i] == self.epoch {
            return false;
        }
        self.marks[i] = self.epoch;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_vectors(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        };
        (0..n).map(|_| (0..dims).map(|_| next()).collect()).collect()
    }

    fn exact(vectors: &[Vec<f32>], q: &[f32], k: usize) -> Vec<u32> {
        let q = normalized(q);
        let mut all: Vec<(f32, u32)> =
            vectors.iter().enumerate().map(|(i, v)| (1.0 - dot(&q, &normalized(v)), i as u32)).collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        all.into_iter().take(k).map(|(_, i)| i).collect()
    }

    fn recall(g: &Graph, vectors: &[Vec<f32>], queries: &[Vec<f32>], k: usize, ef: usize) -> f64 {
        let mut hits = 0;
        for q in queries {
            let truth = exact(vectors, q, k);
            let got: Vec<u32> = g.search(q, k, ef, |_| true).into_iter().map(|(_, n)| n).collect();
            hits += got.iter().filter(|n| truth.contains(n)).count();
        }
        hits as f64 / (queries.len() * k) as f64
    }

    #[test]
    fn dot_products_agree() {
        let v = random_vectors(3, 389, 5);
        let reference: f32 = v[0].iter().zip(&v[1]).map(|(a, b)| a * b).sum();
        assert!((dot(&v[0], &v[1]) - reference).abs() < 1e-3);
        assert!((dot_portable(&v[0], &v[1]) - reference).abs() < 1e-3);
        assert_eq!(normalized(&[0.0, 0.0]), vec![0.0, 0.0]);
        assert!((dot(&normalized(&v[2]), &normalized(&v[2])) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn serial_and_parallel_builds_have_high_recall() {
        let vectors = random_vectors(3000, 24, 1);
        let queries = random_vectors(50, 24, 2);
        let mut serial = Graph::new(24, Params::default(), 3);
        for v in &vectors {
            serial.insert(v);
        }
        let parallel = Graph::build(24, Params::default(), 3, &vectors, 8);
        for g in [&serial, &parallel] {
            let r = recall(g, &vectors, &queries, 10, 64);
            assert!(r >= 0.95, "recall {r}");
        }
        // An exact match is found first.
        let hit = parallel.search(&vectors[17], 1, 32, |_| true);
        assert_eq!(hit[0].1, 17);
        assert!(hit[0].0.abs() < 1e-5);
        // The accept predicate filters results but not routing.
        let odd = parallel.search(&queries[0], 10, 64, |n| n % 2 == 1);
        assert_eq!(odd.len(), 10);
        assert!(odd.iter().all(|(_, n)| n % 2 == 1));
        assert!(odd.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    /// Regression: oversubscribed parallel builds (many more threads than cores, so links are preempted midway) used
    /// to leave nodes unreachable (about 0.2% of them here: a search for their own vector missed them), and still did
    /// for about one in 100,000 under heavy contention. See `link` and `reconnect`.
    #[test]
    fn oversubscribed_parallel_builds_reach_every_node() {
        let vectors = random_vectors(4800, 24, 11);
        let mut misses = 0;
        for seed in 0..2 {
            let g = Graph::build(24, Params::default(), seed, &vectors, 64);
            assert_eq!(g.orphans(), Vec::<u32>::new(), "nodes without an incoming link");
            misses += (0..vectors.len()).filter(|&i| g.search(&vectors[i], 1, 128, |_| true)[0].1 != i as u32).count();
        }
        assert!(misses <= 1, "{misses} of {} self-searches missed", 2 * vectors.len());
    }

    #[test]
    fn reconnect_links_nodes_nothing_links_to() {
        let vectors = random_vectors(2000, 16, 5);
        let g = Graph::build(16, Params::default(), 7, &vectors, 4);
        assert!(g.orphans().is_empty());
        // Cut every link to a few nodes on every layer, as lost races would.
        let cut = [3u32, 400, 1999];
        for links in &g.links {
            for list in links.write().unwrap().iter_mut() {
                list.retain(|n| !cut.contains(n));
            }
        }
        let ep = g.entry.lock().unwrap().unwrap().0;
        let expected: Vec<u32> = cut.into_iter().filter(|&n| n != ep).collect();
        assert_eq!(g.orphans(), expected);
        g.reconnect();
        assert!(g.orphans().is_empty());
        for n in cut {
            assert_eq!(g.search(&vectors[n as usize], 1, 64, |_| true)[0].1, n);
        }
        assert!(g.links.iter().all(|l| l.read().unwrap()[0].len() <= g.max_links(0)));
    }

    #[test]
    #[ignore = "measurement; run with --ignored --nocapture"]
    fn self_recall() {
        for (n, dims) in [(8000, 24), (8000, 64)] {
            let vectors = random_vectors(n, dims, 11);
            let g = Graph::build(dims, Params::default(), 3, &vectors, 16);
            for ef in [32, 64, 96, 128] {
                let misses = (0..2000).filter(|&i| g.search(&vectors[i], 1, ef, |_| true)[0].1 != i as u32).count();
                let queries = random_vectors(200, dims, 99);
                println!(
                    "n {n} dims {dims} ef {ef}: self-miss {misses}/2000, recall@10 {:.3}",
                    recall(&g, &vectors, &queries, 10, ef)
                );
            }
        }
    }

    #[test]
    fn persistence_round_trips_and_rejects_garbage() {
        let vectors = random_vectors(500, 8, 9);
        let g = Graph::build(8, Params { m: 8, ef_construction: 32 }, 4, &vectors, 4);
        let mut bytes = Vec::new();
        g.write_to(&mut bytes).unwrap();
        let back = Graph::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(back.len(), g.len());
        assert_eq!(back.params(), g.params());
        let q = &vectors[3];
        assert_eq!(back.search(q, 5, 32, |_| true), g.search(q, 5, 32, |_| true));
        // Truncated or corrupted input is an error, never a panic.
        for cut in [0, 10, 40, bytes.len() / 2, bytes.len() - 1] {
            assert!(Graph::read_from(&mut &bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut bad = bytes.clone();
        bad[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Graph::read_from(&mut bad.as_slice()).is_err());
        let empty = Graph::new(8, Params::default(), 1);
        let mut bytes = Vec::new();
        empty.write_to(&mut bytes).unwrap();
        assert!(Graph::read_from(&mut bytes.as_slice()).unwrap().is_empty());
    }
}
