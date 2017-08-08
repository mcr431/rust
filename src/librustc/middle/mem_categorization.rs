// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! # Categorization
//!
//! The job of the categorization module is to analyze an expression to
//! determine what kind of memory is used in evaluating it (for example,
//! where dereferences occur and what kind of pointer is dereferenced;
//! whether the memory is mutable; etc)
//!
//! Categorization effectively transforms all of our expressions into
//! expressions of the following forms (the actual enum has many more
//! possibilities, naturally, but they are all variants of these base
//! forms):
//!
//!     E = rvalue    // some computed rvalue
//!       | x         // address of a local variable or argument
//!       | *E        // deref of a ptr
//!       | E.comp    // access to an interior component
//!
//! Imagine a routine ToAddr(Expr) that evaluates an expression and returns an
//! address where the result is to be found.  If Expr is an lvalue, then this
//! is the address of the lvalue.  If Expr is an rvalue, this is the address of
//! some temporary spot in memory where the result is stored.
//!
//! Now, cat_expr() classifies the expression Expr and the address A=ToAddr(Expr)
//! as follows:
//!
//! - cat: what kind of expression was this?  This is a subset of the
//!   full expression forms which only includes those that we care about
//!   for the purpose of the analysis.
//! - mutbl: mutability of the address A
//! - ty: the type of data found at the address A
//!
//! The resulting categorization tree differs somewhat from the expressions
//! themselves.  For example, auto-derefs are explicit.  Also, an index a[b] is
//! decomposed into two operations: a dereference to reach the array data and
//! then an index to jump forward to the relevant item.
//!
//! ## By-reference upvars
//!
//! One part of the translation which may be non-obvious is that we translate
//! closure upvars into the dereference of a borrowed pointer; this more closely
//! resembles the runtime translation. So, for example, if we had:
//!
//!     let mut x = 3;
//!     let y = 5;
//!     let inc = || x += y;
//!
//! Then when we categorize `x` (*within* the closure) we would yield a
//! result of `*x'`, effectively, where `x'` is a `Categorization::Upvar` reference
//! tied to `x`. The type of `x'` will be a borrowed pointer.

#![allow(non_camel_case_types)]

pub use self::PointerKind::*;
pub use self::InteriorKind::*;
pub use self::FieldName::*;
pub use self::MutabilityCategory::*;
pub use self::AliasableReason::*;
pub use self::Note::*;

use self::Aliasability::*;

use middle::region::RegionMaps;
use hir::def_id::DefId;
use hir::map as hir_map;
use infer::InferCtxt;
use hir::def::{Def, CtorKind};
use ty::adjustment;
use ty::{self, Ty, TyCtxt};
use ty::fold::TypeFoldable;

use hir::{MutImmutable, MutMutable, PatKind};
use hir::pat_util::EnumerateAndAdjustIterator;
use hir;
use syntax::ast;
use syntax_pos::Span;

use std::fmt;
use std::rc::Rc;

