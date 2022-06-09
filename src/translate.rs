//! The translation machinery used to lift items into the context of the other crate for
//! comparison and inference.

use crate::mapping::{IdMapping, InherentEntry};
use log::{debug, info};
use rustc_hir::def_id::DefId;
use rustc_infer::infer::InferCtxt;
use rustc_middle::ty::{
    fold::{BottomUpFolder, TypeFoldable, TypeFolder},
    subst::{GenericArg, InternalSubsts, SubstsRef},
    GenericParamDefKind, ParamEnv, Predicate, Region, Term, TraitRef, Ty, TyCtxt,
    TypeSuperFoldable, Unevaluated,
};
use std::collections::HashMap;

/// The context in which `DefId` translation happens.
pub struct TranslationContext<'a, 'tcx> {
    /// The type context to use.
    tcx: TyCtxt<'tcx>,
    /// The id mapping to use.
    id_mapping: &'a IdMapping,
    /// Whether to translate type and region parameters.
    translate_params: bool,
    /// Elementary operation to decide whether to translate a `DefId`.
    needs_translation: fn(&IdMapping, DefId) -> bool,
    /// Elementary operation to translate a `DefId`.
    translate_orig: fn(&IdMapping, DefId) -> Option<DefId>,
}

impl<'a, 'tcx> TranslationContext<'a, 'tcx> {
    /// Construct a translation context translating to the new crate's `DefId`s.
    pub fn target_new(
        tcx: TyCtxt<'tcx>,
        id_mapping: &'a IdMapping,
        translate_params: bool,
    ) -> TranslationContext<'a, 'tcx> {
        TranslationContext {
            tcx,
            id_mapping,
            translate_params,
            needs_translation: IdMapping::in_old_crate,
            translate_orig: IdMapping::get_new_id,
        }
    }

    /// Construct a translation context translating to the old crate's `DefId`s.
    pub fn target_old(
        tcx: TyCtxt<'tcx>,
        id_mapping: &'a IdMapping,
        translate_params: bool,
    ) -> TranslationContext<'a, 'tcx> {
        TranslationContext {
            tcx,
            id_mapping,
            translate_params,
            needs_translation: IdMapping::in_new_crate,
            translate_orig: IdMapping::get_old_id,
        }
    }

    /// Construct a type parameter index map for translation.
    fn construct_index_map(&self, orig_def_id: DefId) -> HashMap<u32, DefId> {
        let mut index_map = HashMap::new();
        let orig_generics = self.tcx.generics_of(orig_def_id);

        for param in &orig_generics.params {
            if let GenericParamDefKind::Type { .. } = param.kind {
                index_map.insert(param.index, param.def_id);
            }
        }

        if let Some(did) = orig_generics.parent {
            let parent_generics = self.tcx.generics_of(did);

            for param in &parent_generics.params {
                if let GenericParamDefKind::Type { .. } = param.kind {
                    index_map.insert(param.index, param.def_id);
                }
            }
        }

        index_map
    }

    /// Check whether a `DefId` needs translation.
    fn needs_translation(&self, def_id: DefId) -> bool {
        (self.needs_translation)(self.id_mapping, def_id)
    }

    /// Translate a `DefId`.
    fn translate_orig(&self, def_id: DefId) -> DefId {
        (self.translate_orig)(self.id_mapping, def_id).unwrap_or_else(|| {
            info!("not mapped: {:?}", def_id);
            def_id
        })
    }

