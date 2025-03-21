use std::hash::Hash;
use std::ops::Index;

use rustc_data_structures::fx::FxIndexMap;
use rustc_index::{IndexSlice, IndexVec};
use rustc_middle::ty::{self, Ty};
use rustc_span::Span;
use tracing::instrument;

/// Compactly stores a set of `R0 member of [R1...Rn]` constraints,
/// indexed by the region `R0`.
#[derive(Debug)]
pub(crate) struct MemberConstraintSet<'tcx, R>
where
    R: Copy + Eq,
{
    /// Stores the first "member" constraint for a given `R0`. This is an
    /// index into the `constraints` vector below.
    first_constraints: FxIndexMap<R, NllMemberConstraintIndex>,

    /// Stores the data about each `R0 member of [R1..Rn]` constraint.
    /// These are organized into a linked list, so each constraint
    /// contains the index of the next constraint with the same `R0`.
    constraints: IndexVec<NllMemberConstraintIndex, MemberConstraint<'tcx>>,

    /// Stores the `R1..Rn` regions for *all* sets. For any given
    /// constraint, we keep two indices so that we can pull out a
    /// slice.
    choice_regions: Vec<ty::RegionVid>,
}

/// Represents a `R0 member of [R1..Rn]` constraint
#[derive(Debug)]
pub(crate) struct MemberConstraint<'tcx> {
    next_constraint: Option<NllMemberConstraintIndex>,

    /// The span where the hidden type was instantiated.
    pub(crate) definition_span: Span,

    /// The hidden type in which `R0` appears. (Used in error reporting.)
    pub(crate) hidden_ty: Ty<'tcx>,

    pub(crate) key: ty::OpaqueTypeKey<'tcx>,

    /// The region `R0`.
    pub(crate) member_region_vid: ty::RegionVid,

    /// Index of `R1` in `choice_regions` vector from `MemberConstraintSet`.
    start_index: usize,

    /// Index of `Rn` in `choice_regions` vector from `MemberConstraintSet`.
    end_index: usize,
}

rustc_index::newtype_index! {
    #[debug_format = "MemberConstraintIndex({})"]
    pub(crate) struct NllMemberConstraintIndex {}
}

impl Default for MemberConstraintSet<'_, ty::RegionVid> {
    fn default() -> Self {
        Self {
            first_constraints: Default::default(),
            constraints: Default::default(),
            choice_regions: Default::default(),
        }
    }
}

impl<'tcx> MemberConstraintSet<'tcx, ty::RegionVid> {
    pub(crate) fn is_empty(&self) -> bool {
        self.constraints.is_empty()
    }

    /// Pushes a member constraint into the set.
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn add_member_constraint(
        &mut self,
        key: ty::OpaqueTypeKey<'tcx>,
        hidden_ty: Ty<'tcx>,
        definition_span: Span,
        member_region_vid: ty::RegionVid,
        choice_regions: &[ty::RegionVid],
    ) {
        let next_constraint = self.first_constraints.get(&member_region_vid).cloned();
        let start_index = self.choice_regions.len();
        self.choice_regions.extend(choice_regions);
        let end_index = self.choice_regions.len();
        let constraint_index = self.constraints.push(MemberConstraint {
            next_constraint,
            member_region_vid,
            definition_span,
            hidden_ty,
            key,
            start_index,
            end_index,
        });
        self.first_constraints.insert(member_region_vid, constraint_index);
    }
}

impl<'tcx, R1> MemberConstraintSet<'tcx, R1>
where
    R1: Copy + Hash + Eq,
{
    /// Remap the "member region" key using `map_fn`, producing a new
    /// member constraint set. This is used in the NLL code to map from
    /// the original `RegionVid` to an scc index. In some cases, we
    /// may have multiple `R1` values mapping to the same `R2` key -- that
    /// is ok, the two sets will be merged.
    pub(crate) fn into_mapped<R2>(
        self,
        mut map_fn: impl FnMut(R1) -> R2,
    ) -> MemberConstraintSet<'tcx, R2>
    where
        R2: Copy + Hash + Eq,
    {
        // We can re-use most of the original data, just tweaking the
        // linked list links a bit.
        //
        // For example if we had two keys `Ra` and `Rb` that both now
        // wind up mapped to the same key `S`, we would append the
        // linked list for `Ra` onto the end of the linked list for
        // `Rb` (or vice versa) -- this basically just requires
        // rewriting the final link from one list to point at the other
        // other (see `append_list`).

        let MemberConstraintSet { first_constraints, mut constraints, choice_regions } = self;

        let mut first_constraints2 = FxIndexMap::default();
        first_constraints2.reserve(first_constraints.len());

        for (r1, start1) in first_constraints {
            let r2 = map_fn(r1);
            if let Some(&start2) = first_constraints2.get(&r2) {
                append_list(&mut constraints, start1, start2);
            }
            first_constraints2.insert(r2, start1);
        }

        MemberConstraintSet { first_constraints: first_constraints2, constraints, choice_regions }
    }
}

impl<'tcx, R> MemberConstraintSet<'tcx, R>
where
    R: Copy + Hash + Eq,
{
    pub(crate) fn all_indices(&self) -> impl Iterator<Item = NllMemberConstraintIndex> {
        self.constraints.indices()
    }

    /// Iterate down the constraint indices associated with a given
    /// peek-region. You can then use `choice_regions` and other
    /// methods to access data.
    pub(crate) fn indices(
        &self,
        member_region_vid: R,
    ) -> impl Iterator<Item = NllMemberConstraintIndex> {
        let mut next = self.first_constraints.get(&member_region_vid).cloned();
        std::iter::from_fn(move || -> Option<NllMemberConstraintIndex> {
            if let Some(current) = next {
                next = self.constraints[current].next_constraint;
                Some(current)
            } else {
                None
            }
        })
    }

    /// Returns the "choice regions" for a given member
    /// constraint. This is the `R1..Rn` from a constraint like:
    ///
    /// ```text
    /// R0 member of [R1..Rn]
    /// ```
    pub(crate) fn choice_regions(&self, pci: NllMemberConstraintIndex) -> &[ty::RegionVid] {
        let MemberConstraint { start_index, end_index, .. } = &self.constraints[pci];
        &self.choice_regions[*start_index..*end_index]
    }
}

impl<'tcx, R> Index<NllMemberConstraintIndex> for MemberConstraintSet<'tcx, R>
where
    R: Copy + Eq,
{
    type Output = MemberConstraint<'tcx>;

    fn index(&self, i: NllMemberConstraintIndex) -> &MemberConstraint<'tcx> {
        &self.constraints[i]
    }
}

/// Given a linked list starting at `source_list` and another linked
/// list starting at `target_list`, modify `target_list` so that it is
/// followed by `source_list`.
///
/// Before:
///
/// ```text
/// target_list: A -> B -> C -> (None)
/// source_list: D -> E -> F -> (None)
/// ```
///
/// After:
///
/// ```text
/// target_list: A -> B -> C -> D -> E -> F -> (None)
/// ```
fn append_list(
    constraints: &mut IndexSlice<NllMemberConstraintIndex, MemberConstraint<'_>>,
    target_list: NllMemberConstraintIndex,
    source_list: NllMemberConstraintIndex,
) {
    let mut p = target_list;
    loop {
        let r = &mut constraints[p];
        match r.next_constraint {
            Some(q) => p = q,
            None => {
                r.next_constraint = Some(source_list);
                return;
            }
        }
    }
}
