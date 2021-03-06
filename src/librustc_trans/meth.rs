// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::rc::Rc;

use attributes;
use arena::TypedArena;
use back::symbol_names;
use llvm::{ValueRef, get_params};
use rustc::hir::def_id::DefId;
use rustc::ty::subst::{Subst, Substs};
use rustc::traits::{self, Reveal};
use abi::FnType;
use base::*;
use build::*;
use callee::{Callee, Virtual, trans_fn_pointer_shim};
use closure;
use common::*;
use consts;
use debuginfo::DebugLoc;
use declare;
use glue;
use machine;
use type_::Type;
use type_of::*;
use value::Value;
use rustc::ty::{self, Ty, TyCtxt, TypeFoldable};

use syntax::ast::Name;
use syntax_pos::DUMMY_SP;

// drop_glue pointer, size, align.
const VTABLE_OFFSET: usize = 3;

/// Extracts a method from a trait object's vtable, at the specified index.
pub fn get_virtual_method<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                      llvtable: ValueRef,
                                      vtable_index: usize)
                                      -> ValueRef {
    // Load the data pointer from the object.
    debug!("get_virtual_method(vtable_index={}, llvtable={:?})",
           vtable_index, Value(llvtable));

    Load(bcx, GEPi(bcx, llvtable, &[vtable_index + VTABLE_OFFSET]))
}

/// Generate a shim function that allows an object type like `SomeTrait` to
/// implement the type `SomeTrait`. Imagine a trait definition:
///
///    trait SomeTrait { fn get(&self) -> i32; ... }
///
/// And a generic bit of code:
///
///    fn foo<T:SomeTrait>(t: &T) {
///        let x = SomeTrait::get;
///        x(t)
///    }
///
/// What is the value of `x` when `foo` is invoked with `T=SomeTrait`?
/// The answer is that it is a shim function generated by this routine:
///
///    fn shim(t: &SomeTrait) -> i32 {
///        // ... call t.get() virtually ...
///    }
///
/// In fact, all virtual calls can be thought of as normal trait calls
/// that go through this shim function.
pub fn trans_object_shim<'a, 'tcx>(ccx: &'a CrateContext<'a, 'tcx>,
                                   method_ty: Ty<'tcx>,
                                   vtable_index: usize)
                                   -> ValueRef {
    let _icx = push_ctxt("trans_object_shim");
    let tcx = ccx.tcx();

    debug!("trans_object_shim(vtable_index={}, method_ty={:?})",
           vtable_index,
           method_ty);

    let sig = tcx.erase_late_bound_regions(&method_ty.fn_sig());
    let sig = tcx.normalize_associated_type(&sig);
    let fn_ty = FnType::new(ccx, method_ty.fn_abi(), &sig, &[]);

    let function_name =
        symbol_names::internal_name_from_type_and_suffix(ccx, method_ty, "object_shim");
    let llfn = declare::define_internal_fn(ccx, &function_name, method_ty);
    attributes::set_frame_pointer_elimination(ccx, llfn);

    let (block_arena, fcx): (TypedArena<_>, FunctionContext);
    block_arena = TypedArena::new();
    fcx = FunctionContext::new(ccx, llfn, fn_ty, None, &block_arena);
    let mut bcx = fcx.init(false);

    let dest = fcx.llretslotptr.get();

    debug!("trans_object_shim: method_offset_in_vtable={}",
           vtable_index);

    let llargs = get_params(fcx.llfn);

    let callee = Callee {
        data: Virtual(vtable_index),
        ty: method_ty
    };
    bcx = callee.call(bcx, DebugLoc::None,
                      &llargs[fcx.fn_ty.ret.is_indirect() as usize..], dest).bcx;

    fcx.finish(bcx, DebugLoc::None);

    llfn
}

