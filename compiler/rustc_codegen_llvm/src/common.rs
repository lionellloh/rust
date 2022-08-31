//! Code that is useful in various codegen modules.

use crate::consts::{self, const_alloc_to_llvm};
pub use crate::context::CodegenCx;
use crate::llvm::{self, BasicBlock, Bool, ConstantInt, False, OperandBundleDef, True};
use crate::type_::Type;
use crate::type_of::LayoutLlvmExt;
use crate::value::Value;

use rustc_ast::Mutability;
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::*;
use rustc_hir::def_id::DefId;
use rustc_middle::bug;
use rustc_middle::mir::interpret::{ConstAllocation, GlobalAlloc, Scalar};
use rustc_middle::ty::layout::{LayoutOf, TyAndLayout};
use rustc_middle::ty::TyCtxt;
use rustc_session::cstore::{DllCallingConvention, DllImport, PeImportNameType};
use rustc_target::abi::{self, AddressSpace, HasDataLayout, Pointer, Size};
use rustc_target::spec::Target;

use libc::{c_char, c_uint};
use std::fmt::Write;

/*
* A note on nomenclature of linking: "extern", "foreign", and "upcall".
*
* An "extern" is an LLVM symbol we wind up emitting an undefined external
* reference to. This means "we don't have the thing in this compilation unit,
* please make sure you link it in at runtime". This could be a reference to
* C code found in a C library, or rust code found in a rust crate.
*
* Most "externs" are implicitly declared (automatically) as a result of a
* user declaring an extern _module_ dependency; this causes the rust driver
* to locate an extern crate, scan its compilation metadata, and emit extern
* declarations for any symbols used by the declaring crate.
*
* A "foreign" is an extern that references C (or other non-rust ABI) code.
* There is no metadata to scan for extern references so in these cases either
* a header-digester like bindgen, or manual function prototypes, have to
* serve as declarators. So these are usually given explicitly as prototype
* declarations, in rust code, with ABI attributes on them noting which ABI to
* link via.
*
* An "upcall" is a foreign call generated by the compiler (not corresponding
* to any user-written call in the code) into the runtime library, to perform
* some helper task such as bringing a task to life, allocating memory, etc.
*
*/

/// A structure representing an active landing pad for the duration of a basic
/// block.
///
/// Each `Block` may contain an instance of this, indicating whether the block
/// is part of a landing pad or not. This is used to make decision about whether
/// to emit `invoke` instructions (e.g., in a landing pad we don't continue to
/// use `invoke`) and also about various function call metadata.
///
/// For GNU exceptions (`landingpad` + `resume` instructions) this structure is
/// just a bunch of `None` instances (not too interesting), but for MSVC
/// exceptions (`cleanuppad` + `cleanupret` instructions) this contains data.
/// When inside of a landing pad, each function call in LLVM IR needs to be
/// annotated with which landing pad it's a part of. This is accomplished via
/// the `OperandBundleDef` value created for MSVC landing pads.
pub struct Funclet<'ll> {
    cleanuppad: &'ll Value,
    operand: OperandBundleDef<'ll>,
}

impl<'ll> Funclet<'ll> {
    pub fn new(cleanuppad: &'ll Value) -> Self {
        Funclet { cleanuppad, operand: OperandBundleDef::new("funclet", &[cleanuppad]) }
    }

    pub fn cleanuppad(&self) -> &'ll Value {
        self.cleanuppad
    }

    pub fn bundle(&self) -> &OperandBundleDef<'ll> {
        &self.operand
    }
}

impl<'ll> BackendTypes for CodegenCx<'ll, '_> {
    type Value = &'ll Value;
    // FIXME(eddyb) replace this with a `Function` "subclass" of `Value`.
    type Function = &'ll Value;

    type BasicBlock = &'ll BasicBlock;
    type Type = &'ll Type;
    type Funclet = Funclet<'ll>;

    type DIScope = &'ll llvm::debuginfo::DIScope;
    type DILocation = &'ll llvm::debuginfo::DILocation;
    type DIVariable = &'ll llvm::debuginfo::DIVariable;
}

