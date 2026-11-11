//! Automatic layout for the ER diagram.
//!
//! 1. Tables are split into connected groups (by foreign keys).
//! 2. Each group gets a layered ("Sugiyama-lite") layout: tables that reference nothing
//!    sit in the left-most layer, each table one layer right of the deepest table it
//!    references, and layers are ordered by neighbour barycenter to reduce crossings.
//! 3. Groups (including single unrelated tables) are packed onto shelves so the whole
//!    diagram roughly matches the viewport's aspect ratio instead of growing very tall.

use std::collections::HashMap;

pub const NODE_WIDTH: f32 = 240.;
pub const HEADER_HEIGHT: f32 = 30.;
pub const ROW_HEIGHT: f32 = 20.;
pub const MAX_VISIBLE_COLUMNS: usize = 14;
const H_GAP: f32 = 110.;
const V_GAP: f32 = 36.;
/// Gap between packed groups.
const GROUP_GAP: f32 = 70.;
/// Layers taller than this wrap into an extra column.
const MAX_COLUMN_HEIGHT: f32 = 1400.;

pub fn node_height(column_count: usize) -> f32 {
    let rows = column_count.min(MAX_VISIBLE_COLUMNS) + usize::from(column_count > MAX_VISIBLE_COLUMNS);
    HEADER_HEIGHT + rows.max(1) as f32 * ROW_HEIGHT + 8.
}

/// `edges` are `(from, to)` pairs meaning "`from` references `to`"; `aspect` is the
/// desired width / height of the whole diagram (usually the viewport's).
/// Returns the top-left position of every node.
pub fn layout(heights: &[f32], edges: &[(usize, usize)], aspect: f32) -> Vec<(f32, f32)> {
    let n = heights.len();
    let mut out = vec![(0., 0.); n];
    if n == 0 {
        return out;
    }
    let edges: Vec<(usize, usize)> = edges.iter().copied().filter(|&(a, b)| a != b && a < n && b < n).collect();

    // Lay out every connected group on its own, at local coordinates.
    let mut groups: Vec<(Vec<usize>, Vec<(f32, f32)>, f32, f32)> = components(n, &edges)
        .into_iter()
        .map(|members| {
            let local = layered(&members, heights, &edges);
            let w = local.iter().map(|p| p.0).fold(0., f32::max) + NODE_WIDTH;
            let h = members.iter().zip(&local).map(|(&i, p)| p.1 + heights[i]).fold(0., f32::max);
            (members, local, w, h)
        })
        .collect();

    // Bigger groups first; single tables keep alphabetical order after them.
    groups.sort_by(|a, b| {
        (a.0.len() == 1).cmp(&(b.0.len() == 1)).then_with(|| {
            if a.0.len() == 1 { a.0[0].cmp(&b.0[0]) } else { (b.2 * b.3).total_cmp(&(a.2 * a.3)) }
        })
    });

    // Shelf packing towards the requested aspect ratio.
    let area: f32 = groups.iter().map(|g| (g.2 + GROUP_GAP) * (g.3 + GROUP_GAP)).sum();
    let widest = groups.iter().map(|g| g.2).fold(0., f32::max);
    let target_width = (area * aspect.clamp(0.5, 4.)).sqrt().max(widest);
    let (mut x, mut y, mut shelf_height) = (0., 0., 0f32);
    for (members, local, w, h) in &groups {
        if x > 0. && x + w > target_width {
            x = 0.;
            y += shelf_height + GROUP_GAP;
            shelf_height = 0.;
        }
        for (&i, p) in members.iter().zip(local) {
            out[i] = (x + p.0, y + p.1);
        }
        x += w + GROUP_GAP;
        shelf_height = shelf_height.max(*h);
    }
    out
}