#[derive(Clone, PartialEq)]
pub enum Categorization<'tcx> {
    Rvalue(ty::Region<'tcx>),              // temporary val, argument is its scope
    StaticItem,
    Upvar(Upvar),                          // upvar referenced by closure env
    Local(ast::NodeId),                    // local variable
    Deref(cmt<'tcx>, PointerKind<'tcx>),   // deref of a ptr
    Interior(cmt<'tcx>, InteriorKind),     // something interior: field, tuple, etc
    Downcast(cmt<'tcx>, DefId),            // selects a particular enum variant (*1)

    // (*1) downcast is only required if the enum has more than one variant
}

// Represents any kind of upvar
#[derive(Clone, Copy, PartialEq)]
pub struct Upvar {
    pub id: ty::UpvarId,
    pub kind: ty::ClosureKind
}

// different kinds of pointers:
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerKind<'tcx> {
    /// `Box<T>`
    Unique,

    /// `&T`
    BorrowedPtr(ty::BorrowKind, ty::Region<'tcx>),

    /// `*T`
    UnsafePtr(hir::Mutability),

    /// Implicit deref of the `&T` that results from an overloaded index `[]`.
    Implicit(ty::BorrowKind, ty::Region<'tcx>),
}

// We use the term "interior" to mean "something reachable from the
// base without a pointer dereference", e.g. a field
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum InteriorKind {
    InteriorField(FieldName),
    InteriorElement(InteriorOffsetKind),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FieldName {
    NamedField(ast::Name),
    PositionalField(usize)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InteriorOffsetKind {
    Index,            // e.g. `array_expr[index_expr]`
    Pattern,          // e.g. `fn foo([_, a, _, _]: [A; 4]) { ... }`
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MutabilityCategory {
    McImmutable, // Immutable.
    McDeclared,  // Directly declared as mutable.
    McInherited, // Inherited from the fact that owner is mutable.
}

// A note about the provenance of a `cmt`.  This is used for
// special-case handling of upvars such as mutability inference.
// Upvar categorization can generate a variable number of nested
// derefs.  The note allows detecting them without deep pattern
// matching on the categorization.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Note {
    NoteClosureEnv(ty::UpvarId), // Deref through closure env
    NoteUpvarRef(ty::UpvarId),   // Deref through by-ref upvar
    NoteNone                     // Nothing special
}

// `cmt`: "Category, Mutability, and Type".
//
// a complete categorization of a value indicating where it originated
// and how it is located, as well as the mutability of the memory in
// which the value is stored.
//
// *WARNING* The field `cmt.type` is NOT necessarily the same as the
// result of `node_id_to_type(cmt.id)`. This is because the `id` is
// always the `id` of the node producing the type; in an expression
// like `*x`, the type of this deref node is the deref'd type (`T`),
// but in a pattern like `@x`, the `@x` pattern is again a
// dereference, but its type is the type *before* the dereference
// (`@T`). So use `cmt.ty` to find the type of the value in a consistent
// fashion. For more details, see the method `cat_pattern`
#[derive(Clone, PartialEq)]
pub struct cmt_<'tcx> {
    pub id: ast::NodeId,           // id of expr/pat producing this value
    pub span: Span,                // span of same expr/pat
    pub cat: Categorization<'tcx>, // categorization of expr
    pub mutbl: MutabilityCategory, // mutability of expr as lvalue
    pub ty: Ty<'tcx>,              // type of the expr (*see WARNING above*)
    pub note: Note,                // Note about the provenance of this cmt
}

pub type cmt<'tcx> = Rc<cmt_<'tcx>>;

pub enum ImmutabilityBlame<'tcx> {
    ImmLocal(ast::NodeId),
    ClosureEnv(ast::NodeId),
    LocalDeref(ast::NodeId),
    AdtFieldDeref(&'tcx ty::AdtDef, &'tcx ty::FieldDef)
}

impl<'tcx> cmt_<'tcx> {
    fn resolve_field(&self, field_name: FieldName) -> Option<(&'tcx ty::AdtDef, &'tcx ty::FieldDef)>
    {
        let adt_def = match self.ty.sty {
            ty::TyAdt(def, _) => def,
            ty::TyTuple(..) => return None,
            // closures get `Categorization::Upvar` rather than `Categorization::Interior`
            _ =>  bug!("interior cmt {:?} is not an ADT", self)
        };
        let variant_def = match self.cat {
            Categorization::Downcast(_, variant_did) => {
                adt_def.variant_with_id(variant_did)
            }
            _ => {
                assert!(adt_def.is_univariant());
                &adt_def.variants[0]
            }
        };
        let field_def = match field_name {
            NamedField(name) => variant_def.field_named(name),
            PositionalField(idx) => &variant_def.fields[idx]
        };
        Some((adt_def, field_def))
    }

    pub fn immutability_blame(&self) -> Option<ImmutabilityBlame<'tcx>> {
        match self.cat {
            Categorization::Deref(ref base_cmt, BorrowedPtr(ty::ImmBorrow, _)) |
            Categorization::Deref(ref base_cmt, Implicit(ty::ImmBorrow, _)) => {
                // try to figure out where the immutable reference came from
                match base_cmt.cat {
                    Categorization::Local(node_id) =>
                        Some(ImmutabilityBlame::LocalDeref(node_id)),
                    Categorization::Interior(ref base_cmt, InteriorField(field_name)) => {
                        base_cmt.resolve_field(field_name).map(|(adt_def, field_def)| {
                            ImmutabilityBlame::AdtFieldDeref(adt_def, field_def)
                        })
                    }
                    Categorization::Upvar(Upvar { id, .. }) => {
                        if let NoteClosureEnv(..) = self.note {
                            Some(ImmutabilityBlame::ClosureEnv(id.closure_expr_id))
                        } else {
                            None
                        }
                    }
                    _ => None
                }
            }
            Categorization::Local(node_id) => {
                Some(ImmutabilityBlame::ImmLocal(node_id))
            }
            Categorization::Rvalue(..) |
            Categorization::Upvar(..) |
            Categorization::Deref(_, UnsafePtr(..)) => {
                // This should not be reachable up to inference limitations.
                None
            }
            Categorization::Interior(ref base_cmt, _) |
            Categorization::Downcast(ref base_cmt, _) |
            Categorization::Deref(ref base_cmt, _) => {
                base_cmt.immutability_blame()
            }
            Categorization::StaticItem => {
                // Do we want to do something here?
                None
            }
        }
    }
}

pub trait ast_node {
    fn id(&self) -> ast::NodeId;
    fn span(&self) -> Span;
}

impl ast_node for hir::Expr {
    fn id(&self) -> ast::NodeId { self.id }
    fn span(&self) -> Span { self.span }
}

impl ast_node for hir::Pat {
    fn id(&self) -> ast::NodeId { self.id }
    fn span(&self) -> Span { self.span }
}

#[derive(Clone)]
pub struct MemCategorizationContext<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    pub tcx: TyCtxt<'a, 'gcx, 'tcx>,
    pub region_maps: &'a RegionMaps,
    pub tables: &'a ty::TypeckTables<'tcx>,
    infcx: Option<&'a InferCtxt<'a, 'gcx, 'tcx>>,
}

pub type McResult<T> = Result<T, ()>;

impl MutabilityCategory {
    pub fn from_mutbl(m: hir::Mutability) -> MutabilityCategory {
        let ret = match m {
            MutImmutable => McImmutable,
            MutMutable => McDeclared
        };
        debug!("MutabilityCategory::{}({:?}) => {:?}",
               "from_mutbl", m, ret);
        ret
    }

    pub fn from_borrow_kind(borrow_kind: ty::BorrowKind) -> MutabilityCategory {
        let ret = match borrow_kind {
            ty::ImmBorrow => McImmutable,
            ty::UniqueImmBorrow => McImmutable,
            ty::MutBorrow => McDeclared,
        };
        debug!("MutabilityCategory::{}({:?}) => {:?}",
               "from_borrow_kind", borrow_kind, ret);
        ret
    }

    fn from_pointer_kind(base_mutbl: MutabilityCategory,
                         ptr: PointerKind) -> MutabilityCategory {
        let ret = match ptr {
            Unique => {
                base_mutbl.inherit()
            }
            BorrowedPtr(borrow_kind, _) | Implicit(borrow_kind, _) => {
                MutabilityCategory::from_borrow_kind(borrow_kind)
            }
            UnsafePtr(m) => {
                MutabilityCategory::from_mutbl(m)
            }
        };
        debug!("MutabilityCategory::{}({:?}, {:?}) => {:?}",
               "from_pointer_kind", base_mutbl, ptr, ret);
        ret
    }

    fn from_local(tcx: TyCtxt, tables: &ty::TypeckTables, id: ast::NodeId) -> MutabilityCategory {
        let ret = match tcx.hir.get(id) {
            hir_map::NodeLocal(p) => match p.node {
                PatKind::Binding(..) => {
                    let bm = *tables.pat_binding_modes.get(&p.id).expect("missing binding mode");
                    if bm == ty::BindByValue(hir::MutMutable) {
                        McDeclared
                    } else {
                        McImmutable
                    }
                }
                _ => span_bug!(p.span, "expected identifier pattern")
            },
            _ => span_bug!(tcx.hir.span(id), "expected identifier pattern")
        };
        debug!("MutabilityCategory::{}(tcx, id={:?}) => {:?}",
               "from_local", id, ret);
        ret
    }

    pub fn inherit(&self) -> MutabilityCategory {
        let ret = match *self {
            McImmutable => McImmutable,
            McDeclared => McInherited,
            McInherited => McInherited,
        };
        debug!("{:?}.inherit() => {:?}", self, ret);
        ret
    }

    pub fn is_mutable(&self) -> bool {
        let ret = match *self {
            McImmutable => false,
            McInherited => true,
            McDeclared => true,
        };
        debug!("{:?}.is_mutable() => {:?}", self, ret);
        ret
    }

    pub fn is_immutable(&self) -> bool {
        let ret = match *self {
            McImmutable => true,
            McDeclared | McInherited => false
        };
        debug!("{:?}.is_immutable() => {:?}", self, ret);
        ret
    }

    pub fn to_user_str(&self) -> &'static str {
        match *self {
            McDeclared | McInherited => "mutable",
            McImmutable => "immutable",
        }
    }
}

impl<'a, 'tcx> MemCategorizationContext<'a, 'tcx, 'tcx> {
    pub fn new(tcx: TyCtxt<'a, 'tcx, 'tcx>,
               region_maps: &'a RegionMaps,
               tables: &'a ty::TypeckTables<'tcx>)
               -> MemCategorizationContext<'a, 'tcx, 'tcx> {
        MemCategorizationContext { tcx, region_maps, tables, infcx: None }
    }
}