    /// Translate the `DefId` and substs of an item.
    fn translate_orig_substs(
        &self,
        index_map: &HashMap<u32, DefId>,
        orig_def_id: DefId,
        orig_substs: SubstsRef<'tcx>,
    ) -> Option<(DefId, SubstsRef<'tcx>)> {
        use rustc_middle::ty::subst::GenericArgKind;
        use rustc_middle::ty::ReEarlyBound;
        use std::cell::Cell;

        debug!(
            "translating w/ substs: did: {:?}, substs: {:?}",
            orig_def_id, orig_substs
        );

        let target_def_id = (self.translate_orig)(self.id_mapping, orig_def_id);

        if let Some(target_def_id) = target_def_id {
            let success = Cell::new(true);

            let target_substs =
                InternalSubsts::for_item(self.tcx, target_def_id, |def, _| match def.kind {
                    GenericParamDefKind::Lifetime => GenericArg::from(if !success.get() {
                        self.tcx
                            .mk_region(ReEarlyBound(def.to_early_bound_region_data()))
                    } else if let Some(GenericArgKind::Lifetime(region)) =
                        orig_substs.get(def.index as usize).map(|k| k.unpack())
                    {
                        self.translate_region(region)
                    } else {
                        success.set(false);
                        self.tcx
                            .mk_region(ReEarlyBound(def.to_early_bound_region_data()))
                    }),
                    GenericParamDefKind::Type { .. } => {
                        if !success.get() {
                            self.tcx.mk_param_from_def(def)
                        } else if let Some(GenericArgKind::Type(type_)) =
                            orig_substs.get(def.index as usize).map(|k| k.unpack())
                        {
                            self.translate(index_map, GenericArg::from(type_))
                        } else if self
                            .id_mapping
                            .is_non_mapped_defaulted_type_param(def.def_id)
                        {
                            GenericArg::from(self.tcx.type_of(def.def_id))
                        } else if self.tcx.generics_of(target_def_id).has_self && def.index == 0 {
                            self.tcx.mk_param_from_def(def)
                        } else {
                            success.set(false);
                            self.tcx.mk_param_from_def(def)
                        }
                    }
                    GenericParamDefKind::Const { .. } => unreachable!(),
                });

            if success.get() {
                return Some((target_def_id, target_substs));
            }
        }

        None
    }

    /// Fold a structure, translating all `DefId`s reachable by the folder.
    fn translate<T: TypeFoldable<'tcx>>(&self, index_map: &HashMap<u32, DefId>, orig: T) -> T {
        use rustc_middle::ty::ExistentialPredicate::*;
        use rustc_middle::ty::TyKind;
        use rustc_middle::ty::TypeAndMut;
        use rustc_middle::ty::{Binder, ExistentialProjection, ExistentialTraitRef};