/// Connected components of the undirected FK graph, each sorted by node index.
fn components(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for &(a, b) in edges {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        parent[ra] = rb;
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = groups.into_values().collect();
    groups.sort_by_key(|g| g[0]);
    groups
}

/// Layered layout of one connected group; positions are relative to the group's origin.
fn layered(members: &[usize], heights: &[f32], edges: &[(usize, usize)]) -> Vec<(f32, f32)> {
    let local: HashMap<usize, usize> = members.iter().enumerate().map(|(li, &i)| (i, li)).collect();
    let m = members.len();
    let mut refs: Vec<Vec<usize>> = vec![Vec::new(); m];
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); m];
    for &(a, b) in edges {
        if let (Some(&a), Some(&b)) = (local.get(&a), local.get(&b)) {
            refs[a].push(b);
            neighbours[a].push(b);
            neighbours[b].push(a);
        }
    }

    // Longest-path layering; bounded iterations make reference cycles harmless.
    let mut layer = vec![0usize; m];
    for _ in 0..m.min(32) {
        let mut changed = false;
        for a in 0..m {
            for &b in &refs[a] {
                if layer[a] < layer[b] + 1 {
                    layer[a] = layer[b] + 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let layer_count = layer.iter().max().map_or(0, |l| l + 1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for i in 0..m {
        layers[layer[i]].push(i);
    }
    // Order within layers by neighbour barycenter (two sweeps).
    let mut order = vec![0f32; m];
    for l in &layers {
        for (pos, &i) in l.iter().enumerate() {
            order[i] = pos as f32;
        }
    }
    for _ in 0..2 {
        for l in layers.iter_mut() {
            let key = |i: &usize| {
                let ns = &neighbours[*i];
                if ns.is_empty() { order[*i] } else { ns.iter().map(|&j| order[j]).sum::<f32>() / ns.len() as f32 }
            };
            l.sort_by(|a, b| key(a).total_cmp(&key(b)));
            for (pos, &i) in l.iter().enumerate() {
                order[i] = pos as f32;
            }
        }
    }

    // Place layers left to right, wrapping tall layers into extra columns.
    let mut pos = vec![(0., 0.); m];
    let mut x = 0.;
    for l in &layers {
        let mut y = 0.;
        for &i in l {
            let h = heights[members[i]];
            if y > 0. && y + h > MAX_COLUMN_HEIGHT {
                x += NODE_WIDTH + H_GAP / 2.;
                y = 0.;
            }
            pos[i] = (x, y);
            y += h + V_GAP;
        }
        x += NODE_WIDTH + H_GAP;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_tables_are_left_of_referencing_ones() {
        // 0 -> 1 -> 2, 3 isolated
        let pos = layout(&[100.; 4], &[(0, 1), (1, 2)], 1.6);
        assert!(pos[2].0 < pos[1].0 && pos[1].0 < pos[0].0);
    }

    #[test]
    fn cycles_terminate() {
        let pos = layout(&[50.; 3], &[(0, 1), (1, 2), (2, 0)], 1.6);
        assert_eq!(pos.len(), 3);
    }

    #[test]
    fn many_unrelated_tables_fill_a_landscape_area() {
        let pos = layout(&[200.; 30], &[], 1.6);
        let width = pos.iter().map(|p| p.0).fold(0., f32::max) + NODE_WIDTH;
        let height = pos.iter().map(|p| p.1).fold(0., f32::max) + 200.;
        assert!(width > height, "expected landscape, got {width}x{height}");
        for i in 0..pos.len() {
            for j in i + 1..pos.len() {
                let overlap_x = (pos[i].0 - pos[j].0).abs() < NODE_WIDTH;
                let overlap_y = (pos[i].1 - pos[j].1).abs() < 200.;
                assert!(!(overlap_x && overlap_y), "nodes {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn nodes_do_not_overlap_in_a_column() {
        let pos = layout(&[100., 120., 80.], &[(0, 2), (1, 2)], 1.6);
        assert_eq!(pos[0].0, pos[1].0);
        assert!((pos[0].1 - pos[1].1).abs() >= 100.);
    }
}