impl<'a, 'gcx, 'tcx> MemCategorizationContext<'a, 'gcx, 'tcx> {
    pub fn with_infer(infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
                      region_maps: &'a RegionMaps,
                      tables: &'a ty::TypeckTables<'tcx>)
                      -> MemCategorizationContext<'a, 'gcx, 'tcx> {
        MemCategorizationContext {
            tcx: infcx.tcx,
            region_maps,
            tables,
            infcx: Some(infcx),
        }
    }

    pub fn type_moves_by_default(&self,
                                 param_env: ty::ParamEnv<'tcx>,
                                 ty: Ty<'tcx>,
                                 span: Span)
                                 -> bool {
        self.infcx.map(|infcx| infcx.type_moves_by_default(param_env, ty, span))
            .or_else(|| {
                self.tcx.lift_to_global(&(param_env, ty)).map(|(param_env, ty)| {
                    ty.moves_by_default(self.tcx.global_tcx(), param_env, span)
                })
            })
            .unwrap_or(true)
    }

    fn resolve_type_vars_if_possible<T>(&self, value: &T) -> T
        where T: TypeFoldable<'tcx>
    {
        self.infcx.map(|infcx| infcx.resolve_type_vars_if_possible(value))
            .unwrap_or_else(|| value.clone())
    }

    fn is_tainted_by_errors(&self) -> bool {
        self.infcx.map_or(false, |infcx| infcx.is_tainted_by_errors())
    }

    fn resolve_type_vars_or_error(&self,
                                  id: ast::NodeId,
                                  ty: Option<Ty<'tcx>>)
                                  -> McResult<Ty<'tcx>> {
        match ty {
            Some(ty) => {
                let ty = self.resolve_type_vars_if_possible(&ty);
                if ty.references_error() || ty.is_ty_var() {
                    debug!("resolve_type_vars_or_error: error from {:?}", ty);
                    Err(())
                } else {
                    Ok(ty)
                }
            }
            // FIXME
            None if self.is_tainted_by_errors() => Err(()),
            None => {
                bug!("no type for node {}: {} in mem_categorization",
                     id, self.tcx.hir.node_to_string(id));
            }
        }
    }

    pub fn node_ty(&self, id: ast::NodeId) -> McResult<Ty<'tcx>> {
        self.resolve_type_vars_or_error(id, self.tables.node_id_to_type_opt(id))
    }

    pub fn expr_ty(&self, expr: &hir::Expr) -> McResult<Ty<'tcx>> {
        self.resolve_type_vars_or_error(expr.id, self.tables.expr_ty_opt(expr))
    }

    pub fn expr_ty_adjusted(&self, expr: &hir::Expr) -> McResult<Ty<'tcx>> {
        self.resolve_type_vars_or_error(expr.id, self.tables.expr_ty_adjusted_opt(expr))
    }

    fn pat_ty(&self, pat: &hir::Pat) -> McResult<Ty<'tcx>> {
        let base_ty = self.node_ty(pat.id)?;
        // FIXME (Issue #18207): This code detects whether we are
        // looking at a `ref x`, and if so, figures out what the type
        // *being borrowed* is.  But ideally we would put in a more
        // fundamental fix to this conflated use of the node id.
        let ret_ty = match pat.node {
            PatKind::Binding(..) => {
                let bm = *self.tables.pat_binding_modes.get(&pat.id).expect("missing binding mode");
                if let ty::BindByReference(_) = bm {
                    // a bind-by-ref means that the base_ty will be the type of the ident itself,
                    // but what we want here is the type of the underlying value being borrowed.
                    // So peel off one-level, turning the &T into T.
                    match base_ty.builtin_deref(false, ty::NoPreference) {
                        Some(t) => t.ty,
                        None => {
                            debug!("By-ref binding of non-derefable type {:?}", base_ty);
                            return Err(());
                        }
                    }
                } else {
                    base_ty
                }
            }
            _ => base_ty,
        };
        debug!("pat_ty(pat={:?}) base_ty={:?} ret_ty={:?}",
               pat, base_ty, ret_ty);
        Ok(ret_ty)
    }

    pub fn cat_expr(&self, expr: &hir::Expr) -> McResult<cmt<'tcx>> {
        // This recursion helper avoids going through *too many*
        // adjustments, since *only* non-overloaded deref recurses.
        fn helper<'a, 'gcx, 'tcx>(mc: &MemCategorizationContext<'a, 'gcx, 'tcx>,
                                  expr: &hir::Expr,
                                  adjustments: &[adjustment::Adjustment<'tcx>])
                                   -> McResult<cmt<'tcx>> {
            match adjustments.split_last() {
                None => mc.cat_expr_unadjusted(expr),
                Some((adjustment, previous)) => {
                    mc.cat_expr_adjusted_with(expr, || helper(mc, expr, previous), adjustment)
                }
            }
        }

        helper(self, expr, self.tables.expr_adjustments(expr))
    }

    pub fn cat_expr_adjusted(&self, expr: &hir::Expr,
                             previous: cmt<'tcx>,
                             adjustment: &adjustment::Adjustment<'tcx>)
                             -> McResult<cmt<'tcx>> {
        self.cat_expr_adjusted_with(expr, || Ok(previous), adjustment)
    }

    fn cat_expr_adjusted_with<F>(&self, expr: &hir::Expr,
                                 previous: F,
                                 adjustment: &adjustment::Adjustment<'tcx>)
                                 -> McResult<cmt<'tcx>>
        where F: FnOnce() -> McResult<cmt<'tcx>>
    {
        debug!("cat_expr_adjusted_with({:?}): {:?}", adjustment, expr);
        let target = self.resolve_type_vars_if_possible(&adjustment.target);
        match adjustment.kind {
            adjustment::Adjust::Deref(overloaded) => {
                // Equivalent to *expr or something similar.
                let base = if let Some(deref) = overloaded {
                    let ref_ty = self.tcx.mk_ref(deref.region, ty::TypeAndMut {
                        ty: target,
                        mutbl: deref.mutbl,
                    });
                    self.cat_rvalue_node(expr.id, expr.span, ref_ty)
                } else {
                    previous()?
                };
                self.cat_deref(expr, base, false)
            }

            adjustment::Adjust::NeverToAny |
            adjustment::Adjust::ReifyFnPointer |
            adjustment::Adjust::UnsafeFnPointer |
            adjustment::Adjust::ClosureFnPointer |
            adjustment::Adjust::MutToConstPointer |
            adjustment::Adjust::Borrow(_) |
            adjustment::Adjust::Unsize => {
                // Result is an rvalue.
                Ok(self.cat_rvalue_node(expr.id, expr.span, target))
            }
        }
    }

    pub fn cat_expr_unadjusted(&self, expr: &hir::Expr) -> McResult<cmt<'tcx>> {
        debug!("cat_expr: id={} expr={:?}", expr.id, expr);

        let expr_ty = self.expr_ty(expr)?;
        match expr.node {
          hir::ExprUnary(hir::UnDeref, ref e_base) => {
            if self.tables.is_method_call(expr) {
                self.cat_overloaded_lvalue(expr, e_base, false)
            } else {
                let base_cmt = self.cat_expr(&e_base)?;
                self.cat_deref(expr, base_cmt, false)
            }
          }

          hir::ExprField(ref base, f_name) => {
            let base_cmt = self.cat_expr(&base)?;
            debug!("cat_expr(cat_field): id={} expr={:?} base={:?}",
                   expr.id,
                   expr,
                   base_cmt);
            Ok(self.cat_field(expr, base_cmt, f_name.node, expr_ty))
          }

          hir::ExprTupField(ref base, idx) => {
            let base_cmt = self.cat_expr(&base)?;
            Ok(self.cat_tup_field(expr, base_cmt, idx.node, expr_ty))
          }

          hir::ExprIndex(ref base, _) => {
            if self.tables.is_method_call(expr) {
                // If this is an index implemented by a method call, then it
                // will include an implicit deref of the result.
                // The call to index() returns a `&T` value, which
                // is an rvalue. That is what we will be
                // dereferencing.
                self.cat_overloaded_lvalue(expr, base, true)
            } else {
                let base_cmt = self.cat_expr(&base)?;
                self.cat_index(expr, base_cmt, expr_ty, InteriorOffsetKind::Index)
            }
          }

          hir::ExprPath(ref qpath) => {
            let def = self.tables.qpath_def(qpath, expr.id);
            self.cat_def(expr.id, expr.span, expr_ty, def)
          }

          hir::ExprType(ref e, _) => {
            self.cat_expr(&e)
          }

          hir::ExprAddrOf(..) | hir::ExprCall(..) |
          hir::ExprAssign(..) | hir::ExprAssignOp(..) |
          hir::ExprClosure(..) | hir::ExprRet(..) |
          hir::ExprUnary(..) | hir::ExprYield(..) |
          hir::ExprMethodCall(..) | hir::ExprCast(..) |
          hir::ExprArray(..) | hir::ExprTup(..) | hir::ExprIf(..) |
          hir::ExprBinary(..) | hir::ExprWhile(..) |
          hir::ExprBlock(..) | hir::ExprLoop(..) | hir::ExprMatch(..) |
          hir::ExprLit(..) | hir::ExprBreak(..) |
          hir::ExprAgain(..) | hir::ExprStruct(..) | hir::ExprRepeat(..) |
          hir::ExprInlineAsm(..) | hir::ExprBox(..) => {
            Ok(self.cat_rvalue_node(expr.id(), expr.span(), expr_ty))
          }
        }
    }

    pub fn cat_def(&self,
                   id: ast::NodeId,
                   span: Span,
                   expr_ty: Ty<'tcx>,
                   def: Def)
                   -> McResult<cmt<'tcx>> {
        debug!("cat_def: id={} expr={:?} def={:?}",
               id, expr_ty, def);

        match def {
          Def::StructCtor(..) | Def::VariantCtor(..) | Def::Const(..) |
          Def::AssociatedConst(..) | Def::Fn(..) | Def::Method(..) => {
                Ok(self.cat_rvalue_node(id, span, expr_ty))
          }

          Def::Static(_, mutbl) => {
              Ok(Rc::new(cmt_ {
                  id:id,
                  span:span,
                  cat:Categorization::StaticItem,
                  mutbl: if mutbl { McDeclared } else { McImmutable},
                  ty:expr_ty,
                  note: NoteNone
              }))
          }

          Def::Upvar(def_id, _, fn_node_id) => {
              let var_id = self.tcx.hir.as_local_node_id(def_id).unwrap();
              self.cat_upvar(id, span, var_id, fn_node_id)
          }

          Def::Local(def_id) => {
            let vid = self.tcx.hir.as_local_node_id(def_id).unwrap();
            Ok(Rc::new(cmt_ {
                id,
                span,
                cat: Categorization::Local(vid),
                mutbl: MutabilityCategory::from_local(self.tcx, self.tables, vid),
                ty: expr_ty,
                note: NoteNone
            }))
          }

          def => span_bug!(span, "unexpected definition in memory categorization: {:?}", def)
        }
    }

    // Categorize an upvar, complete with invisible derefs of closure
    // environment and upvar reference as appropriate.
    fn cat_upvar(&self,
                 id: ast::NodeId,
                 span: Span,
                 var_id: ast::NodeId,
                 fn_node_id: ast::NodeId)
                 -> McResult<cmt<'tcx>>
    {
        // An upvar can have up to 3 components. We translate first to a
        // `Categorization::Upvar`, which is itself a fiction -- it represents the reference to the
        // field from the environment.
        //
        // `Categorization::Upvar`.  Next, we add a deref through the implicit
        // environment pointer with an anonymous free region 'env and
        // appropriate borrow kind for closure kinds that take self by
        // reference.  Finally, if the upvar was captured
        // by-reference, we add a deref through that reference.  The
        // region of this reference is an inference variable 'up that
        // was previously generated and recorded in the upvar borrow
        // map.  The borrow kind bk is inferred by based on how the
        // upvar is used.
        //
        // This results in the following table for concrete closure
        // types:
        //
        //                | move                 | ref
        // ---------------+----------------------+-------------------------------
        // Fn             | copied -> &'env      | upvar -> &'env -> &'up bk
        // FnMut          | copied -> &'env mut  | upvar -> &'env mut -> &'up bk
        // FnOnce         | copied               | upvar -> &'up bk

        let kind = match self.tables.closure_kinds.get(&fn_node_id) {
            Some(&(kind, _)) => kind,
            None => {
                let ty = self.node_ty(fn_node_id)?;
                match ty.sty {
                    ty::TyGenerator(..) => ty::ClosureKind::FnOnce,
                    _ => span_bug!(span, "missing closure kind"),
                }
            }
        };

        let upvar_id = ty::UpvarId { var_id,
                                     closure_expr_id: fn_node_id };
        let var_ty = self.node_ty(var_id)?;

        // Mutability of original variable itself
        let var_mutbl = MutabilityCategory::from_local(self.tcx, self.tables, var_id);

        // Construct the upvar. This represents access to the field
        // from the environment (perhaps we should eventually desugar
        // this field further, but it will do for now).
        let cmt_result = cmt_ {
            id,
            span,
            cat: Categorization::Upvar(Upvar {id: upvar_id, kind: kind}),
            mutbl: var_mutbl,
            ty: var_ty,
            note: NoteNone
        };

        // If this is a `FnMut` or `Fn` closure, then the above is
        // conceptually a `&mut` or `&` reference, so we have to add a
        // deref.
        let cmt_result = match kind {
            ty::ClosureKind::FnOnce => {
                cmt_result
            }
            ty::ClosureKind::FnMut => {
                self.env_deref(id, span, upvar_id, var_mutbl, ty::MutBorrow, cmt_result)
            }
            ty::ClosureKind::Fn => {
                self.env_deref(id, span, upvar_id, var_mutbl, ty::ImmBorrow, cmt_result)
            }
        };

        // If this is a by-ref capture, then the upvar we loaded is
        // actually a reference, so we have to add an implicit deref
        // for that.
        let upvar_id = ty::UpvarId { var_id,
                                     closure_expr_id: fn_node_id };
        let upvar_capture = self.tables.upvar_capture(upvar_id);
        let cmt_result = match upvar_capture {
            ty::UpvarCapture::ByValue => {
                cmt_result
            }
            ty::UpvarCapture::ByRef(upvar_borrow) => {
                let ptr = BorrowedPtr(upvar_borrow.kind, upvar_borrow.region);
                cmt_ {
                    id,
                    span,
                    cat: Categorization::Deref(Rc::new(cmt_result), ptr),
                    mutbl: MutabilityCategory::from_borrow_kind(upvar_borrow.kind),
                    ty: var_ty,
                    note: NoteUpvarRef(upvar_id)
                }
            }
        };

        let ret = Rc::new(cmt_result);
        debug!("cat_upvar ret={:?}", ret);
        Ok(ret)
    }

    fn env_deref(&self,
                 id: ast::NodeId,
                 span: Span,
                 upvar_id: ty::UpvarId,
                 upvar_mutbl: MutabilityCategory,
                 env_borrow_kind: ty::BorrowKind,
                 cmt_result: cmt_<'tcx>)
                 -> cmt_<'tcx>
    {
        // Region of environment pointer
        let env_region = self.tcx.mk_region(ty::ReFree(ty::FreeRegion {
            // The environment of a closure is guaranteed to
            // outlive any bindings introduced in the body of the
            // closure itself.
            scope: self.tcx.hir.local_def_id(upvar_id.closure_expr_id),
            bound_region: ty::BrEnv
        }));

        let env_ptr = BorrowedPtr(env_borrow_kind, env_region);

        let var_ty = cmt_result.ty;

        // We need to add the env deref.  This means
        // that the above is actually immutable and
        // has a ref type.  However, nothing should
        // actually look at the type, so we can get
        // away with stuffing a `TyError` in there
        // instead of bothering to construct a proper
        // one.
        let cmt_result = cmt_ {
            mutbl: McImmutable,
            ty: self.tcx.types.err,
            ..cmt_result
        };

        let mut deref_mutbl = MutabilityCategory::from_borrow_kind(env_borrow_kind);

        // Issue #18335. If variable is declared as immutable, override the
        // mutability from the environment and substitute an `&T` anyway.
        match upvar_mutbl {
            McImmutable => { deref_mutbl = McImmutable; }
            McDeclared | McInherited => { }
        }

        let ret = cmt_ {
            id,
            span,
            cat: Categorization::Deref(Rc::new(cmt_result), env_ptr),
            mutbl: deref_mutbl,
            ty: var_ty,
            note: NoteClosureEnv(upvar_id)
        };

        debug!("env_deref ret {:?}", ret);

        ret
    }

    /// Returns the lifetime of a temporary created by expr with id `id`.
    /// This could be `'static` if `id` is part of a constant expression.
    pub fn temporary_scope(&self, id: ast::NodeId) -> ty::Region<'tcx>
    {
        let scope = self.region_maps.temporary_scope(id);
        self.tcx.mk_region(match scope {
            Some(scope) => ty::ReScope(scope),
            None => ty::ReStatic
        })
    }

    pub fn cat_rvalue_node(&self,
                           id: ast::NodeId,
                           span: Span,
                           expr_ty: Ty<'tcx>)
                           -> cmt<'tcx> {
        let promotable = self.tcx.rvalue_promotable_to_static.borrow().get(&id).cloned()
                                   .unwrap_or(false);

        // When the corresponding feature isn't toggled, only promote `[T; 0]`.
        let promotable = match expr_ty.sty {
            ty::TyArray(_, 0) => true,
            _ => promotable && self.tcx.sess.features.borrow().rvalue_static_promotion,
        };

        // Compute maximum lifetime of this rvalue. This is 'static if
        // we can promote to a constant, otherwise equal to enclosing temp
        // lifetime.
        let re = if promotable {
            self.tcx.types.re_static
        } else {
            self.temporary_scope(id)
        };
        let ret = self.cat_rvalue(id, span, re, expr_ty);
        debug!("cat_rvalue_node ret {:?}", ret);
        ret
    }

    pub fn cat_rvalue(&self,
                      cmt_id: ast::NodeId,
                      span: Span,
                      temp_scope: ty::Region<'tcx>,
                      expr_ty: Ty<'tcx>) -> cmt<'tcx> {
        let ret = Rc::new(cmt_ {
            id:cmt_id,
            span:span,
            cat:Categorization::Rvalue(temp_scope),
            mutbl:McDeclared,
            ty:expr_ty,
            note: NoteNone
        });
        debug!("cat_rvalue ret {:?}", ret);
        ret
    }

    pub fn cat_field<N:ast_node>(&self,
                                 node: &N,
                                 base_cmt: cmt<'tcx>,
                                 f_name: ast::Name,
                                 f_ty: Ty<'tcx>)
                                 -> cmt<'tcx> {
        let ret = Rc::new(cmt_ {
            id: node.id(),
            span: node.span(),
            mutbl: base_cmt.mutbl.inherit(),
            cat: Categorization::Interior(base_cmt, InteriorField(NamedField(f_name))),
            ty: f_ty,
            note: NoteNone
        });
        debug!("cat_field ret {:?}", ret);
        ret
    }

    pub fn cat_tup_field<N:ast_node>(&self,
                                     node: &N,
                                     base_cmt: cmt<'tcx>,
                                     f_idx: usize,
                                     f_ty: Ty<'tcx>)
                                     -> cmt<'tcx> {
        let ret = Rc::new(cmt_ {
            id: node.id(),
            span: node.span(),
            mutbl: base_cmt.mutbl.inherit(),
            cat: Categorization::Interior(base_cmt, InteriorField(PositionalField(f_idx))),
            ty: f_ty,
            note: NoteNone
        });
        debug!("cat_tup_field ret {:?}", ret);
        ret
    }

    fn cat_overloaded_lvalue(&self,
                             expr: &hir::Expr,
                             base: &hir::Expr,
                             implicit: bool)
                             -> McResult<cmt<'tcx>> {
        debug!("cat_overloaded_lvalue: implicit={}", implicit);

        // Reconstruct the output assuming it's a reference with the
        // same region and mutability as the receiver. This holds for
        // `Deref(Mut)::Deref(_mut)` and `Index(Mut)::index(_mut)`.
        let lvalue_ty = self.expr_ty(expr)?;
        let base_ty = self.expr_ty_adjusted(base)?;

        let (region, mutbl) = match base_ty.sty {
            ty::TyRef(region, mt) => (region, mt.mutbl),
            _ => {
                span_bug!(expr.span, "cat_overloaded_lvalue: base is not a reference")
            }
        };
        let ref_ty = self.tcx.mk_ref(region, ty::TypeAndMut {
            ty: lvalue_ty,
            mutbl,
        });

        let base_cmt = self.cat_rvalue_node(expr.id, expr.span, ref_ty);
        self.cat_deref(expr, base_cmt, implicit)
    }

    pub fn cat_deref<N:ast_node>(&self,
                                 node: &N,
                                 base_cmt: cmt<'tcx>,
                                 implicit: bool)
                                 -> McResult<cmt<'tcx>> {
        debug!("cat_deref: base_cmt={:?}", base_cmt);

        let base_cmt_ty = base_cmt.ty;
        let deref_ty = match base_cmt_ty.builtin_deref(true, ty::NoPreference) {
            Some(mt) => mt.ty,
            None => {
                debug!("Explicit deref of non-derefable type: {:?}",
                       base_cmt_ty);
                return Err(());
            }
        };

        let ptr = match base_cmt.ty.sty {
            ty::TyAdt(def, ..) if def.is_box() => Unique,
            ty::TyRawPtr(ref mt) => UnsafePtr(mt.mutbl),
            ty::TyRef(r, mt) => {
                let bk = ty::BorrowKind::from_mutbl(mt.mutbl);
                if implicit { Implicit(bk, r) } else { BorrowedPtr(bk, r) }
            }
            ref ty => bug!("unexpected type in cat_deref: {:?}", ty)
        };
        let ret = Rc::new(cmt_ {
            id: node.id(),
            span: node.span(),
            // For unique ptrs, we inherit mutability from the owning reference.
            mutbl: MutabilityCategory::from_pointer_kind(base_cmt.mutbl, ptr),
            cat: Categorization::Deref(base_cmt, ptr),
            ty: deref_ty,
            note: NoteNone
        });
        debug!("cat_deref ret {:?}", ret);
        Ok(ret)
    }

    fn cat_index<N:ast_node>(&self,
                             elt: &N,
                             base_cmt: cmt<'tcx>,
                             element_ty: Ty<'tcx>,
                             context: InteriorOffsetKind)
                             -> McResult<cmt<'tcx>> {
        //! Creates a cmt for an indexing operation (`[]`).
        //!
        //! One subtle aspect of indexing that may not be
        //! immediately obvious: for anything other than a fixed-length
        //! vector, an operation like `x[y]` actually consists of two
        //! disjoint (from the point of view of borrowck) operations.
        //! The first is a deref of `x` to create a pointer `p` that points
        //! at the first element in the array. The second operation is
        //! an index which adds `y*sizeof(T)` to `p` to obtain the
        //! pointer to `x[y]`. `cat_index` will produce a resulting
        //! cmt containing both this deref and the indexing,
        //! presuming that `base_cmt` is not of fixed-length type.
        //!
        //! # Parameters
        //! - `elt`: the AST node being indexed
        //! - `base_cmt`: the cmt of `elt`

        let interior_elem = InteriorElement(context);
        let ret =
            self.cat_imm_interior(elt, base_cmt, element_ty, interior_elem);
        debug!("cat_index ret {:?}", ret);
        return Ok(ret);
    }

    pub fn cat_imm_interior<N:ast_node>(&self,
                                        node: &N,
                                        base_cmt: cmt<'tcx>,
                                        interior_ty: Ty<'tcx>,
                                        interior: InteriorKind)
                                        -> cmt<'tcx> {
        let ret = Rc::new(cmt_ {
            id: node.id(),
            span: node.span(),
            mutbl: base_cmt.mutbl.inherit(),
            cat: Categorization::Interior(base_cmt, interior),
            ty: interior_ty,
            note: NoteNone
        });
        debug!("cat_imm_interior ret={:?}", ret);
        ret
    }

    pub fn cat_downcast_if_needed<N:ast_node>(&self,
                                              node: &N,
                                              base_cmt: cmt<'tcx>,
                                              variant_did: DefId)
                                              -> cmt<'tcx> {
        // univariant enums do not need downcasts
        let base_did = self.tcx.parent_def_id(variant_did).unwrap();
        if !self.tcx.adt_def(base_did).is_univariant() {
            let base_ty = base_cmt.ty;
            let ret = Rc::new(cmt_ {
                id: node.id(),
                span: node.span(),
                mutbl: base_cmt.mutbl.inherit(),
                cat: Categorization::Downcast(base_cmt, variant_did),
                ty: base_ty,
                note: NoteNone
            });
            debug!("cat_downcast ret={:?}", ret);
            ret
        } else {
            debug!("cat_downcast univariant={:?}", base_cmt);
            base_cmt
        }
    }

    pub fn cat_pattern<F>(&self, cmt: cmt<'tcx>, pat: &hir::Pat, mut op: F) -> McResult<()>
        where F: FnMut(cmt<'tcx>, &hir::Pat),
    {
        self.cat_pattern_(cmt, pat, &mut op)
    }

    // FIXME(#19596) This is a workaround, but there should be a better way to do this
    fn cat_pattern_<F>(&self, cmt: cmt<'tcx>, pat: &hir::Pat, op: &mut F) -> McResult<()>
        where F : FnMut(cmt<'tcx>, &hir::Pat)
    {
        // Here, `cmt` is the categorization for the value being
        // matched and pat is the pattern it is being matched against.
        //
        // In general, the way that this works is that we walk down
        // the pattern, constructing a cmt that represents the path
        // that will be taken to reach the value being matched.
        //
        // When we encounter named bindings, we take the cmt that has
        // been built up and pass it off to guarantee_valid() so that
        // we can be sure that the binding will remain valid for the
        // duration of the arm.
        //
        // (*2) There is subtlety concerning the correspondence between
        // pattern ids and types as compared to *expression* ids and
        // types. This is explained briefly. on the definition of the
        // type `cmt`, so go off and read what it says there, then
        // come back and I'll dive into a bit more detail here. :) OK,
        // back?
        //
        // In general, the id of the cmt should be the node that
        // "produces" the value---patterns aren't executable code
        // exactly, but I consider them to "execute" when they match a
        // value, and I consider them to produce the value that was
        // matched. So if you have something like:
        //
        //     let x = @@3;
        //     match x {
        //       @@y { ... }
        //     }
        //
        // In this case, the cmt and the relevant ids would be:
        //
        //     CMT             Id                  Type of Id Type of cmt
        //
        //     local(x)->@->@
        //     ^~~~~~~^        `x` from discr      @@int      @@int
        //     ^~~~~~~~~~^     `@@y` pattern node  @@int      @int
        //     ^~~~~~~~~~~~~^  `@y` pattern node   @int       int
        //
        // You can see that the types of the id and the cmt are in
        // sync in the first line, because that id is actually the id
        // of an expression. But once we get to pattern ids, the types
        // step out of sync again. So you'll see below that we always
        // get the type of the *subpattern* and use that.

        debug!("cat_pattern: {:?} cmt={:?}", pat, cmt);

        op(cmt.clone(), pat);

        match pat.node {
          PatKind::TupleStruct(ref qpath, ref subpats, ddpos) => {
            let def = self.tables.qpath_def(qpath, pat.id);
            let (cmt, expected_len) = match def {
                Def::Err => {
                    debug!("access to unresolvable pattern {:?}", pat);
                    return Err(())
                }
                Def::VariantCtor(def_id, CtorKind::Fn) => {
                    let enum_def = self.tcx.parent_def_id(def_id).unwrap();
                    (self.cat_downcast_if_needed(pat, cmt, def_id),
                     self.tcx.adt_def(enum_def).variant_with_id(def_id).fields.len())
                }
                Def::StructCtor(_, CtorKind::Fn) => {
                    match self.pat_ty(&pat)?.sty {
                        ty::TyAdt(adt_def, _) => {
                            (cmt, adt_def.struct_variant().fields.len())
                        }
                        ref ty => {
                            span_bug!(pat.span, "tuple struct pattern unexpected type {:?}", ty);
                        }
                    }
                }
                def => {
                    span_bug!(pat.span, "tuple struct pattern didn't resolve \
                                         to variant or struct {:?}", def);
                }
            };

            for (i, subpat) in subpats.iter().enumerate_and_adjust(expected_len, ddpos) {
                let subpat_ty = self.pat_ty(&subpat)?; // see (*2)
                let subcmt = self.cat_imm_interior(pat, cmt.clone(), subpat_ty,
                                                   InteriorField(PositionalField(i)));
                self.cat_pattern_(subcmt, &subpat, op)?;
            }
          }

          PatKind::Struct(ref qpath, ref field_pats, _) => {
            // {f1: p1, ..., fN: pN}
            let def = self.tables.qpath_def(qpath, pat.id);
            let cmt = match def {
                Def::Err => {
                    debug!("access to unresolvable pattern {:?}", pat);
                    return Err(())
                },
                Def::Variant(variant_did) |
                Def::VariantCtor(variant_did, ..) => {
                    self.cat_downcast_if_needed(pat, cmt, variant_did)
                },
                _ => cmt
            };

            for fp in field_pats {
                let field_ty = self.pat_ty(&fp.node.pat)?; // see (*2)
                let cmt_field = self.cat_field(pat, cmt.clone(), fp.node.name, field_ty);
                self.cat_pattern_(cmt_field, &fp.node.pat, op)?;
            }
          }

          PatKind::Binding(.., Some(ref subpat)) => {
              self.cat_pattern_(cmt, &subpat, op)?;
          }

          PatKind::Tuple(ref subpats, ddpos) => {
            // (p1, ..., pN)
            let expected_len = match self.pat_ty(&pat)?.sty {
                ty::TyTuple(ref tys, _) => tys.len(),
                ref ty => span_bug!(pat.span, "tuple pattern unexpected type {:?}", ty),
            };
            for (i, subpat) in subpats.iter().enumerate_and_adjust(expected_len, ddpos) {
                let subpat_ty = self.pat_ty(&subpat)?; // see (*2)
                let subcmt = self.cat_imm_interior(pat, cmt.clone(), subpat_ty,
                                                   InteriorField(PositionalField(i)));
                self.cat_pattern_(subcmt, &subpat, op)?;
            }
          }

          PatKind::Box(ref subpat) | PatKind::Ref(ref subpat, _) => {
            // box p1, &p1, &mut p1.  we can ignore the mutability of
            // PatKind::Ref since that information is already contained
            // in the type.
            let subcmt = self.cat_deref(pat, cmt, false)?;
            self.cat_pattern_(subcmt, &subpat, op)?;
          }

          PatKind::Slice(ref before, ref slice, ref after) => {
            let element_ty = match cmt.ty.builtin_index() {
                Some(ty) => ty,
                None => {
                    debug!("Explicit index of non-indexable type {:?}", cmt);
                    return Err(());
                }
            };
            let context = InteriorOffsetKind::Pattern;
            let elt_cmt = self.cat_index(pat, cmt, element_ty, context)?;
            for before_pat in before {
                self.cat_pattern_(elt_cmt.clone(), &before_pat, op)?;
            }
            if let Some(ref slice_pat) = *slice {
                self.cat_pattern_(elt_cmt.clone(), &slice_pat, op)?;
            }
            for after_pat in after {
                self.cat_pattern_(elt_cmt.clone(), &after_pat, op)?;
            }
          }

          PatKind::Path(_) | PatKind::Binding(.., None) |
          PatKind::Lit(..) | PatKind::Range(..) | PatKind::Wild => {
            // always ok
          }
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum Aliasability {
    FreelyAliasable(AliasableReason),
    NonAliasable,
    ImmutableUnique(Box<Aliasability>),
}

#[derive(Copy, Clone, Debug)]
pub enum AliasableReason {
    AliasableBorrowed,
    AliasableStatic,
    AliasableStaticMut,
}

impl<'tcx> cmt_<'tcx> {
    pub fn guarantor(&self) -> cmt<'tcx> {
        //! Returns `self` after stripping away any derefs or
        //! interior content. The return value is basically the `cmt` which
        //! determines how long the value in `self` remains live.

        match self.cat {
            Categorization::Rvalue(..) |
            Categorization::StaticItem |
            Categorization::Local(..) |
            Categorization::Deref(_, UnsafePtr(..)) |
            Categorization::Deref(_, BorrowedPtr(..)) |
            Categorization::Deref(_, Implicit(..)) |
            Categorization::Upvar(..) => {
                Rc::new((*self).clone())
            }
            Categorization::Downcast(ref b, _) |
            Categorization::Interior(ref b, _) |
            Categorization::Deref(ref b, Unique) => {
                b.guarantor()
            }
        }
    }

    /// Returns `FreelyAliasable(_)` if this lvalue represents a freely aliasable pointer type.
    pub fn freely_aliasable(&self) -> Aliasability {
        // Maybe non-obvious: copied upvars can only be considered
        // non-aliasable in once closures, since any other kind can be
        // aliased and eventually recused.

        match self.cat {
            Categorization::Deref(ref b, BorrowedPtr(ty::MutBorrow, _)) |
            Categorization::Deref(ref b, Implicit(ty::MutBorrow, _)) |
            Categorization::Deref(ref b, BorrowedPtr(ty::UniqueImmBorrow, _)) |
            Categorization::Deref(ref b, Implicit(ty::UniqueImmBorrow, _)) |
            Categorization::Deref(ref b, Unique) |
            Categorization::Downcast(ref b, _) |
            Categorization::Interior(ref b, _) => {
                // Aliasability depends on base cmt
                b.freely_aliasable()
            }

            Categorization::Rvalue(..) |
            Categorization::Local(..) |
            Categorization::Upvar(..) |
            Categorization::Deref(_, UnsafePtr(..)) => { // yes, it's aliasable, but...
                NonAliasable
            }

            Categorization::StaticItem => {
                if self.mutbl.is_mutable() {
                    FreelyAliasable(AliasableStaticMut)
                } else {
                    FreelyAliasable(AliasableStatic)
                }
            }

            Categorization::Deref(_, BorrowedPtr(ty::ImmBorrow, _)) |
            Categorization::Deref(_, Implicit(ty::ImmBorrow, _)) => {
                FreelyAliasable(AliasableBorrowed)
            }
        }
    }

    // Digs down through one or two layers of deref and grabs the cmt
    // for the upvar if a note indicates there is one.
    pub fn upvar(&self) -> Option<cmt<'tcx>> {
        match self.note {
            NoteClosureEnv(..) | NoteUpvarRef(..) => {
                Some(match self.cat {
                    Categorization::Deref(ref inner, _) => {
                        match inner.cat {
                            Categorization::Deref(ref inner, _) => inner.clone(),
                            Categorization::Upvar(..) => inner.clone(),
                            _ => bug!()
                        }
                    }
                    _ => bug!()
                })
            }
            NoteNone => None
        }
    }


    pub fn descriptive_string(&self, tcx: TyCtxt) -> String {
        match self.cat {
            Categorization::StaticItem => {
                "static item".to_string()
            }
            Categorization::Rvalue(..) => {
                "non-lvalue".to_string()
            }
            Categorization::Local(vid) => {
                if tcx.hir.is_argument(vid) {
                    "argument".to_string()
                } else {
                    "local variable".to_string()
                }
            }
            Categorization::Deref(_, pk) => {
                let upvar = self.upvar();
                match upvar.as_ref().map(|i| &i.cat) {
                    Some(&Categorization::Upvar(ref var)) => {
                        var.to_string()
                    }
                    Some(_) => bug!(),
                    None => {
                        match pk {
                            Implicit(..) => {
                                format!("indexed content")
                            }
                            Unique => {
                                format!("`Box` content")
                            }
                            UnsafePtr(..) => {
                                format!("dereference of raw pointer")
                            }
                            BorrowedPtr(..) => {
                                format!("borrowed content")
                            }
                        }
                    }
                }
            }
            Categorization::Interior(_, InteriorField(NamedField(_))) => {
                "field".to_string()
            }
            Categorization::Interior(_, InteriorField(PositionalField(_))) => {
                "anonymous field".to_string()
            }
            Categorization::Interior(_, InteriorElement(InteriorOffsetKind::Index)) => {
                "indexed content".to_string()
            }
            Categorization::Interior(_, InteriorElement(InteriorOffsetKind::Pattern)) => {
                "pattern-bound indexed content".to_string()
            }
            Categorization::Upvar(ref var) => {
                var.to_string()
            }
            Categorization::Downcast(ref cmt, _) => {
                cmt.descriptive_string(tcx)
            }
        }
    }
}

impl<'tcx> fmt::Debug for cmt_<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{{:?} id:{} m:{:?} ty:{:?}}}",
               self.cat,
               self.id,
               self.mutbl,
               self.ty)
    }
}

impl<'tcx> fmt::Debug for Categorization<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Categorization::StaticItem => write!(f, "static"),
            Categorization::Rvalue(r) => { write!(f, "rvalue({:?})", r) }
            Categorization::Local(id) => {
               let name = ty::tls::with(|tcx| tcx.local_var_name_str(id));
               write!(f, "local({})", name)
            }
            Categorization::Upvar(upvar) => {
                write!(f, "upvar({:?})", upvar)
            }
            Categorization::Deref(ref cmt, ptr) => {
                write!(f, "{:?}-{:?}->", cmt.cat, ptr)
            }
            Categorization::Interior(ref cmt, interior) => {
                write!(f, "{:?}.{:?}", cmt.cat, interior)
            }
            Categorization::Downcast(ref cmt, _) => {
                write!(f, "{:?}->(enum)", cmt.cat)
            }
        }
    }
}

