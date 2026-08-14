use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
  document::BoardDocument,
  element::{
    ElementError, ElementId, ElementPayload, RectangleLabelAnchor, RectangleLabelEdge,
    RectangleLabelLayout, RectangleLabelSide, RectanglePayload, canonical_rectangle_label_anchor,
    ordered_rectangle_label_tracks, raw_rectangle_label_layout,
  },
  geometry::{GeometryError, PointPx, RectPx, SizePx},
};

const COLLISION_EPSILON_PX: f32 = 0.01;
const MAX_REFLOW_LABELS: usize = 32;
const BEAM_WIDTH: usize = 128;
const MAX_BEAM_STATES: usize = 4096;
const MAX_CANDIDATES_PER_LABEL: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct RectangleLabelScene {
  pub canvas_size_px: SizePx,
  pub rectangles: Vec<RectangleLabelSceneItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RectangleLabelSceneItem {
  pub element_id: ElementId,
  pub z_index: i64,
  pub payload: RectanglePayload,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectangleLabelSolution {
  pub element_id: ElementId,
  pub preferred_anchor: RectangleLabelAnchor,
  pub actual_anchor: RectangleLabelAnchor,
}

#[derive(Debug, Clone)]
struct LabelNode {
  item: RectangleLabelSceneItem,
  distance: usize,
}

#[derive(Debug, Clone, Copy)]
struct Obstacle {
  bounds_px: RectPx,
  margin_px: f32,
}

#[derive(Debug, Clone)]
struct Candidate {
  anchor: RectangleLabelAnchor,
  bounds_px: RectPx,
  fallback_phase: u8,
  fallback_order: u8,
  preferred_distance_px: f32,
  current_distance_px: f32,
}

#[derive(Debug, Clone)]
struct BeamState {
  placements: Vec<Candidate>,
  overlap_area_px: f32,
  collision_count: usize,
  fallback_phase_cost: u32,
  fallback_order_cost: u32,
  preferred_distance_px: f32,
  current_distance_px: f32,
}

impl RectangleLabelScene {
  pub fn from_document(document: &BoardDocument) -> Self {
    Self {
      canvas_size_px: document.canvas_size_px,
      rectangles: document
        .elements
        .iter()
        .filter_map(|element| {
          let ElementPayload::Rectangle(payload) = &element.payload else {
            return None;
          };
          Some(RectangleLabelSceneItem {
            element_id: element.element_id,
            z_index: element.z_index,
            payload: payload.clone(),
          })
        })
        .collect(),
    }
  }

  pub fn new(
    canvas_size_px: SizePx,
    rectangles: Vec<RectangleLabelSceneItem>,
  ) -> Result<Self, ElementError> {
    canvas_size_px.validate()?;
    for item in &rectangles {
      item.payload.validate_for_reflow()?;
    }
    Ok(Self { canvas_size_px, rectangles })
  }

  fn item(&self, element_id: ElementId) -> Option<&RectangleLabelSceneItem> {
    self.rectangles.iter().find(|item| item.element_id == element_id)
  }
}

pub fn solve_rectangle_label_reflow(
  before: &RectangleLabelScene,
  after: &RectangleLabelScene,
  primary_id: ElementId,
  seed_ids: &[ElementId],
) -> Result<Vec<RectangleLabelSolution>, ElementError> {
  before.canvas_size_px.validate()?;
  after.canvas_size_px.validate()?;
  let distances = connected_distances(before, after, primary_id, seed_ids);
  if distances.is_empty() {
    return Ok(Vec::new());
  }

  let mut movable = after
    .rectangles
    .iter()
    .filter_map(|item| {
      let distance = *distances.get(&item.element_id)?;
      item.payload.label.visible_text()?;
      Some(LabelNode { item: item.clone(), distance })
    })
    .collect::<Vec<_>>();
  if movable.is_empty() {
    return Ok(Vec::new());
  }
  for node in &mut movable {
    let preferred = node.item.payload.preferred_label_anchor;
    node.item.payload.preferred_label_anchor =
      canonical_rectangle_label_anchor(&node.item.payload, after.canvas_size_px, preferred)?;
  }

  movable = capped_solution_members(movable, primary_id);
  let mut fixed_label_ids = collect_fixed_label_ids(after, &movable);

  let placements = match solve_beam(after, &movable, &fixed_label_ids, false)? {
    Some(placements) => placements,
    None => {
      let collision_fallback = solve_beam(after, &movable, &fixed_label_ids, true)?;
      let blockers = collision_fallback.as_ref().map_or_else(Vec::new, |placements| {
        fixed_label_blockers_for_placements(after, &movable, placements, &fixed_label_ids)
      });
      if !blockers.is_empty() {
        let expanded = expanded_movable_with_blockers(after, &movable, &blockers, primary_id);
        if expanded.len() > movable.len() {
          movable = expanded;
          fixed_label_ids = collect_fixed_label_ids(after, &movable);
          if let Some(placements) = solve_beam(after, &movable, &fixed_label_ids, false)? {
            return Ok(solutions_from_placements(&movable, placements));
          }
        }
      }
      if let Some(placements) =
        solve_beam_forced_primary_top_inside(after, &movable, &fixed_label_ids, primary_id)?
      {
        placements
      } else if let Some(placements) = collision_fallback {
        if placements.len() == movable.len() {
          placements
        } else {
          solve_beam(after, &movable, &fixed_label_ids, true)?.unwrap_or_default()
        }
      } else {
        solve_beam(after, &movable, &fixed_label_ids, true)?.unwrap_or_default()
      }
    }
  };

  Ok(solutions_from_placements(&movable, placements))
}

fn solutions_from_placements(
  movable: &[LabelNode],
  placements: Vec<Candidate>,
) -> Vec<RectangleLabelSolution> {
  movable
    .iter()
    .zip(placements)
    .map(|(node, candidate)| RectangleLabelSolution {
      element_id: node.item.element_id,
      preferred_anchor: node.item.payload.preferred_label_anchor,
      actual_anchor: candidate.anchor,
    })
    .collect()
}

fn capped_solution_members(mut movable: Vec<LabelNode>, primary_id: ElementId) -> Vec<LabelNode> {
  movable.sort_by(compare_reflow_members);
  movable.truncate(MAX_REFLOW_LABELS);
  movable.sort_by(|left, right| compare_solution_order(left, right, primary_id));
  movable
}

fn collect_fixed_label_ids(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
) -> HashSet<ElementId> {
  let moving_ids = movable.iter().map(|node| node.item.element_id).collect::<HashSet<_>>();
  scene
    .rectangles
    .iter()
    .filter(|item| !moving_ids.contains(&item.element_id))
    .filter(|item| item.payload.label.visible_text().is_some())
    .map(|item| item.element_id)
    .collect()
}

trait RectanglePayloadReflowExt {
  fn validate_for_reflow(&self) -> Result<(), ElementError>;
}

impl RectanglePayloadReflowExt for RectanglePayload {
  fn validate_for_reflow(&self) -> Result<(), ElementError> {
    self.validate_for_layout()
  }
}

fn connected_distances(
  before: &RectangleLabelScene,
  after: &RectangleLabelScene,
  primary_id: ElementId,
  seed_ids: &[ElementId],
) -> HashMap<ElementId, usize> {
  let mut ids = before
    .rectangles
    .iter()
    .chain(after.rectangles.iter())
    .map(|item| item.element_id)
    .collect::<Vec<_>>();
  ids.sort_by_key(|id| id.as_uuid().as_u128());
  ids.dedup();
  let id_to_index =
    ids.iter().enumerate().map(|(index, id)| (*id, index)).collect::<HashMap<_, _>>();
  let mut adjacency = vec![Vec::new(); ids.len()];
  for scene in [before, after] {
    for left_index in 0..scene.rectangles.len() {
      for right_index in left_index + 1..scene.rectangles.len() {
        let left = &scene.rectangles[left_index];
        let right = &scene.rectangles[right_index];
        if rectangles_connected(scene.canvas_size_px, left, right) {
          let Some(&left_node) = id_to_index.get(&left.element_id) else {
            continue;
          };
          let Some(&right_node) = id_to_index.get(&right.element_id) else {
            continue;
          };
          adjacency[left_node].push(right_node);
          adjacency[right_node].push(left_node);
        }
      }
    }
  }

  let mut queue = VecDeque::new();
  let mut distances = HashMap::new();
  for id in std::iter::once(primary_id).chain(seed_ids.iter().copied()) {
    if let Some(&index) = id_to_index.get(&id)
      && distances.insert(id, 0).is_none()
    {
      queue.push_back(index);
    }
  }

  while let Some(index) = queue.pop_front() {
    let distance = distances[&ids[index]];
    let mut neighbors = adjacency[index].clone();
    neighbors.sort_by_key(|neighbor| ids[*neighbor].as_uuid().as_u128());
    for neighbor in neighbors {
      if let std::collections::hash_map::Entry::Vacant(entry) = distances.entry(ids[neighbor]) {
        entry.insert(distance + 1);
        queue.push_back(neighbor);
      }
    }
  }
  distances
}

fn rectangles_connected(
  canvas_size_px: SizePx,
  left: &RectangleLabelSceneItem,
  right: &RectangleLabelSceneItem,
) -> bool {
  let left_body = rectangle_body(&left.payload);
  let right_body = rectangle_body(&right.payload);
  if left_body.intersects(right_body) {
    return true;
  }

  let left_labels = label_connection_bounds(canvas_size_px, &left.payload);
  let right_labels = label_connection_bounds(canvas_size_px, &right.payload);
  for label in &left_labels {
    if rects_conflict(label.bounds_px, right_body, left.payload.label.anchor_offset_px) {
      return true;
    }
  }
  for label in &right_labels {
    if rects_conflict(label.bounds_px, left_body, right.payload.label.anchor_offset_px) {
      return true;
    }
  }
  for left_label in &left_labels {
    for right_label in &right_labels {
      let margin = left.payload.label.anchor_offset_px.max(right.payload.label.anchor_offset_px);
      if rects_conflict(left_label.bounds_px, right_label.bounds_px, margin) {
        return true;
      }
    }
  }
  false
}

fn label_connection_bounds(
  canvas_size_px: SizePx,
  payload: &RectanglePayload,
) -> Vec<RectangleLabelLayout> {
  if payload.label.visible_text().is_none() {
    return Vec::new();
  }
  [payload.label_anchor, payload.preferred_label_anchor]
    .into_iter()
    .filter_map(|anchor| raw_rectangle_label_layout(payload, anchor, canvas_size_px).ok())
    .collect()
}

fn solve_beam(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  fixed_label_ids: &HashSet<ElementId>,
  allow_collisions: bool,
) -> Result<Option<Vec<Candidate>>, ElementError> {
  solve_beam_with_forced_primary(scene, movable, fixed_label_ids, allow_collisions, None)
}

fn solve_beam_forced_primary_top_inside(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  fixed_label_ids: &HashSet<ElementId>,
  primary_id: ElementId,
) -> Result<Option<Vec<Candidate>>, ElementError> {
  solve_beam_with_forced_primary(scene, movable, fixed_label_ids, true, Some(primary_id))
}

fn solve_beam_with_forced_primary(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  fixed_label_ids: &HashSet<ElementId>,
  allow_collisions: bool,
  force_top_inside_id: Option<ElementId>,
) -> Result<Option<Vec<Candidate>>, ElementError> {
  let mut candidate_lists = Vec::with_capacity(movable.len());
  for node in movable {
    let fixed = fixed_obstacles(scene, node.item.element_id, fixed_label_ids);
    let mut candidates =
      candidates_for_label(scene.canvas_size_px, &node.item.payload, &fixed, !allow_collisions)?;
    if candidates.is_empty() && !allow_collisions {
      candidates = candidates_for_label(scene.canvas_size_px, &node.item.payload, &fixed, false)?;
    }
    if force_top_inside_id == Some(node.item.element_id) {
      candidates.retain(|candidate| {
        candidate.anchor.edge == RectangleLabelEdge::Top
          && candidate.anchor.side == RectangleLabelSide::Inside
      });
    }
    if candidates.is_empty() {
      return Ok(None);
    }
    candidate_lists.push(candidates);
  }

  let mut states = vec![BeamState::empty()];
  for (index, candidates) in candidate_lists.iter().enumerate() {
    let mut next = Vec::new();
    for state in &states {
      for candidate in candidates {
        let mut next_state = state.clone();
        let (overlap, collisions) = incremental_penalty(
          scene,
          movable,
          index,
          candidate,
          &next_state.placements,
          fixed_label_ids,
        );
        if !allow_collisions && collisions > 0 {
          continue;
        }
        next_state.push(candidate.clone(), overlap, collisions);
        next.push(next_state);
      }
    }
    if next.is_empty() {
      return Ok(None);
    }
    next.sort_by(compare_states);
    next.truncate(MAX_BEAM_STATES);
    next.truncate(BEAM_WIDTH);
    states = next;
  }

  states.sort_by(compare_states);
  Ok(states.into_iter().next().map(|state| state.placements))
}

fn fixed_label_blockers_for_placements(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  placements: &[Candidate],
  fixed_label_ids: &HashSet<ElementId>,
) -> Vec<ElementId> {
  let mut blockers = Vec::new();
  for (node, candidate) in movable.iter().zip(placements) {
    let owner = node.item.element_id;
    let offset = node.item.payload.label.anchor_offset_px;
    for fixed_id in fixed_label_ids {
      if *fixed_id == owner {
        continue;
      }
      let Some(item) = scene.item(*fixed_id) else {
        continue;
      };
      let Some(bounds) = actual_label_bounds(scene.canvas_size_px, &item.payload) else {
        continue;
      };
      let margin = offset.max(item.payload.label.anchor_offset_px);
      if rects_conflict(candidate.bounds_px, bounds, margin) {
        blockers.push(*fixed_id);
      }
    }
  }
  blockers.sort_by(|left, right| compare_fixed_label_ids(scene, *left, *right));
  blockers.dedup();
  blockers
}

fn expanded_movable_with_blockers(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  blockers: &[ElementId],
  primary_id: ElementId,
) -> Vec<LabelNode> {
  let mut expanded = movable.to_vec();
  let mut moving_ids = expanded.iter().map(|node| node.item.element_id).collect::<HashSet<_>>();
  for blocker in blockers {
    if expanded.len() >= MAX_REFLOW_LABELS || !moving_ids.insert(*blocker) {
      continue;
    }
    let Some(item) = scene.item(*blocker) else {
      continue;
    };
    if item.payload.label.visible_text().is_none() {
      continue;
    }
    expanded.push(LabelNode { item: item.clone(), distance: usize::MAX });
  }
  expanded.sort_by(|left, right| compare_solution_order(left, right, primary_id));
  expanded
}

impl BeamState {
  fn empty() -> Self {
    Self {
      placements: Vec::new(),
      overlap_area_px: 0.0,
      collision_count: 0,
      fallback_phase_cost: 0,
      fallback_order_cost: 0,
      preferred_distance_px: 0.0,
      current_distance_px: 0.0,
    }
  }

  fn push(&mut self, candidate: Candidate, overlap_area_px: f32, collision_count: usize) {
    self.overlap_area_px += overlap_area_px;
    self.collision_count += collision_count;
    self.fallback_phase_cost += u32::from(candidate.fallback_phase);
    self.fallback_order_cost += u32::from(candidate.fallback_order);
    self.preferred_distance_px += candidate.preferred_distance_px;
    self.current_distance_px += candidate.current_distance_px;
    self.placements.push(candidate);
  }
}

fn incremental_penalty(
  scene: &RectangleLabelScene,
  movable: &[LabelNode],
  index: usize,
  candidate: &Candidate,
  previous: &[Candidate],
  fixed_label_ids: &HashSet<ElementId>,
) -> (f32, usize) {
  let owner = movable[index].item.element_id;
  let offset = movable[index].item.payload.label.anchor_offset_px;
  let mut overlap = 0.0;
  let mut collisions = 0;

  for item in &scene.rectangles {
    if item.element_id == owner {
      continue;
    }
    let margin = offset;
    let body = rectangle_body(&item.payload);
    if rects_conflict(candidate.bounds_px, body, margin) {
      collisions += 1;
      overlap += rect_overlap_area(candidate.bounds_px, body, margin);
    }
  }

  for fixed_id in fixed_label_ids {
    let Some(item) = scene.item(*fixed_id) else {
      continue;
    };
    if item.element_id == owner {
      continue;
    }
    let Some(bounds) = actual_label_bounds(scene.canvas_size_px, &item.payload) else {
      continue;
    };
    let margin = offset.max(item.payload.label.anchor_offset_px);
    if rects_conflict(candidate.bounds_px, bounds, margin) {
      collisions += 1;
      overlap += rect_overlap_area(candidate.bounds_px, bounds, margin);
    }
  }

  for (previous_index, previous_candidate) in previous.iter().enumerate() {
    let previous_node = &movable[previous_index];
    let margin = offset.max(previous_node.item.payload.label.anchor_offset_px);
    if rects_conflict(candidate.bounds_px, previous_candidate.bounds_px, margin) {
      collisions += 1;
      overlap += rect_overlap_area(candidate.bounds_px, previous_candidate.bounds_px, margin);
    }
  }

  (overlap, collisions)
}

fn fixed_obstacles(
  scene: &RectangleLabelScene,
  owner: ElementId,
  fixed_label_ids: &HashSet<ElementId>,
) -> Vec<Obstacle> {
  let Some(owner_item) = scene.item(owner) else {
    return Vec::new();
  };
  let owner_offset = owner_item.payload.label.anchor_offset_px;
  let mut obstacles = Vec::new();
  for item in &scene.rectangles {
    if item.element_id == owner {
      continue;
    }
    obstacles.push(Obstacle { bounds_px: rectangle_body(&item.payload), margin_px: owner_offset });
    if fixed_label_ids.contains(&item.element_id)
      && let Some(bounds_px) = actual_label_bounds(scene.canvas_size_px, &item.payload)
    {
      obstacles.push(Obstacle {
        bounds_px,
        margin_px: owner_offset.max(item.payload.label.anchor_offset_px),
      });
    }
  }
  obstacles
}

fn candidates_for_label(
  canvas_size_px: SizePx,
  payload: &RectanglePayload,
  fixed_obstacles: &[Obstacle],
  avoid_fixed_obstacles: bool,
) -> Result<Vec<Candidate>, ElementError> {
  let current_center =
    raw_rectangle_label_layout(payload, payload.label_anchor, canvas_size_px)?.bounds_px.center();
  let preferred_center =
    raw_rectangle_label_layout(payload, payload.preferred_label_anchor, canvas_size_px)?
      .bounds_px
      .center();
  let mut candidates = Vec::new();
  for track in ordered_rectangle_label_tracks(canvas_size_px, payload) {
    let anchor = RectangleLabelAnchor::new(track.edge, track.side, 0.0);
    let sample = raw_rectangle_label_layout(payload, anchor, canvas_size_px)?;
    if sample.bounds_px.width() > canvas_size_px.width_px as f32 + COLLISION_EPSILON_PX
      || sample.bounds_px.height() > canvas_size_px.height_px as f32 + COLLISION_EPSILON_PX
    {
      return Err(ElementError::Geometry(GeometryError::GeometryTooLargeForCanvas));
    }
    let intervals = legal_track_intervals(
      canvas_size_px,
      payload,
      track.edge,
      track.side,
      sample.bounds_px.width(),
      sample.bounds_px.height(),
      if avoid_fixed_obstacles { fixed_obstacles } else { &[] },
    );
    for interval in intervals {
      let projected_preferred = position_for_center(
        payload,
        track.edge,
        sample.bounds_px.width(),
        sample.bounds_px.height(),
        preferred_center,
      );
      let projected_current = position_for_center(
        payload,
        track.edge,
        sample.bounds_px.width(),
        sample.bounds_px.height(),
        current_center,
      );
      for position in [
        projected_preferred.clamp(interval.0, interval.1),
        projected_current.clamp(interval.0, interval.1),
        interval.0,
        interval.1,
      ] {
        let anchor = RectangleLabelAnchor::new(track.edge, track.side, position.clamp(0.0, 1.0));
        if candidates.iter().any(|candidate: &Candidate| same_anchor(candidate.anchor, anchor)) {
          continue;
        }
        let layout = raw_rectangle_label_layout(payload, anchor, canvas_size_px)?;
        if !canvas_size_px.bounds().contains_rect(layout.bounds_px) {
          continue;
        }
        if avoid_fixed_obstacles
          && fixed_obstacles.iter().any(|obstacle| {
            rects_conflict(layout.bounds_px, obstacle.bounds_px, obstacle.margin_px)
          })
        {
          continue;
        }
        candidates.push(Candidate {
          anchor,
          bounds_px: layout.bounds_px,
          fallback_phase: track.fallback_phase,
          fallback_order: track.fallback_order,
          preferred_distance_px: layout.bounds_px.center().distance_to(preferred_center),
          current_distance_px: layout.bounds_px.center().distance_to(current_center),
        });
      }
    }
  }
  candidates.sort_by(compare_candidates);
  candidates.truncate(MAX_CANDIDATES_PER_LABEL);
  Ok(candidates)
}

fn legal_track_intervals(
  canvas_size_px: SizePx,
  payload: &RectanglePayload,
  edge: RectangleLabelEdge,
  side: RectangleLabelSide,
  width_px: f32,
  height_px: f32,
  obstacles: &[Obstacle],
) -> Vec<(f32, f32)> {
  let body = rectangle_body(payload);
  let gap = payload.label.anchor_offset_px;
  let canvas = canvas_size_px.bounds();
  let mut intervals = match edge {
    RectangleLabelEdge::Top | RectangleLabelEdge::Bottom => {
      let y_px = horizontal_track_y(body, edge, side, gap, height_px);
      if y_px < canvas.min.y_px || y_px + height_px > canvas.max.y_px || body.width() <= 0.0 {
        return Vec::new();
      }
      let lo = ((canvas.min.x_px - body.min.x_px - gap) / body.width()).clamp(0.0, 1.0);
      let hi = ((canvas.max.x_px - width_px - body.min.x_px - gap) / body.width()).clamp(0.0, 1.0);
      if lo <= hi { vec![(lo, hi)] } else { Vec::new() }
    }
    RectangleLabelEdge::Left | RectangleLabelEdge::Right => {
      let x_px = vertical_track_x(body, edge, side, gap, width_px);
      if x_px < canvas.min.x_px || x_px + width_px > canvas.max.x_px || body.height() <= 0.0 {
        return Vec::new();
      }
      let lo = ((canvas.min.y_px - body.min.y_px - gap) / body.height()).clamp(0.0, 1.0);
      let hi =
        ((canvas.max.y_px - height_px - body.min.y_px - gap) / body.height()).clamp(0.0, 1.0);
      if lo <= hi { vec![(lo, hi)] } else { Vec::new() }
    }
  };

  for obstacle in obstacles {
    intervals =
      subtract_obstacle_interval(intervals, payload, edge, side, width_px, height_px, *obstacle);
    if intervals.is_empty() {
      break;
    }
  }
  intervals
}

fn subtract_obstacle_interval(
  intervals: Vec<(f32, f32)>,
  payload: &RectanglePayload,
  edge: RectangleLabelEdge,
  side: RectangleLabelSide,
  width_px: f32,
  height_px: f32,
  obstacle: Obstacle,
) -> Vec<(f32, f32)> {
  let body = rectangle_body(payload);
  let gap = payload.label.anchor_offset_px;
  let margin = obstacle.margin_px + COLLISION_EPSILON_PX;
  let mut result = Vec::new();
  match edge {
    RectangleLabelEdge::Top | RectangleLabelEdge::Bottom => {
      let y_px = horizontal_track_y(body, edge, side, gap, height_px);
      if !ranges_conflict(
        y_px,
        y_px + height_px,
        obstacle.bounds_px.min.y_px - margin,
        obstacle.bounds_px.max.y_px + margin,
      ) {
        return intervals;
      }
      let forbidden_lo =
        (obstacle.bounds_px.min.x_px - margin - width_px - body.min.x_px - gap) / body.width();
      let forbidden_hi =
        (obstacle.bounds_px.max.x_px + margin - body.min.x_px - gap) / body.width();
      let clearance = COLLISION_EPSILON_PX / body.width().max(1.0);
      for interval in intervals {
        push_interval_difference(&mut result, interval, (forbidden_lo, forbidden_hi), clearance);
      }
    }
    RectangleLabelEdge::Left | RectangleLabelEdge::Right => {
      let x_px = vertical_track_x(body, edge, side, gap, width_px);
      if !ranges_conflict(
        x_px,
        x_px + width_px,
        obstacle.bounds_px.min.x_px - margin,
        obstacle.bounds_px.max.x_px + margin,
      ) {
        return intervals;
      }
      let forbidden_lo =
        (obstacle.bounds_px.min.y_px - margin - height_px - body.min.y_px - gap) / body.height();
      let forbidden_hi =
        (obstacle.bounds_px.max.y_px + margin - body.min.y_px - gap) / body.height();
      let clearance = COLLISION_EPSILON_PX / body.height().max(1.0);
      for interval in intervals {
        push_interval_difference(&mut result, interval, (forbidden_lo, forbidden_hi), clearance);
      }
    }
  }
  result
}

fn push_interval_difference(
  result: &mut Vec<(f32, f32)>,
  interval: (f32, f32),
  cut: (f32, f32),
  clearance: f32,
) {
  let lo = interval.0;
  let hi = interval.1;
  let cut_lo = cut.0.max(lo);
  let cut_hi = cut.1.min(hi);
  if cut_hi < lo || cut_lo > hi {
    result.push(interval);
    return;
  }
  let position_epsilon = f32::EPSILON * 8.0;
  let left_hi = cut_lo - clearance;
  if lo <= left_hi + position_epsilon {
    result.push((lo, left_hi.clamp(lo, hi)));
  }
  let right_lo = cut_hi + clearance;
  if right_lo <= hi + position_epsilon {
    result.push((right_lo.clamp(lo, hi), hi));
  }
}

fn position_for_center(
  payload: &RectanglePayload,
  edge: RectangleLabelEdge,
  width_px: f32,
  height_px: f32,
  center: PointPx,
) -> f32 {
  let body = rectangle_body(payload);
  let gap = payload.label.anchor_offset_px;
  match edge {
    RectangleLabelEdge::Top | RectangleLabelEdge::Bottom => {
      ((center.x_px - width_px / 2.0 - body.min.x_px - gap) / body.width()).clamp(0.0, 1.0)
    }
    RectangleLabelEdge::Left | RectangleLabelEdge::Right => {
      ((center.y_px - height_px / 2.0 - body.min.y_px - gap) / body.height()).clamp(0.0, 1.0)
    }
  }
}

fn horizontal_track_y(
  body: RectPx,
  edge: RectangleLabelEdge,
  side: RectangleLabelSide,
  gap: f32,
  height_px: f32,
) -> f32 {
  match (edge, side) {
    (RectangleLabelEdge::Top, RectangleLabelSide::Outside) => body.min.y_px - gap - height_px,
    (RectangleLabelEdge::Top, RectangleLabelSide::Inside) => body.min.y_px + gap,
    (RectangleLabelEdge::Bottom, RectangleLabelSide::Inside) => body.max.y_px - gap - height_px,
    (RectangleLabelEdge::Bottom, RectangleLabelSide::Outside) => body.max.y_px + gap,
    _ => unreachable!(),
  }
}

fn vertical_track_x(
  body: RectPx,
  edge: RectangleLabelEdge,
  side: RectangleLabelSide,
  gap: f32,
  width_px: f32,
) -> f32 {
  match (edge, side) {
    (RectangleLabelEdge::Left, RectangleLabelSide::Outside) => body.min.x_px - gap - width_px,
    (RectangleLabelEdge::Left, RectangleLabelSide::Inside) => body.min.x_px + gap,
    (RectangleLabelEdge::Right, RectangleLabelSide::Inside) => body.max.x_px - gap - width_px,
    (RectangleLabelEdge::Right, RectangleLabelSide::Outside) => body.max.x_px + gap,
    _ => unreachable!(),
  }
}

fn actual_label_bounds(canvas_size_px: SizePx, payload: &RectanglePayload) -> Option<RectPx> {
  payload.label.visible_text()?;
  raw_rectangle_label_layout(payload, payload.label_anchor, canvas_size_px)
    .ok()
    .map(|layout| layout.bounds_px)
}

fn rectangle_body(payload: &RectanglePayload) -> RectPx {
  RectPx::from_points(payload.start_px, payload.end_px)
}

fn rects_conflict(left: RectPx, right: RectPx, margin_px: f32) -> bool {
  let margin = margin_px + COLLISION_EPSILON_PX;
  left.min.x_px <= right.max.x_px + margin
    && left.max.x_px >= right.min.x_px - margin
    && left.min.y_px <= right.max.y_px + margin
    && left.max.y_px >= right.min.y_px - margin
}

fn ranges_conflict(left_min: f32, left_max: f32, right_min: f32, right_max: f32) -> bool {
  left_min <= right_max + COLLISION_EPSILON_PX && left_max >= right_min - COLLISION_EPSILON_PX
}

fn rect_overlap_area(left: RectPx, right: RectPx, margin_px: f32) -> f32 {
  let margin = margin_px + COLLISION_EPSILON_PX;
  let expanded = right.expanded(margin);
  let width =
    (left.max.x_px.min(expanded.max.x_px) - left.min.x_px.max(expanded.min.x_px)).max(0.0);
  let height =
    (left.max.y_px.min(expanded.max.y_px) - left.min.y_px.max(expanded.min.y_px)).max(0.0);
  width * height
}

fn same_anchor(left: RectangleLabelAnchor, right: RectangleLabelAnchor) -> bool {
  left.edge == right.edge
    && left.side == right.side
    && (left.position - right.position).abs() < 0.001
}

fn compare_reflow_members(left: &LabelNode, right: &LabelNode) -> std::cmp::Ordering {
  left
    .distance
    .cmp(&right.distance)
    .then_with(|| right.item.z_index.cmp(&left.item.z_index))
    .then_with(|| {
      left.item.element_id.as_uuid().as_u128().cmp(&right.item.element_id.as_uuid().as_u128())
    })
}

fn compare_solution_order(
  left: &LabelNode,
  right: &LabelNode,
  primary_id: ElementId,
) -> std::cmp::Ordering {
  (right.item.element_id == primary_id)
    .cmp(&(left.item.element_id == primary_id))
    .then_with(|| right.item.z_index.cmp(&left.item.z_index))
    .then_with(|| {
      left.item.element_id.as_uuid().as_u128().cmp(&right.item.element_id.as_uuid().as_u128())
    })
}

fn compare_fixed_label_ids(
  scene: &RectangleLabelScene,
  left: ElementId,
  right: ElementId,
) -> std::cmp::Ordering {
  let left_item = scene.item(left);
  let right_item = scene.item(right);
  right_item
    .map(|item| item.z_index)
    .cmp(&left_item.map(|item| item.z_index))
    .then_with(|| left.as_uuid().as_u128().cmp(&right.as_uuid().as_u128()))
}

fn compare_candidates(left: &Candidate, right: &Candidate) -> std::cmp::Ordering {
  left
    .fallback_phase
    .cmp(&right.fallback_phase)
    .then_with(|| left.fallback_order.cmp(&right.fallback_order))
    .then_with(|| left.preferred_distance_px.total_cmp(&right.preferred_distance_px))
    .then_with(|| left.current_distance_px.total_cmp(&right.current_distance_px))
    .then_with(|| left.anchor.position.total_cmp(&right.anchor.position))
}

fn compare_states(left: &BeamState, right: &BeamState) -> std::cmp::Ordering {
  left
    .overlap_area_px
    .total_cmp(&right.overlap_area_px)
    .then_with(|| left.collision_count.cmp(&right.collision_count))
    .then_with(|| left.fallback_phase_cost.cmp(&right.fallback_phase_cost))
    .then_with(|| left.fallback_order_cost.cmp(&right.fallback_order_cost))
    .then_with(|| left.preferred_distance_px.total_cmp(&right.preferred_distance_px))
    .then_with(|| left.current_distance_px.total_cmp(&right.current_distance_px))
}

#[cfg(test)]
mod tests {
  use chrono::{TimeZone, Utc};
  use uuid::Uuid;

  use super::*;
  use crate::{
    document::{CapturedDisplay, DocumentId, GlobalBoundsPx},
    element::{ColorRgba, Element, ElementLabel, StrokeStyle, TextStyle},
  };

  fn document() -> BoardDocument {
    BoardDocument::new_capture(
      DocumentId::from_uuid(Uuid::nil()),
      SizePx::new(420, 260),
      CapturedDisplay {
        global_bounds_px: GlobalBoundsPx { x_px: 0, y_px: 0, width_px: 420, height_px: 260 },
        scale_factor: 1.0,
      },
      Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap(),
    )
    .unwrap()
  }

  fn rectangle(id: u128, z_index: i64, start: PointPx, end: PointPx) -> Element {
    let color = ColorRgba::YELLOW;
    let anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    Element::new(
      ElementId::from_uuid(Uuid::from_u128(id)),
      z_index,
      ElementPayload::Rectangle(RectanglePayload {
        start_px: start,
        end_px: end,
        stroke_style: StrokeStyle::mvp(color, 8.0).unwrap(),
        fill_rgba: None,
        label: ElementLabel {
          text: Some(format!("框{id}")),
          max_width_px: 180.0,
          padding_px: 8.0,
          anchor_offset_px: 8.0,
          text_style: TextStyle::mvp(color.contrasting_text(), 24.0).unwrap(),
        },
        preferred_label_anchor: anchor,
        label_anchor: anchor,
      }),
      SizePx::new(420, 260),
    )
    .unwrap()
  }

  fn candidate_tracks(candidates: &[Candidate]) -> Vec<(RectangleLabelEdge, RectangleLabelSide)> {
    let mut tracks = Vec::new();
    for candidate in candidates {
      let track = (candidate.anchor.edge, candidate.anchor.side);
      if tracks.last().copied() != Some(track) {
        tracks.push(track);
      }
    }
    tracks
  }

  fn outside_track_blocker(
    canvas_size_px: SizePx,
    payload: &RectanglePayload,
    edge: RectangleLabelEdge,
  ) -> Obstacle {
    let layout = raw_rectangle_label_layout(
      payload,
      RectangleLabelAnchor::new(edge, RectangleLabelSide::Outside, 0.0),
      canvas_size_px,
    )
    .unwrap();
    let canvas = canvas_size_px.bounds();
    let bounds_px = match edge {
      RectangleLabelEdge::Top | RectangleLabelEdge::Bottom => RectPx::from_min_max(
        PointPx::new(canvas.min.x_px, layout.bounds_px.min.y_px - 1.0),
        PointPx::new(canvas.max.x_px, layout.bounds_px.max.y_px + 1.0),
      ),
      RectangleLabelEdge::Left | RectangleLabelEdge::Right => RectPx::from_min_max(
        PointPx::new(layout.bounds_px.min.x_px - 1.0, canvas.min.y_px),
        PointPx::new(layout.bounds_px.max.x_px + 1.0, canvas.max.y_px),
      ),
    };
    Obstacle { bounds_px, margin_px: 0.0 }
  }

  #[test]
  fn adjacent_bodies_reflow_visible_labels_but_keep_preferences() {
    let mut before = document();
    before.elements.push(rectangle(1, 0, PointPx::new(60.0, 100.0), PointPx::new(170.0, 180.0)));
    before.elements.push(rectangle(2, 1, PointPx::new(220.0, 100.0), PointPx::new(330.0, 180.0)));
    let mut after = before.clone();
    let ElementPayload::Rectangle(payload) = &mut after.elements[0].payload else {
      unreachable!();
    };
    payload.start_px.x_px += 55.0;
    payload.end_px.x_px += 55.0;
    after.elements[0].refresh_bounds(after.canvas_size_px).unwrap();

    let solution = solve_rectangle_label_reflow(
      &RectangleLabelScene::from_document(&before),
      &RectangleLabelScene::from_document(&after),
      after.elements[0].element_id,
      &[],
    )
    .unwrap();
    assert!(solution.len() >= 2);
    assert!(solution.iter().all(|item| item.preferred_anchor.side == RectangleLabelSide::Outside));
    assert!(solution.iter().any(|item| item.actual_anchor != item.preferred_anchor));
  }

  #[test]
  fn candidates_keep_preferred_track_when_it_can_slide() {
    let canvas = SizePx::new(420, 260);
    let mut element = rectangle(1, 0, PointPx::new(80.0, 120.0), PointPx::new(230.0, 200.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    payload.label_anchor = payload.preferred_label_anchor;
    let preferred_layout =
      raw_rectangle_label_layout(payload, payload.preferred_label_anchor, canvas).unwrap();

    let candidates = candidates_for_label(
      canvas,
      payload,
      &[Obstacle { bounds_px: preferred_layout.bounds_px, margin_px: 0.0 }],
      true,
    )
    .unwrap();

    let first = candidates.first().unwrap();
    assert_eq!(
      (first.anchor.edge, first.anchor.side),
      (RectangleLabelEdge::Top, RectangleLabelSide::Outside)
    );
    assert!(first.anchor.position > 0.0);
  }

  #[test]
  fn candidates_use_nearest_legal_edge_after_horizontal_blocker() {
    let canvas = SizePx::new(420, 260);
    let mut element = rectangle(1, 0, PointPx::new(24.0, 150.0), PointPx::new(360.0, 240.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some("1".to_owned());
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    payload.label_anchor = payload.preferred_label_anchor;
    let upper_body = RectPx::from_min_max(PointPx::new(24.0, 24.0), PointPx::new(168.0, 140.0));

    let candidates = candidates_for_label(
      canvas,
      payload,
      &[Obstacle { bounds_px: upper_body, margin_px: payload.label.anchor_offset_px }],
      true,
    )
    .unwrap();

    let first = candidates.first().unwrap();
    let expected_min_x =
      upper_body.max.x_px + payload.label.anchor_offset_px + COLLISION_EPSILON_PX * 2.0;
    assert_eq!(
      (first.anchor.edge, first.anchor.side),
      (RectangleLabelEdge::Top, RectangleLabelSide::Outside)
    );
    assert!(first.bounds_px.min.x_px >= expected_min_x);
    assert!(first.bounds_px.min.x_px < expected_min_x + 0.1);
    assert!(first.bounds_px.max.x_px < 260.0);
  }

  #[test]
  fn right_outside_multiline_label_slides_down_when_blocked_from_above() {
    let canvas = SizePx::new(1004, 538);
    let lower_id = ElementId::from_uuid(Uuid::from_u128(1));
    let upper_id = ElementId::from_uuid(Uuid::from_u128(2));
    let ElementPayload::Rectangle(mut lower) =
      rectangle(1, 0, PointPx::new(20.0, 100.0), PointPx::new(180.0, 220.0)).payload
    else {
      unreachable!();
    };
    lower.start_px = PointPx::new(54.0, 268.0);
    lower.end_px = PointPx::new(492.0, 488.0);
    lower.label.text = Some(
      "123123123123123123\n123123123123123123\n123123123123123123\n132131231231231212\n312"
        .to_owned(),
    );
    lower.label.max_width_px = 330.0;
    lower.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Outside, 0.0);
    lower.label_anchor = lower.preferred_label_anchor;

    let ElementPayload::Rectangle(mut upper_before) =
      rectangle(2, 1, PointPx::new(200.0, 20.0), PointPx::new(390.0, 120.0)).payload
    else {
      unreachable!();
    };
    upper_before.start_px = PointPx::new(504.0, 41.0);
    upper_before.end_px = PointPx::new(943.0, 250.0);
    upper_before.label.text = None;
    let mut upper_after = upper_before.clone();
    upper_after.end_px.y_px = 275.0;

    let before = RectangleLabelScene::new(
      canvas,
      vec![
        RectangleLabelSceneItem { element_id: lower_id, z_index: 0, payload: lower.clone() },
        RectangleLabelSceneItem { element_id: upper_id, z_index: 1, payload: upper_before },
      ],
    )
    .unwrap();
    let after = RectangleLabelScene::new(
      canvas,
      vec![
        RectangleLabelSceneItem { element_id: lower_id, z_index: 0, payload: lower.clone() },
        RectangleLabelSceneItem { element_id: upper_id, z_index: 1, payload: upper_after.clone() },
      ],
    )
    .unwrap();

    let solution = solve_rectangle_label_reflow(&before, &after, upper_id, &[]).unwrap();
    let lower_solution = solution.iter().find(|item| item.element_id == lower_id).unwrap();
    let layout = raw_rectangle_label_layout(&lower, lower_solution.actual_anchor, canvas).unwrap();
    let expected_min_y =
      upper_after.end_px.y_px + lower.label.anchor_offset_px + COLLISION_EPSILON_PX * 2.0;

    assert_eq!(lower_solution.actual_anchor.edge, RectangleLabelEdge::Right);
    assert_eq!(lower_solution.actual_anchor.side, RectangleLabelSide::Outside);
    assert!(lower_solution.actual_anchor.position > 0.0);
    assert!(layout.bounds_px.min.y_px >= expected_min_y);
    assert!(layout.bounds_px.min.y_px < expected_min_y + 0.1);
  }

  #[test]
  fn right_outside_label_at_top_limit_keeps_single_legal_position_at_track_end() {
    let canvas = SizePx::new(420, 260);
    let mut element = rectangle(1, 0, PointPx::new(40.0, 80.0), PointPx::new(180.0, 180.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.label.text = Some("123123\n123123\n123123\n123123\n123123".to_owned());
    payload.label.max_width_px = 180.0;
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Outside, 0.0);
    payload.label_anchor = payload.preferred_label_anchor;
    let sample = raw_rectangle_label_layout(
      payload,
      RectangleLabelAnchor::new(RectangleLabelEdge::Right, RectangleLabelSide::Outside, 0.0),
      canvas,
    )
    .unwrap();
    let only_legal_y = canvas.height_px as f32 - sample.bounds_px.height();
    let blocker = Obstacle {
      bounds_px: RectPx::from_min_max(
        PointPx::new(190.0, 0.0),
        PointPx::new(
          400.0,
          only_legal_y - payload.label.anchor_offset_px - COLLISION_EPSILON_PX * 2.0,
        ),
      ),
      margin_px: payload.label.anchor_offset_px,
    };

    let candidates = candidates_for_label(canvas, payload, &[blocker], true).unwrap();
    let first = candidates.first().unwrap();

    assert_eq!(first.anchor.edge, RectangleLabelEdge::Right);
    assert_eq!(first.anchor.side, RectangleLabelSide::Outside);
    assert!((first.bounds_px.min.y_px - only_legal_y).abs() < 0.1);
  }

  #[test]
  fn screenshot_scene_normalizes_corner_anchor_and_slides_down_outside() {
    let canvas = SizePx::new(942, 332);
    let owner_id = ElementId::from_uuid(Uuid::from_u128(1));
    let intruder_id = ElementId::from_uuid(Uuid::from_u128(2));
    let ElementPayload::Rectangle(mut owner) =
      rectangle(1, 0, PointPx::new(40.0, 80.0), PointPx::new(180.0, 180.0)).payload
    else {
      unreachable!();
    };
    owner.start_px = PointPx::new(51.0, 132.0);
    owner.end_px = PointPx::new(347.0, 288.0);
    owner.label.text = Some(
      "abcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv\nabcdefghijklmnopqrstuv"
        .to_owned(),
    );
    owner.label.max_width_px = 340.0;
    owner.label.text_style.line_height_px = 25.75;
    owner.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Inside, 1.0);
    owner.label_anchor = owner.preferred_label_anchor;

    let ElementPayload::Rectangle(mut intruder_before) =
      rectangle(2, 1, PointPx::new(200.0, 20.0), PointPx::new(390.0, 120.0)).payload
    else {
      unreachable!();
    };
    intruder_before.start_px = PointPx::new(543.0, 0.0);
    intruder_before.end_px = PointPx::new(841.0, 125.0);
    intruder_before.label.text = None;
    let mut intruder_after = intruder_before.clone();
    intruder_after.start_px.y_px += 53.0;
    intruder_after.end_px.y_px += 53.0;

    let obstacle = Obstacle {
      bounds_px: rectangle_body(&intruder_after),
      margin_px: owner.label.anchor_offset_px,
    };
    let candidates = candidates_for_label(canvas, &owner, &[obstacle], true).unwrap();
    assert!(candidates.iter().any(|candidate| {
      candidate.anchor.edge == RectangleLabelEdge::Right
        && candidate.anchor.side == RectangleLabelSide::Outside
        && candidate.anchor.position > 0.0
    }));

    let before = RectangleLabelScene::new(
      canvas,
      vec![
        RectangleLabelSceneItem { element_id: owner_id, z_index: 0, payload: owner.clone() },
        RectangleLabelSceneItem { element_id: intruder_id, z_index: 1, payload: intruder_before },
      ],
    )
    .unwrap();
    let after = RectangleLabelScene::new(
      canvas,
      vec![
        RectangleLabelSceneItem { element_id: owner_id, z_index: 0, payload: owner.clone() },
        RectangleLabelSceneItem {
          element_id: intruder_id,
          z_index: 1,
          payload: intruder_after.clone(),
        },
      ],
    )
    .unwrap();
    let solution = solve_rectangle_label_reflow(&before, &after, intruder_id, &[]).unwrap();
    let owner_solution = solution.iter().find(|item| item.element_id == owner_id).unwrap();
    let layout = raw_rectangle_label_layout(&owner, owner_solution.actual_anchor, canvas).unwrap();
    let expected_min_y =
      intruder_after.end_px.y_px + owner.label.anchor_offset_px + COLLISION_EPSILON_PX * 2.0;

    assert_eq!(owner_solution.preferred_anchor.edge, RectangleLabelEdge::Right);
    assert_eq!(owner_solution.preferred_anchor.side, RectangleLabelSide::Outside);
    assert!(owner_solution.preferred_anchor.position.abs() < 0.001);
    assert_eq!(owner_solution.actual_anchor.edge, RectangleLabelEdge::Right);
    assert_eq!(owner_solution.actual_anchor.side, RectangleLabelSide::Outside);
    assert!(owner_solution.actual_anchor.position > 0.0);
    assert!((layout.bounds_px.min.y_px - expected_min_y).abs() < 0.1);
  }

  #[test]
  fn candidates_use_canvas_center_for_outer_fallback_order() {
    let canvas = SizePx::new(420, 260);
    let mut element = rectangle(1, 0, PointPx::new(240.0, 140.0), PointPx::new(340.0, 190.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Inside, 0.25);
    payload.label_anchor = payload.preferred_label_anchor;

    let candidates = candidates_for_label(canvas, payload, &[], false).unwrap();
    let tracks = candidate_tracks(&candidates);

    assert_eq!(
      &tracks[..5],
      &[
        (RectangleLabelEdge::Top, RectangleLabelSide::Inside),
        (RectangleLabelEdge::Bottom, RectangleLabelSide::Outside),
        (RectangleLabelEdge::Top, RectangleLabelSide::Outside),
        (RectangleLabelEdge::Right, RectangleLabelSide::Outside),
        (RectangleLabelEdge::Left, RectangleLabelSide::Outside),
      ]
    );
  }

  #[test]
  fn candidates_enter_inside_clockwise_after_outside_tracks_are_blocked() {
    let canvas = SizePx::new(420, 260);
    let mut element = rectangle(1, 0, PointPx::new(160.0, 90.0), PointPx::new(280.0, 170.0));
    let ElementPayload::Rectangle(payload) = &mut element.payload else {
      unreachable!();
    };
    payload.preferred_label_anchor =
      RectangleLabelAnchor::new(RectangleLabelEdge::Left, RectangleLabelSide::Outside, 0.5);
    payload.label_anchor = payload.preferred_label_anchor;
    let blockers = [
      outside_track_blocker(canvas, payload, RectangleLabelEdge::Left),
      outside_track_blocker(canvas, payload, RectangleLabelEdge::Top),
      outside_track_blocker(canvas, payload, RectangleLabelEdge::Bottom),
      outside_track_blocker(canvas, payload, RectangleLabelEdge::Right),
    ];

    let candidates = candidates_for_label(canvas, payload, &blockers, true).unwrap();
    let tracks = candidate_tracks(&candidates);

    assert_eq!(
      &tracks[..4],
      &[
        (RectangleLabelEdge::Left, RectangleLabelSide::Inside),
        (RectangleLabelEdge::Top, RectangleLabelSide::Inside),
        (RectangleLabelEdge::Right, RectangleLabelSide::Inside),
        (RectangleLabelEdge::Bottom, RectangleLabelSide::Inside),
      ]
    );
  }

  #[test]
  fn moved_obstacle_allows_displaced_label_to_return_to_preferred() {
    let preferred =
      RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0);
    let displaced =
      RectangleLabelAnchor::new(RectangleLabelEdge::Bottom, RectangleLabelSide::Outside, 0.0);
    let mut before = document();
    let mut first = rectangle(1, 0, PointPx::new(80.0, 120.0), PointPx::new(190.0, 200.0));
    let ElementPayload::Rectangle(payload) = &mut first.payload else {
      unreachable!();
    };
    payload.preferred_label_anchor = preferred;
    payload.label_anchor = displaced;
    first.refresh_bounds(before.canvas_size_px).unwrap();
    before.elements.push(first);
    before.elements.push(rectangle(2, 1, PointPx::new(70.0, 35.0), PointPx::new(210.0, 105.0)));
    let mut after = before.clone();
    let ElementPayload::Rectangle(blocker) = &mut after.elements[1].payload else {
      unreachable!();
    };
    blocker.start_px.x_px += 180.0;
    blocker.end_px.x_px += 180.0;
    after.elements[1].refresh_bounds(after.canvas_size_px).unwrap();

    let solution = solve_rectangle_label_reflow(
      &RectangleLabelScene::from_document(&before),
      &RectangleLabelScene::from_document(&after),
      after.elements[1].element_id,
      &[],
    )
    .unwrap();
    let first_solution =
      solution.iter().find(|item| item.element_id == after.elements[0].element_id).unwrap();
    assert_eq!(first_solution.actual_anchor, preferred);
  }

  #[test]
  fn collision_fallback_prefers_primary_top_inside_when_no_zero_solution_exists() {
    let mut before = document();
    let primary = rectangle(1, 0, PointPx::new(8.0, 8.0), PointPx::new(412.0, 252.0));
    let primary_id = primary.element_id;
    before.elements.push(primary);
    let mut blocker = rectangle(2, 1, PointPx::new(20.0, 20.0), PointPx::new(400.0, 240.0));
    let ElementPayload::Rectangle(payload) = &mut blocker.payload else {
      unreachable!();
    };
    payload.label.text = None;
    blocker.refresh_bounds(before.canvas_size_px).unwrap();
    before.elements.push(blocker);
    before.validate().unwrap();

    let solution = solve_rectangle_label_reflow(
      &RectangleLabelScene::from_document(&before),
      &RectangleLabelScene::from_document(&before),
      primary_id,
      &[],
    )
    .unwrap();

    let primary_solution = solution.iter().find(|item| item.element_id == primary_id).unwrap();
    assert_eq!(primary_solution.actual_anchor.edge, RectangleLabelEdge::Top);
    assert_eq!(primary_solution.actual_anchor.side, RectangleLabelSide::Inside);
  }

  #[test]
  fn fixed_label_blockers_are_expanded_once_in_stable_order() {
    let mut document = document();
    let primary = rectangle(1, 0, PointPx::new(80.0, 120.0), PointPx::new(190.0, 200.0));
    let primary_id = primary.element_id;
    document.elements.push(primary);
    let fixed = rectangle(2, 1, PointPx::new(75.0, 34.0), PointPx::new(205.0, 105.0));
    let fixed_id = fixed.element_id;
    document.elements.push(fixed);
    document.validate().unwrap();
    let scene = RectangleLabelScene::from_document(&document);
    let primary_item = scene.item(primary_id).unwrap().clone();
    let fixed_item = scene.item(fixed_id).unwrap();
    let fixed_bounds = actual_label_bounds(scene.canvas_size_px, &fixed_item.payload).unwrap();
    let movable = vec![LabelNode { item: primary_item, distance: 0 }];
    let fixed_label_ids = collect_fixed_label_ids(&scene, &movable);
    let placements = vec![Candidate {
      anchor: RectangleLabelAnchor::new(RectangleLabelEdge::Top, RectangleLabelSide::Outside, 0.0),
      bounds_px: fixed_bounds,
      fallback_phase: 0,
      fallback_order: 0,
      preferred_distance_px: 0.0,
      current_distance_px: 0.0,
    }];

    let blockers =
      fixed_label_blockers_for_placements(&scene, &movable, &placements, &fixed_label_ids);
    assert_eq!(blockers, vec![fixed_id]);
    let expanded = expanded_movable_with_blockers(&scene, &movable, &blockers, primary_id);
    assert_eq!(
      expanded.iter().map(|node| node.item.element_id).collect::<Vec<_>>(),
      vec![primary_id, fixed_id,]
    );
  }
}