        orig.fold_with(&mut BottomUpFolder {
            tcx: self.tcx,
            ty_op: |ty| {
                match *ty.kind() {
                    TyKind::Adt(adt_def, substs) if self.needs_translation(adt_def.did()) => {
                        // we fold bottom-up, so the code above is invalid, as it assumes the
                        // substs (that have been folded already) are yet untranslated
                        if let Some(target_def_id) =
                            (self.translate_orig)(self.id_mapping, adt_def.did())
                        {
                            let target_adt = self.tcx.adt_def(target_def_id);
                            self.tcx.mk_adt(target_adt, substs)
                        } else {
                            ty
                        }
                    }
                    TyKind::Ref(region, ty, mutbl) => {
                        let ty_and_mut = TypeAndMut { ty, mutbl };
                        self.tcx.mk_ref(self.translate_region(region), ty_and_mut)
                    }
                    TyKind::FnDef(did, substs) => {
                        // TODO: this might be buggy as *technically* the substs are
                        // already translated (see TyKind::Adt for a possible fix)
                        if let Some((target_def_id, target_substs)) =
                            self.translate_orig_substs(index_map, did, substs)
                        {
                            self.tcx.mk_fn_def(target_def_id, target_substs)
                        } else {
                            ty
                        }
                    }
                    TyKind::Dynamic(preds, region) => {
                        // hacky error catching mechanism
                        use rustc_hir::def_id::CRATE_DEF_INDEX;
                        use std::cell::Cell;

                        let success = Cell::new(true);
                        let err_pred = AutoTrait(DefId::local(CRATE_DEF_INDEX));
                        // A `Self` within original bounds are to be substituted
                        // with a `trait_object_dummy_self`.
                        let dummy_self = self.tcx.types.trait_object_dummy_self;

                        let res: Vec<_> = preds
                            .iter()
                            .map(|p| {
                                p.map_bound(|p| {
                                    match p {
                                        Trait(existential_trait_ref) => {
                                            let trait_ref = Binder::dummy(existential_trait_ref)
                                                .with_self_ty(self.tcx, dummy_self);
                                            let did = trait_ref.skip_binder().def_id;
                                            let substs = trait_ref.skip_binder().substs;

                                            // TODO: here, the substs could also be already translated
                                            if let Some((target_def_id, target_substs)) =
                                                self.translate_orig_substs(index_map, did, substs)
                                            {
                                                let target_trait_ref = TraitRef {
                                                    def_id: target_def_id,
                                                    substs: target_substs,
                                                };
                                                Trait(ExistentialTraitRef::erase_self_ty(
                                                    self.tcx,
                                                    target_trait_ref,
                                                ))
                                            } else {
                                                success.set(false);
                                                err_pred
                                            }
                                        }
                                        Projection(existential_projection) => {
                                            let projection_pred =
                                                Binder::dummy(existential_projection)
                                                    .with_self_ty(self.tcx, dummy_self);
                                            let item_def_id = projection_pred
                                                .skip_binder()
                                                .projection_ty
                                                .item_def_id;
                                            let substs =
                                                projection_pred.skip_binder().projection_ty.substs;

                                            // TODO: here, the substs could also be already translated
                                            if let Some((target_def_id, target_substs)) = self
                                                .translate_orig_substs(
                                                    index_map,
                                                    item_def_id,
                                                    substs,
                                                )
                                            {
                                                Projection(ExistentialProjection {
                                                    item_def_id: target_def_id,
                                                    // TODO: should be it's own method in rustc
                                                    substs: self
                                                        .tcx
                                                        .intern_substs(&target_substs[1..]),
                                                    term: Term::Ty(ty),
                                                })
                                            } else {
                                                success.set(false);
                                                err_pred
                                            }
                                        }
                                        AutoTrait(did) => AutoTrait(self.translate_orig(did)),
                                    }
                                })
                            })
                            .collect();

                        if success.get() {
                            let target_preds = self.tcx.mk_poly_existential_predicates(res.iter());
                            self.tcx.mk_dynamic(target_preds, region)
                        } else {
                            ty
                        }
                    }
                    TyKind::Projection(proj) => {
                        if let Some((target_def_id, target_substs)) =
                            self.translate_orig_substs(index_map, proj.item_def_id, proj.substs)
                        {
                            self.tcx.mk_projection(target_def_id, target_substs)
                        } else {
                            ty
                        }
                    }
                    TyKind::Opaque(did, substs) => {
                        if let Some((target_def_id, target_substs)) =
                            self.translate_orig_substs(index_map, did, substs)
                        {
                            self.tcx.mk_opaque(target_def_id, target_substs)
                        } else {
                            ty
                        }
                    }
                    TyKind::Param(param) => {
                        // FIXME: we should check `has_self` if this gets used again!
                        if param.index != 0 && self.translate_params {
                            // `Self` is special
                            let orig_def_id = index_map[&param.index];
                            if self.needs_translation(orig_def_id) {
                                use rustc_middle::ty::subst::GenericArgKind;

                                let target_def_id = self.translate_orig(orig_def_id);
                                debug!("translating type param: {:?}", param);
                                let type_param = self.id_mapping.get_type_param(&target_def_id);
                                debug!("translated type param: {:?}", type_param);
                                match self.tcx.mk_param_from_def(type_param).unpack() {
                                    GenericArgKind::Type(param_t) => param_t,
                                    _ => unreachable!(),
                                }
                            } else {
                                ty
                            }
                        } else {
                            ty
                        }
                    }
                    _ => ty,
                }
            },
            lt_op: |region| self.translate_region(region),
            ct_op: |konst| konst, // TODO: translate consts
        })
    }

    /// Translate a region.
    fn translate_region(&self, region: Region<'tcx>) -> Region<'tcx> {
        use rustc_middle::ty::BoundRegionKind::*;
        use rustc_middle::ty::RegionKind::*;
        use rustc_middle::ty::{EarlyBoundRegion, FreeRegion};

        if !self.translate_params {
            return region;
        }

        self.tcx.mk_region(match *region {
            ReEarlyBound(early) => {
                let target_early = EarlyBoundRegion {
                    def_id: self.translate_orig(early.def_id),
                    index: early.index,
                    name: early.name,
                };

                ReEarlyBound(target_early)
            }
            ReFree(FreeRegion {
                scope,
                bound_region,
            }) => ReFree(FreeRegion {
                scope: self.translate_orig(scope),
                bound_region: match bound_region {
                    BrNamed(def_id, name) => BrNamed(self.translate_orig(def_id), name),
                    reg => reg,
                },
            }),
            reg => reg,
        })
    }

    /// Translate an item's type.
    pub fn translate_item_type(&self, orig_def_id: DefId, orig: Ty<'tcx>) -> Ty<'tcx> {
        self.translate(&self.construct_index_map(orig_def_id), orig)
    }

    /// Translate a predicate using a type parameter index map.
    fn translate_predicate(
        &self,
        index_map: &HashMap<u32, DefId>,
        predicate: Predicate<'tcx>,
    ) -> Option<Predicate<'tcx>> {
        use rustc_middle::ty::{
            self, CoercePredicate, OutlivesPredicate, PredicateKind, ProjectionPredicate,
            ProjectionTy, SubtypePredicate, ToPredicate, TraitPredicate, WithOptConstParam,
        };

        let pred = match predicate.kind().skip_binder() {
            PredicateKind::Trait(pred) => PredicateKind::Trait(
                if let Some((target_def_id, target_substs)) = self.translate_orig_substs(
                    index_map,
                    pred.trait_ref.def_id,
                    pred.trait_ref.substs,
                ) {
                    TraitPredicate {
                        trait_ref: TraitRef {
                            def_id: target_def_id,
                            substs: target_substs,
                        },
                        constness: pred.constness,
                        polarity: pred.polarity,
                    }
                } else {
                    return None;
                },
            ),
            PredicateKind::RegionOutlives(pred) => PredicateKind::RegionOutlives({
                let l = self.translate_region(pred.0);
                let r = self.translate_region(pred.1);
                OutlivesPredicate(l, r)
            }),
            PredicateKind::TypeOutlives(pred) => PredicateKind::TypeOutlives({
                let l = self.translate(index_map, pred.0);
                let r = self.translate_region(pred.1);
                OutlivesPredicate(l, r)
            }),
            PredicateKind::Projection(pred) => PredicateKind::Projection(
                if let Some((target_def_id, target_substs)) = self.translate_orig_substs(
                    index_map,
                    pred.projection_ty.item_def_id,
                    pred.projection_ty.substs,
                ) {
                    ProjectionPredicate {
                        projection_ty: ProjectionTy {
                            substs: target_substs,
                            item_def_id: target_def_id,
                        },
                        term: match pred.term {
                            Term::Ty(ty) => Term::Ty(self.translate(index_map, ty)),
                            Term::Const(_) => pred.term,
                        },
                    }
                } else {
                    return None;
                },
            ),
            PredicateKind::WellFormed(ty) => {
                PredicateKind::WellFormed(self.translate(index_map, ty))
            }
            PredicateKind::ObjectSafe(did) => PredicateKind::ObjectSafe(self.translate_orig(did)),
            PredicateKind::ClosureKind(did, substs, kind) => PredicateKind::ClosureKind(
                self.translate_orig(did),
                self.translate(index_map, substs),
                kind,
            ),
            PredicateKind::Subtype(pred) => PredicateKind::Subtype({
                let l = self.translate(index_map, pred.a);
                let r = self.translate(index_map, pred.b);
                SubtypePredicate {
                    a_is_expected: pred.a_is_expected,
                    a: l,
                    b: r,
                }
            }),
            PredicateKind::Coerce(pred) => PredicateKind::Coerce({
                let a = self.translate(index_map, pred.a);
                let b = self.translate(index_map, pred.b);
                CoercePredicate { a, b }
            }),
            PredicateKind::ConstEvaluatable(uv) => {
                if let Some((target_def_id, target_substs)) =
                    self.translate_orig_substs(index_map, uv.def.did, uv.substs)
                {
                    // TODO: We could probably use translated version for
                    // `WithOptConstParam::const_param_did`
                    let const_param = WithOptConstParam::unknown(target_def_id);
                    PredicateKind::ConstEvaluatable(Unevaluated::new(const_param, target_substs))
                } else {
                    return None;
                }
            }
            PredicateKind::ConstEquate(c1, c2) => PredicateKind::ConstEquate(
                self.translate(index_map, c1),
                self.translate(index_map, c2),
            ),
            // NOTE: Only used for Chalk trait solver
            PredicateKind::TypeWellFormedFromEnv(_) => unimplemented!(),
        };

        Some(ty::Binder::dummy(pred).to_predicate(self.tcx))
    }

    /// Translate a slice of predicates in the context of an item.
    fn translate_predicates(
        &self,
        orig_def_id: DefId,
        orig_preds: &[Predicate<'tcx>],
    ) -> Option<Vec<Predicate<'tcx>>> {
        let index_map = self.construct_index_map(orig_def_id);
        let mut target_preds = Vec::with_capacity(orig_preds.len());

        for orig_pred in orig_preds {
            if let Some(target_pred) = self.translate_predicate(&index_map, *orig_pred) {
                target_preds.push(target_pred);
            } else {
                return None;
            }
        }

        Some(target_preds)
    }

    /// Translate a `ParamEnv` in the context of an item.
    pub fn translate_param_env(
        &self,
        orig_def_id: DefId,
        param_env: ParamEnv<'tcx>,
    ) -> Option<ParamEnv<'tcx>> {
        self.translate_predicates(orig_def_id, param_env.caller_bounds())
            .map(|target_preds| {
                ParamEnv::new(
                    self.tcx.intern_predicates(&target_preds),
                    param_env.reveal(),
                    param_env.constness(),
                )
            })
    }

    /// Translate a `TraitRef` in the context of an item.
    pub fn translate_trait_ref(
        &self,
        orig_def_id: DefId,
        orig_trait_ref: &TraitRef<'tcx>,
    ) -> TraitRef<'tcx> {
        let index_map = self.construct_index_map(orig_def_id);
        TraitRef {
            def_id: self.translate_orig(orig_trait_ref.def_id),
            substs: self.translate(&index_map, orig_trait_ref.substs),
        }
    }

    /// Translate an `InherentEntry`.
    pub fn translate_inherent_entry(&self, orig_entry: &InherentEntry) -> Option<InherentEntry> {
        (self.translate_orig)(self.id_mapping, orig_entry.parent_def_id).map(|parent_def_id| {
            InherentEntry {
                parent_def_id,
                kind: orig_entry.kind,
                name: orig_entry.name,
            }
        })
    }

    /// Check whether a given `DefId` can be translated.
    pub fn can_translate(&self, def_id: DefId) -> bool {
        (self.translate_orig)(self.id_mapping, def_id).is_some()
    }
}