pub fn ptr_sigil(ptr: PointerKind) -> &'static str {
    match ptr {
        Unique => "Box",
        BorrowedPtr(ty::ImmBorrow, _) |
        Implicit(ty::ImmBorrow, _) => "&",
        BorrowedPtr(ty::MutBorrow, _) |
        Implicit(ty::MutBorrow, _) => "&mut",
        BorrowedPtr(ty::UniqueImmBorrow, _) |
        Implicit(ty::UniqueImmBorrow, _) => "&unique",
        UnsafePtr(_) => "*",
    }
}

impl<'tcx> fmt::Debug for PointerKind<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Unique => write!(f, "Box"),
            BorrowedPtr(ty::ImmBorrow, ref r) |
            Implicit(ty::ImmBorrow, ref r) => {
                write!(f, "&{:?}", r)
            }
            BorrowedPtr(ty::MutBorrow, ref r) |
            Implicit(ty::MutBorrow, ref r) => {
                write!(f, "&{:?} mut", r)
            }
            BorrowedPtr(ty::UniqueImmBorrow, ref r) |
            Implicit(ty::UniqueImmBorrow, ref r) => {
                write!(f, "&{:?} uniq", r)
            }
            UnsafePtr(_) => write!(f, "*")
        }
    }
}

impl fmt::Debug for InteriorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            InteriorField(NamedField(fld)) => write!(f, "{}", fld),
            InteriorField(PositionalField(i)) => write!(f, "#{}", i),
            InteriorElement(..) => write!(f, "[]"),
        }
    }
}

impl fmt::Debug for Upvar {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}/{:?}", self.id, self.kind)
    }
}

impl fmt::Display for Upvar {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let kind = match self.kind {
            ty::ClosureKind::Fn => "Fn",
            ty::ClosureKind::FnMut => "FnMut",
            ty::ClosureKind::FnOnce => "FnOnce",
        };
        write!(f, "captured outer variable in an `{}` closure", kind)
    }
}