impl<'ll> CodegenCx<'ll, '_> {
    pub fn const_array(&self, ty: &'ll Type, elts: &[&'ll Value]) -> &'ll Value {
        unsafe { llvm::LLVMConstArray(ty, elts.as_ptr(), elts.len() as c_uint) }
    }

    pub fn const_vector(&self, elts: &[&'ll Value]) -> &'ll Value {
        unsafe { llvm::LLVMConstVector(elts.as_ptr(), elts.len() as c_uint) }
    }

    pub fn const_bytes(&self, bytes: &[u8]) -> &'ll Value {
        bytes_in_context(self.llcx, bytes)
    }

    pub fn const_get_elt(&self, v: &'ll Value, idx: u64) -> &'ll Value {
        unsafe {
            assert_eq!(idx as c_uint as u64, idx);
            let r = llvm::LLVMGetAggregateElement(v, idx as c_uint).unwrap();

            debug!("const_get_elt(v={:?}, idx={}, r={:?})", v, idx, r);

            r
        }
    }
}

impl<'ll, 'tcx> ConstMethods<'tcx> for CodegenCx<'ll, 'tcx> {
    fn const_null(&self, t: &'ll Type) -> &'ll Value {
        unsafe { llvm::LLVMConstNull(t) }
    }

    fn const_undef(&self, t: &'ll Type) -> &'ll Value {
        unsafe { llvm::LLVMGetUndef(t) }
    }

    fn const_int(&self, t: &'ll Type, i: i64) -> &'ll Value {
        unsafe { llvm::LLVMConstInt(t, i as u64, True) }
    }

    fn const_uint(&self, t: &'ll Type, i: u64) -> &'ll Value {
        unsafe { llvm::LLVMConstInt(t, i, False) }
    }

    fn const_uint_big(&self, t: &'ll Type, u: u128) -> &'ll Value {
        unsafe {
            let words = [u as u64, (u >> 64) as u64];
            llvm::LLVMConstIntOfArbitraryPrecision(t, 2, words.as_ptr())
        }
    }

    fn const_bool(&self, val: bool) -> &'ll Value {
        self.const_uint(self.type_i1(), val as u64)
    }

    fn const_i16(&self, i: i16) -> &'ll Value {
        self.const_int(self.type_i16(), i as i64)
    }

    fn const_i32(&self, i: i32) -> &'ll Value {
        self.const_int(self.type_i32(), i as i64)
    }

    fn const_u32(&self, i: u32) -> &'ll Value {
        self.const_uint(self.type_i32(), i as u64)
    }

    fn const_u64(&self, i: u64) -> &'ll Value {
        self.const_uint(self.type_i64(), i)
    }

    fn const_usize(&self, i: u64) -> &'ll Value {
        let bit_size = self.data_layout().pointer_size.bits();
        if bit_size < 64 {
            // make sure it doesn't overflow
            assert!(i < (1 << bit_size));
        }

        self.const_uint(self.isize_ty, i)
    }

    fn const_u8(&self, i: u8) -> &'ll Value {
        self.const_uint(self.type_i8(), i as u64)
    }

    fn const_real(&self, t: &'ll Type, val: f64) -> &'ll Value {
        unsafe { llvm::LLVMConstReal(t, val) }
    }

    fn const_str(&self, s: &str) -> (&'ll Value, &'ll Value) {
        let str_global = *self
            .const_str_cache
            .borrow_mut()
            .raw_entry_mut()
            .from_key(s)
            .or_insert_with(|| {
                let sc = self.const_bytes(s.as_bytes());
                let sym = self.generate_local_symbol_name("str");
                let g = self.define_global(&sym, self.val_ty(sc)).unwrap_or_else(|| {
                    bug!("symbol `{}` is already defined", sym);
                });
                unsafe {
                    llvm::LLVMSetInitializer(g, sc);
                    llvm::LLVMSetGlobalConstant(g, True);
                    llvm::LLVMRustSetLinkage(g, llvm::Linkage::InternalLinkage);
                }
                (s.to_owned(), g)
            })
            .1;
        let len = s.len();
        let cs = consts::ptrcast(
            str_global,
            self.type_ptr_to(self.layout_of(self.tcx.types.str_).llvm_type(self)),
        );
        (cs, self.const_usize(len as u64))
    }

    fn const_struct(&self, elts: &[&'ll Value], packed: bool) -> &'ll Value {
        struct_in_context(self.llcx, elts, packed)
    }

    fn const_to_opt_uint(&self, v: &'ll Value) -> Option<u64> {
        try_as_const_integral(v).map(|v| unsafe { llvm::LLVMConstIntGetZExtValue(v) })
    }

    fn const_to_opt_u128(&self, v: &'ll Value, sign_ext: bool) -> Option<u128> {
        try_as_const_integral(v).and_then(|v| unsafe {
            let (mut lo, mut hi) = (0u64, 0u64);
            let success = llvm::LLVMRustConstInt128Get(v, sign_ext, &mut hi, &mut lo);
            success.then_some(hi_lo_to_u128(lo, hi))
        })
    }

    fn zst_to_backend(&self, _llty: &'ll Type) -> &'ll Value {
        self.const_undef(self.type_ix(0))
    }

    fn scalar_to_backend(&self, cv: Scalar, layout: abi::Scalar, llty: &'ll Type) -> &'ll Value {
        let bitsize = if layout.is_bool() { 1 } else { layout.size(self).bits() };
        match cv {
            Scalar::Int(int) => {
                let data = int.assert_bits(layout.size(self));
                let llval = self.const_uint_big(self.type_ix(bitsize), data);
                if layout.primitive() == Pointer {
                    unsafe { llvm::LLVMConstIntToPtr(llval, llty) }
                } else {
                    self.const_bitcast(llval, llty)
                }
            }
            Scalar::Ptr(ptr, _size) => {
                let (alloc_id, offset) = ptr.into_parts();
                let (base_addr, base_addr_space) = match self.tcx.global_alloc(alloc_id) {
                    GlobalAlloc::Memory(alloc) => {
                        let init = const_alloc_to_llvm(self, alloc);
                        let alloc = alloc.inner();
                        let value = match alloc.mutability {
                            Mutability::Mut => self.static_addr_of_mut(init, alloc.align, None),
                            _ => self.static_addr_of(init, alloc.align, None),
                        };
                        if !self.sess().fewer_names() {
                            llvm::set_value_name(value, format!("{:?}", alloc_id).as_bytes());
                        }
                        (value, AddressSpace::DATA)
                    }
                    GlobalAlloc::Function(fn_instance) => (
                        self.get_fn_addr(fn_instance.polymorphize(self.tcx)),
                        self.data_layout().instruction_address_space,
                    ),
                    GlobalAlloc::VTable(ty, trait_ref) => {
                        let alloc = self
                            .tcx
                            .global_alloc(self.tcx.vtable_allocation((ty, trait_ref)))
                            .unwrap_memory();
                        let init = const_alloc_to_llvm(self, alloc);
                        let value = self.static_addr_of(init, alloc.inner().align, None);
                        (value, AddressSpace::DATA)
                    }
                    GlobalAlloc::Static(def_id) => {
                        assert!(self.tcx.is_static(def_id));
                        assert!(!self.tcx.is_thread_local_static(def_id));
                        (self.get_static(def_id), AddressSpace::DATA)
                    }
                };
                let llval = unsafe {
                    llvm::LLVMRustConstInBoundsGEP2(
                        self.type_i8(),
                        self.const_bitcast(base_addr, self.type_i8p_ext(base_addr_space)),
                        &self.const_usize(offset.bytes()),
                        1,
                    )
                };
                if layout.primitive() != Pointer {
                    unsafe { llvm::LLVMConstPtrToInt(llval, llty) }
                } else {
                    self.const_bitcast(llval, llty)
                }
            }
        }
    }

    fn const_data_from_alloc(&self, alloc: ConstAllocation<'tcx>) -> Self::Value {
        const_alloc_to_llvm(self, alloc)
    }

    fn from_const_alloc(
        &self,
        layout: TyAndLayout<'tcx>,
        alloc: ConstAllocation<'tcx>,
        offset: Size,
    ) -> PlaceRef<'tcx, &'ll Value> {
        let alloc_align = alloc.inner().align;
        assert_eq!(alloc_align, layout.align.abi);
        let llty = self.type_ptr_to(layout.llvm_type(self));
        let llval = if layout.size == Size::ZERO {
            let llval = self.const_usize(alloc_align.bytes());
            unsafe { llvm::LLVMConstIntToPtr(llval, llty) }
        } else {
            let init = const_alloc_to_llvm(self, alloc);
            let base_addr = self.static_addr_of(init, alloc_align, None);

            let llval = unsafe {
                llvm::LLVMRustConstInBoundsGEP2(
                    self.type_i8(),
                    self.const_bitcast(base_addr, self.type_i8p()),
                    &self.const_usize(offset.bytes()),
                    1,
                )
            };
            self.const_bitcast(llval, llty)
        };
        PlaceRef::new_sized(llval, layout)
    }

    fn const_ptrcast(&self, val: &'ll Value, ty: &'ll Type) -> &'ll Value {
        consts::ptrcast(val, ty)
    }
}

/// Get the [LLVM type][Type] of a [`Value`].
pub fn val_ty(v: &Value) -> &Type {
    unsafe { llvm::LLVMTypeOf(v) }
}

pub fn bytes_in_context<'ll>(llcx: &'ll llvm::Context, bytes: &[u8]) -> &'ll Value {
    unsafe {
        let ptr = bytes.as_ptr() as *const c_char;
        llvm::LLVMConstStringInContext(llcx, ptr, bytes.len() as c_uint, True)
    }
}

pub fn struct_in_context<'ll>(
    llcx: &'ll llvm::Context,
    elts: &[&'ll Value],
    packed: bool,
) -> &'ll Value {
    unsafe {
        llvm::LLVMConstStructInContext(llcx, elts.as_ptr(), elts.len() as c_uint, packed as Bool)
    }
}