/// Creates a returns a dynamic vtable for the given type and vtable origin.
/// This is used only for objects.
///
/// The `trait_ref` encodes the erased self type. Hence if we are
/// making an object `Foo<Trait>` from a value of type `Foo<T>`, then
/// `trait_ref` would map `T:Trait`.
pub fn get_vtable<'a, 'tcx>(ccx: &CrateContext<'a, 'tcx>,
                            trait_ref: ty::PolyTraitRef<'tcx>)
                            -> ValueRef
{
    let tcx = ccx.tcx();
    let _icx = push_ctxt("meth::get_vtable");

    debug!("get_vtable(trait_ref={:?})", trait_ref);

    // Check the cache.
    match ccx.vtables().borrow().get(&trait_ref) {
        Some(&val) => { return val }
        None => { }
    }

    // Not in the cache. Build it.
    let methods = traits::supertraits(tcx, trait_ref.clone()).flat_map(|trait_ref| {
        let vtable = fulfill_obligation(ccx.shared(), DUMMY_SP, trait_ref.clone());
        match vtable {
            // Should default trait error here?
            traits::VtableDefaultImpl(_) |
            traits::VtableBuiltin(_) => {
                Vec::new().into_iter()
            }
            traits::VtableImpl(
                traits::VtableImplData {
                    impl_def_id: id,
                    substs,
                    nested: _ }) => {
                let nullptr = C_null(Type::nil(ccx).ptr_to());
                get_vtable_methods(tcx, id, substs)
                    .into_iter()
                    .map(|opt_mth| opt_mth.map_or(nullptr, |mth| {
                        Callee::def(ccx, mth.method.def_id, &mth.substs).reify(ccx)
                    }))
                    .collect::<Vec<_>>()
                    .into_iter()
            }
            traits::VtableClosure(
                traits::VtableClosureData {
                    closure_def_id,
                    substs,
                    nested: _ }) => {
                let trait_closure_kind = tcx.lang_items.fn_trait_kind(trait_ref.def_id()).unwrap();
                let llfn = closure::trans_closure_method(ccx,
                                                         closure_def_id,
                                                         substs,
                                                         trait_closure_kind);
                vec![llfn].into_iter()
            }
            traits::VtableFnPointer(
                traits::VtableFnPointerData {
                    fn_ty: bare_fn_ty,
                    nested: _ }) => {
                let trait_closure_kind = tcx.lang_items.fn_trait_kind(trait_ref.def_id()).unwrap();
                vec![trans_fn_pointer_shim(ccx, trait_closure_kind, bare_fn_ty)].into_iter()
            }
            traits::VtableObject(ref data) => {
                // this would imply that the Self type being erased is
                // an object type; this cannot happen because we
                // cannot cast an unsized type into a trait object
                bug!("cannot get vtable for an object type: {:?}",
                     data);
            }
            traits::VtableParam(..) => {
                bug!("resolved vtable for {:?} to bad vtable {:?} in trans",
                     trait_ref,
                     vtable);
            }
        }
    });

    let size_ty = sizing_type_of(ccx, trait_ref.self_ty());
    let size = machine::llsize_of_alloc(ccx, size_ty);
    let align = align_of(ccx, trait_ref.self_ty());

    let components: Vec<_> = vec![
        // Generate a destructor for the vtable.
        glue::get_drop_glue(ccx, trait_ref.self_ty()),
        C_uint(ccx, size),
        C_uint(ccx, align)
    ].into_iter().chain(methods).collect();

    let vtable_const = C_struct(ccx, &components, false);
    let align = machine::llalign_of_pref(ccx, val_ty(vtable_const));
    let vtable = consts::addr_of(ccx, vtable_const, align, "vtable");

    ccx.vtables().borrow_mut().insert(trait_ref, vtable);
    vtable
}

pub fn get_vtable_methods<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                    impl_id: DefId,
                                    substs: &'tcx Substs<'tcx>)
                                    -> Vec<Option<ImplMethod<'tcx>>>
{
    debug!("get_vtable_methods(impl_id={:?}, substs={:?}", impl_id, substs);

    let trait_id = match tcx.impl_trait_ref(impl_id) {
        Some(t_id) => t_id.def_id,
        None       => bug!("make_impl_vtable: don't know how to \
                            make a vtable for a type impl!")
    };

    tcx.populate_implementations_for_trait_if_necessary(trait_id);

    let trait_item_def_ids = tcx.trait_item_def_ids(trait_id);
    trait_item_def_ids
        .iter()

        // Filter out non-method items.
        .filter_map(|item_def_id| {
            match *item_def_id {
                ty::MethodTraitItemId(def_id) => Some(def_id),
                _ => None,
            }
        })

        // Now produce pointers for each remaining method. If the
        // method could never be called from this object, just supply
        // null.
        .map(|trait_method_def_id| {
            debug!("get_vtable_methods: trait_method_def_id={:?}",
                   trait_method_def_id);

            let trait_method_type = match tcx.impl_or_trait_item(trait_method_def_id) {
                ty::MethodTraitItem(m) => m,
                _ => bug!("should be a method, not other assoc item"),
            };
            let name = trait_method_type.name;

            // Some methods cannot be called on an object; skip those.
            if !tcx.is_vtable_safe_method(trait_id, &trait_method_type) {
                debug!("get_vtable_methods: not vtable safe");
                return None;
            }

            debug!("get_vtable_methods: trait_method_type={:?}",
                   trait_method_type);

            // the method may have some early-bound lifetimes, add
            // regions for those
            let method_substs = Substs::for_item(tcx, trait_method_def_id,
                                                 |_, _| tcx.mk_region(ty::ReErased),
                                                 |_, _| tcx.types.err);

            // The substitutions we have are on the impl, so we grab
            // the method type from the impl to substitute into.
            let mth = get_impl_method(tcx, method_substs, impl_id, substs, name);

            debug!("get_vtable_methods: mth={:?}", mth);

            // If this is a default method, it's possible that it
            // relies on where clauses that do not hold for this
            // particular set of type parameters. Note that this
            // method could then never be called, so we do not want to
            // try and trans it, in that case. Issue #23435.
            if mth.is_provided {
                let predicates = mth.method.predicates.predicates.subst(tcx, &mth.substs);
                if !normalize_and_test_predicates(tcx, predicates) {
                    debug!("get_vtable_methods: predicates do not hold");
                    return None;
                }
            }

            Some(mth)
        })
        .collect()
}