/// A type folder that removes inference artifacts.
///
/// Used to lift type errors and predicates to wrap them in an error type.
#[derive(Clone)]
pub struct InferenceCleanupFolder<'a, 'tcx: 'a> {
    /// The inference context used.
    infcx: &'a InferCtxt<'a, 'tcx>,
}

impl<'a, 'tcx> InferenceCleanupFolder<'a, 'tcx> {
    /// Construct a new folder.
    pub fn new(infcx: &'a InferCtxt<'a, 'tcx>) -> Self {
        InferenceCleanupFolder { infcx }
    }
}

impl<'a, 'tcx> TypeFolder<'tcx> for InferenceCleanupFolder<'a, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
        use rustc_middle::ty::TyKind;
        use rustc_middle::ty::TypeAndMut;

        let t1 = ty.super_fold_with(self);
        match *t1.kind() {
            TyKind::Ref(region, ty, mutbl) if region.needs_infer() => {
                let ty_and_mut = TypeAndMut { ty, mutbl };
                self.infcx
                    .tcx
                    .mk_ref(self.infcx.tcx.lifetimes.re_erased, ty_and_mut)
            }
            TyKind::Infer(_) => self.infcx.tcx.ty_error(),
            _ => t1,
        }
    }

    fn fold_region(&mut self, r: Region<'tcx>) -> Region<'tcx> {
        let r1 = r.super_fold_with(self);
        if r1.needs_infer() {
            self.infcx.tcx.lifetimes.re_erased
        } else {
            r1
        }
    }
}