#[inline]
fn hi_lo_to_u128(lo: u64, hi: u64) -> u128 {
    ((hi as u128) << 64) | (lo as u128)
}

fn try_as_const_integral(v: &Value) -> Option<&ConstantInt> {
    unsafe { llvm::LLVMIsAConstantInt(v) }
}

pub(crate) fn get_dllimport<'tcx>(
    tcx: TyCtxt<'tcx>,
    id: DefId,
    name: &str,
) -> Option<&'tcx DllImport> {
    tcx.native_library(id)
        .map(|lib| lib.dll_imports.iter().find(|di| di.name.as_str() == name))
        .flatten()
}

pub(crate) fn is_mingw_gnu_toolchain(target: &Target) -> bool {
    target.vendor == "pc" && target.os == "windows" && target.env == "gnu" && target.abi.is_empty()
}

pub(crate) fn i686_decorated_name(
    dll_import: &DllImport,
    mingw: bool,
    disable_name_mangling: bool,
) -> String {
    let name = dll_import.name.as_str();

    let (add_prefix, add_suffix) = match dll_import.import_name_type {
        Some(PeImportNameType::NoPrefix) => (false, true),
        Some(PeImportNameType::Undecorated) => (false, false),
        _ => (true, true),
    };

    // Worst case: +1 for disable name mangling, +1 for prefix, +4 for suffix (@@__).
    let mut decorated_name = String::with_capacity(name.len() + 6);

    if disable_name_mangling {
        // LLVM uses a binary 1 ('\x01') prefix to a name to indicate that mangling needs to be disabled.
        decorated_name.push('\x01');
    }

    let prefix = if add_prefix && dll_import.is_fn {
        match dll_import.calling_convention {
            DllCallingConvention::C | DllCallingConvention::Vectorcall(_) => None,
            DllCallingConvention::Stdcall(_) => (!mingw
                || dll_import.import_name_type == Some(PeImportNameType::Decorated))
            .then_some('_'),
            DllCallingConvention::Fastcall(_) => Some('@'),
        }
    } else if !dll_import.is_fn && !mingw {
        // For static variables, prefix with '_' on MSVC.
        Some('_')
    } else {
        None
    };
    if let Some(prefix) = prefix {
        decorated_name.push(prefix);
    }

    decorated_name.push_str(name);

    if add_suffix && dll_import.is_fn {
        match dll_import.calling_convention {
            DllCallingConvention::C => {}
            DllCallingConvention::Stdcall(arg_list_size)
            | DllCallingConvention::Fastcall(arg_list_size) => {
                write!(&mut decorated_name, "@{}", arg_list_size).unwrap();
            }
            DllCallingConvention::Vectorcall(arg_list_size) => {
                write!(&mut decorated_name, "@@{}", arg_list_size).unwrap();
            }
        }
    }

    decorated_name
}