#[derive(Debug)]
pub struct ImplMethod<'tcx> {
    pub method: Rc<ty::Method<'tcx>>,
    pub substs: &'tcx Substs<'tcx>,
    pub is_provided: bool
}

/// Locates the applicable definition of a method, given its name.
pub fn get_impl_method<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                 substs: &'tcx Substs<'tcx>,
                                 impl_def_id: DefId,
                                 impl_substs: &'tcx Substs<'tcx>,
                                 name: Name)
                                 -> ImplMethod<'tcx>
{
    assert!(!substs.needs_infer());

    let trait_def_id = tcx.trait_id_of_impl(impl_def_id).unwrap();
    let trait_def = tcx.lookup_trait_def(trait_def_id);

    match trait_def.ancestors(impl_def_id).fn_defs(tcx, name).next() {
        Some(node_item) => {
            let substs = tcx.infer_ctxt(None, None, Reveal::All).enter(|infcx| {
                let substs = substs.rebase_onto(tcx, trait_def_id, impl_substs);
                let substs = traits::translate_substs(&infcx, impl_def_id,
                                                      substs, node_item.node);
                tcx.lift(&substs).unwrap_or_else(|| {
                    bug!("trans::meth::get_impl_method: translate_substs \
                          returned {:?} which contains inference types/regions",
                         substs);
                })
            });
            ImplMethod {
                method: node_item.item,
                substs: substs,
                is_provided: node_item.node.is_from_trait(),
            }
        }
        None => {
            bug!("method {:?} not found in {:?}", name, impl_def_id)
        }
    }
}
