#![forbid(unsafe_code)]
use crate::{Direction, Position};
use std::collections::{HashMap, VecDeque};
const MAX_NODES: usize = 512;
const CARDINAL_COST: u32 = 10;
const DIAGONAL_COST: u32 = 25;
const CREATURE_PENALTY: u32 = 30;
#[derive(Debug, Clone)]
pub struct FindPathParams {
    pub full_search: bool,
    pub clear_sight: bool,
    pub max_search_dist: i32,
}
pub type FrozenPathingConditionCall = Box<dyn Fn(Position) -> bool>;
#[derive(Clone, Copy)]
struct AStarNode {
    parent: u16,
    /// f = g + h (total estimated cost). Set to u32::MAX when closed.
    f: u32,
    /// True path cost from start to this node (for node-reopening comparison).
    g: u32,
    x: u16,
    y: u16,
}
struct AStarNodes {
    nodes: Vec<AStarNode>,
    table: HashMap<(u16, u16), usize>,
}
impl AStarNodes {
    fn new() -> Self {
        Self {
            nodes: Vec::with_capacity(MAX_NODES),
            table: HashMap::with_capacity(MAX_NODES),
        }
    }
    fn get_best_node(&self) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.f < u32::MAX)
            .min_by_key(|(_, n)| n.f)
            .map(|(i, _)| i)
    }
    fn add_node(&mut self, x: u16, y: u16, g: u32, f: u32, parent: u16) -> bool {
        if self.nodes.len() >= MAX_NODES {
            return false;
        }
        let idx = self.nodes.len();
        self.nodes.push(AStarNode { parent, f, g, x, y });
        self.table.insert((x, y), idx);
        true
    }
    fn is_node_at(&self, x: u16, y: u16) -> bool {
        self.table.contains_key(&(x, y))
    }

    /// Update a node's parent, g, and f if this new path is at least as cheap.
    /// Uses `<=` on g so that equal-cost paths with better entry directions
    /// (which enable fewer wasted steps from pruning constraints) can replace
    /// earlier paths. Without this, two equal-cost routes to the same node
    /// freeze on the first entry direction, which may prune the critical
    /// forward step and force an expensive detour.
    /// Reopens closed nodes (f=u32::MAX) when a better or equal path is found.
    fn try_reopen(&mut self, x: u16, y: u16, g: u32, f: u32, parent: u16) -> bool {
        if let Some(&idx) = self.table.get(&(x, y)) {
            let existing = &self.nodes[idx];
            if g <= existing.g {
                self.nodes[idx].g = g;
                self.nodes[idx].f = f;
                self.nodes[idx].parent = parent;
                return true;
            }
        }
        false
    }
}
fn neighbors_with_pruning(dir: Option<Direction>) -> &'static [(i32, i32)] {
    // TFS `dirNeighbors[8][5][2]` from `reference/tfs/src/map.cpp:654` mapped
    // to Rust direction semantics: Rust `ed` is FROM parent TO child, TFS offset
    // is parent→child as well, but TFS indexes by the INCOMING direction
    // (i.e. the opposite of `ed`). The design doc maps each Rust direction to
    // the corresponding TFS table entry.
    //
    // Mapping verified against reference/tfs/src/map.cpp dirNeighbors[8][5][2]:
    match dir {
        None => &[
            (-1, -1),
            (0, -1),
            (1, -1),
            (-1, 0),
            (1, 0),
            (-1, 1),
            (0, 1),
            (1, 1),
        ],
        // Rust East → TFS DIRECTION_WEST [0]
        Some(Direction::East) => &[(-1, 0), (0, 1), (1, 0), (1, 1), (-1, 1)],
        // Rust West → TFS DIRECTION_EAST [1]
        Some(Direction::West) => &[(-1, 0), (0, 1), (0, -1), (-1, -1), (-1, 1)],
        // Rust South → TFS DIRECTION_NORTH [2]
        Some(Direction::South) => &[(-1, 0), (1, 0), (0, -1), (-1, -1), (1, -1)],
        // Rust North → TFS DIRECTION_SOUTH [3]
        Some(Direction::North) => &[(0, 1), (1, 0), (0, -1), (1, -1), (1, 1)],
        // Rust SouthEast → TFS DIRECTION_NORTHWEST [4]
        Some(Direction::SouthEast) => &[(1, 0), (0, -1), (-1, -1), (1, -1), (1, 1)],
        // Rust SouthWest → TFS DIRECTION_NORTHEAST [5]
        Some(Direction::SouthWest) => &[(-1, 0), (0, -1), (-1, -1), (1, -1), (-1, 1)],
        // Rust NorthEast → TFS DIRECTION_SOUTHWEST [6]
        Some(Direction::NorthEast) => &[(0, 1), (1, 0), (1, -1), (1, 1), (-1, 1)],
        // Rust NorthWest → TFS DIRECTION_SOUTHEAST [7]
        Some(Direction::NorthWest) => &[(-1, 0), (0, 1), (-1, -1), (1, 1), (-1, 1)],
    }
}
fn calculate_path(nodes: &[AStarNode], end_idx: usize) -> VecDeque<Direction> {
    let mut path: VecDeque<Direction> = VecDeque::new();
    let mut cur = end_idx;
    loop {
        let parent = nodes[cur].parent;
        if parent == u16::MAX {
            break;
        }
        let p = parent as usize;
        let dx = i32::from(nodes[cur].x) - i32::from(nodes[p].x);
        let dy = i32::from(nodes[cur].y) - i32::from(nodes[p].y);
        let dir = match (dx, dy) {
            (0, -1) => Direction::North,
            (1, 0) => Direction::East,
            (0, 1) => Direction::South,
            (-1, 0) => Direction::West,
            (1, -1) => Direction::NorthEast,
            (1, 1) => Direction::SouthEast,
            (-1, 1) => Direction::SouthWest,
            (-1, -1) => Direction::NorthWest,
            _ => Direction::North,
        };
        path.push_front(dir);
        cur = p;
    }
    path
}
fn heuristic(x1: u16, y1: u16, x2: u16, y2: u16) -> u32 {
    let dx = (i32::from(x1) - i32::from(x2)).unsigned_abs();
    let dy = (i32::from(y1) - i32::from(y2)).unsigned_abs();
    let (mi, ma) = (dx.min(dy), dx.max(dy));
    // Admissible octile heuristic: since DIAGONAL_COST > 2*CARDINAL_COST,
    // two cardinals (20) are cheaper than one diagonal (25). The cheapest
    // way to cover both axes is two cardinal steps, so the diagonal
    // component must use 2*CARDINAL_COST (not DIAGONAL_COST).
    mi * (2 * CARDINAL_COST) + (ma - mi) * CARDINAL_COST
}
pub fn get_path_matching(
    start: Position,
    target: Position,
    creatures: &[(u16, u16)],
    params: &FindPathParams,
    condition: &dyn Fn(Position) -> bool,
    is_walkable: &mut dyn FnMut(u16, u16) -> bool,
) -> VecDeque<Direction> {
    let mut n = AStarNodes::new();
    if !n.add_node(
        start.x,
        start.y,
        0, // g = 0 for start node
        heuristic(start.x, start.y, target.x, target.y),
        u16::MAX,
    ) {
        return VecDeque::new();
    }
    loop {
        let Some(bi) = n.get_best_node() else {
            return VecDeque::new();
        };
        let b = n.nodes[bi];
        n.nodes[bi].f = u32::MAX;
        if !params.full_search
            && (i32::from(b.x) - i32::from(start.x))
                .unsigned_abs()
                .max((i32::from(b.y) - i32::from(start.y)).unsigned_abs())
                > params.max_search_dist as u32
        {
            return VecDeque::new();
        }
        if condition(Position::new(b.x, b.y, start.z)) {
            return calculate_path(&n.nodes, bi);
        }
        let pg = b.f.saturating_sub(heuristic(b.x, b.y, target.x, target.y));
        let ed = if b.parent == u16::MAX {
            None
        } else {
            let p = &n.nodes[b.parent as usize];
            Some(
                match (
                    i32::from(b.x) - i32::from(p.x),
                    i32::from(b.y) - i32::from(p.y),
                ) {
                    (0, -1) => Direction::North,
                    (1, 0) => Direction::East,
                    (0, 1) => Direction::South,
                    (-1, 0) => Direction::West,
                    (1, -1) => Direction::NorthEast,
                    (1, 1) => Direction::SouthEast,
                    (-1, 1) => Direction::SouthWest,
                    (-1, -1) => Direction::NorthWest,
                    _ => Direction::North,
                },
            )
        };
        for &(ndx, ndy) in neighbors_with_pruning(ed) {
            let nx = match i32::from(b.x)
                .checked_add(ndx)
                .filter(|&v| (0..=i32::from(u16::MAX)).contains(&v))
            {
                Some(v) => v as u16,
                None => continue,
            };
            let ny = match i32::from(b.y)
                .checked_add(ndy)
                .filter(|&v| (0..=i32::from(u16::MAX)).contains(&v))
            {
                Some(v) => v as u16,
                None => continue,
            };
            if !is_walkable(nx, ny) {
                continue;
            }
            let sc = if ndx != 0 && ndy != 0 {
                DIAGONAL_COST
            } else {
                CARDINAL_COST
            };
            let occupied = creatures.iter().any(|&(cx, cy)| cx == nx && cy == ny);
            let step_cost = sc + if occupied { CREATURE_PENALTY } else { 0 };
            let child_g = pg + step_cost;
            let child_f = child_g + heuristic(nx, ny, target.x, target.y);
            if n.try_reopen(nx, ny, child_g, child_f, bi as u16) {
                continue;
            }
            if n.is_node_at(nx, ny) {
                continue;
            }
            if !params.full_search
                && (i32::from(nx) - i32::from(start.x))
                    .unsigned_abs()
                    .max((i32::from(ny) - i32::from(start.y)).unsigned_abs())
                    > params.max_search_dist as u32
            {
                continue;
            }
            if !n.add_node(nx, ny, child_g, child_f, bi as u16) {
                return VecDeque::new();
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_path() {
        let p = get_path_matching(
            Position::new(5, 5, 7),
            Position::new(8, 8, 7),
            &[],
            &FindPathParams {
                full_search: false,
                clear_sight: false,
                max_search_dist: 30,
            },
            &|p| p == Position::new(8, 8, 7),
            &mut |x, y| x < 20 && y < 20,
        );
        assert!(!p.is_empty());
    }

    #[test]
    fn blocked() {
        let p = get_path_matching(
            Position::new(5, 5, 7),
            Position::new(5, 7, 7),
            &[],
            &FindPathParams {
                full_search: false,
                clear_sight: false,
                max_search_dist: 30,
            },
            &|_| false,
            &mut |x, y| x == 5 && y == 5,
        );
        assert!(p.is_empty());
    }

    // -------------------------------------------------------------------------
    // Fix 3 — TFS neighbor pruning table alignment
    // -------------------------------------------------------------------------

    #[test]
    fn neighbors_with_pruning_no_parent_produces_all_eight() {
        let n = neighbors_with_pruning(None);
        assert_eq!(n.len(), 8);
        assert!(n.contains(&(-1, -1)));
        assert!(n.contains(&(1, 1)));
    }

    #[test]
    fn neighbors_with_pruning_east_matches_tfs_dir_neighbors_west() {
        // Rust East → parent is WEST of child → TFS DIRECTION_WEST [0]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::East)),
            &[(-1, 0), (0, 1), (1, 0), (1, 1), (-1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_west_matches_tfs_dir_neighbors_east() {
        // Rust West → parent is EAST of child → TFS DIRECTION_EAST [1]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::West)),
            &[(-1, 0), (0, 1), (0, -1), (-1, -1), (-1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_south_matches_tfs_dir_neighbors_north() {
        // Rust South → parent is NORTH of child → TFS DIRECTION_NORTH [2]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::South)),
            &[(-1, 0), (1, 0), (0, -1), (-1, -1), (1, -1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_north_matches_tfs_dir_neighbors_south() {
        // Rust North → parent is SOUTH of child → TFS DIRECTION_SOUTH [3]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::North)),
            &[(0, 1), (1, 0), (0, -1), (1, -1), (1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_northeast_matches_tfs_dir_neighbors_southwest() {
        // Rust NorthEast → parent is SOUTHWEST of child → TFS DIRECTION_SOUTHWEST [6]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::NorthEast)),
            &[(0, 1), (1, 0), (1, -1), (1, 1), (-1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_southeast_matches_tfs_dir_neighbors_northwest() {
        // Rust SouthEast → parent is NORTHWEST of child → TFS DIRECTION_NORTHWEST [4]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::SouthEast)),
            &[(1, 0), (0, -1), (-1, -1), (1, -1), (1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_southwest_matches_tfs_dir_neighbors_northeast() {
        // Rust SouthWest → parent is NORTHEAST of child → TFS DIRECTION_NORTHEAST [5]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::SouthWest)),
            &[(-1, 0), (0, -1), (-1, -1), (1, -1), (-1, 1)],
        );
    }

    #[test]
    fn neighbors_with_pruning_northwest_matches_tfs_dir_neighbors_southeast() {
        // Rust NorthWest → parent is SOUTHEAST of child → TFS DIRECTION_SOUTHEAST [7]
        assert_eq!(
            neighbors_with_pruning(Some(Direction::NorthWest)),
            &[(-1, 0), (0, 1), (-1, -1), (1, 1), (-1, 1)],
        );
    }
}
