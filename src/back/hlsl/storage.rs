//! Logic related to `ByteAddressBuffer` operations.
//!
//! HLSL backend uses byte address buffers for all storage buffers in IR.
//! Matrices have to be transposed, because HLSL syntax implies row majority.

use super::{
    super::{FunctionCtx, INDENT},
    BackendResult, Error,
};
use crate::{
    proc::{NameKey, TypeResolution},
    Handle,
};

use std::{fmt, mem};

const STORE_TEMP_NAME: &str = "_value";

pub(super) enum SubAccess {
    Offset(u32),
    Index {
        value: Handle<crate::Expression>,
        stride: u32,
    },
}

pub(super) enum StoreValue {
    Expression(Handle<crate::Expression>),
    TempIndex {
        depth: usize,
        index: u32,
        ty: TypeResolution,
    },
    TempAccess {
        depth: usize,
        base: Handle<crate::Type>,
        member_index: u32,
    },
}

impl<W: fmt::Write> super::Writer<'_, W> {
    fn write_storage_address(
        &mut self,
        module: &crate::Module,
        chain: &[SubAccess],
        func_ctx: &FunctionCtx,
    ) -> BackendResult {
        for (i, access) in chain.iter().enumerate() {
            if i != 0 {
                write!(self.out, "+")?;
            }
            match *access {
                SubAccess::Offset(offset) => {
                    write!(self.out, "{}", offset)?;
                }
                SubAccess::Index { value, stride } => {
                    self.write_expr(module, value, func_ctx)?;
                    write!(self.out, "*{}", stride)?;
                }
            }
        }
        Ok(())
    }

    fn write_storage_load_sequence<I: Iterator<Item = (TypeResolution, u32)>>(
        &mut self,
        module: &crate::Module,
        var_handle: Handle<crate::GlobalVariable>,
        sequence: I,
        func_ctx: &FunctionCtx,
    ) -> BackendResult {
        for (i, (ty_resolution, offset)) in sequence.enumerate() {
            // add the index temporarily
            self.temp_access_chain.push(SubAccess::Offset(offset));
            if i != 0 {
                write!(self.out, ", ")?;
            };
            self.write_storage_load(module, var_handle, ty_resolution, func_ctx)?;
            self.temp_access_chain.pop();
        }
        Ok(())
    }

    /// Helper function to write down the Load operation on a `ByteAddressBuffer`.
    pub(super) fn write_storage_load(
        &mut self,
        module: &crate::Module,
        var_handle: Handle<crate::GlobalVariable>,
        result_ty: TypeResolution,
        func_ctx: &FunctionCtx,
    ) -> BackendResult {
        match *result_ty.inner_with(&module.types) {
            crate::TypeInner::Scalar { kind, width: _ } => {
                // working around the borrow checker in `self.write_expr`
                let chain = mem::take(&mut self.temp_access_chain);
                let var_name = &self.names[&NameKey::GlobalVariable(var_handle)];
                let cast = kind.to_hlsl_cast();
                write!(self.out, "{}({}.Load(", cast, var_name)?;
                self.write_storage_address(module, &chain, func_ctx)?;
                write!(self.out, "))")?;
                self.temp_access_chain = chain;
            }
            crate::TypeInner::Vector {
                size,
                kind,
                width: _,
            } => {
                // working around the borrow checker in `self.write_expr`
                let chain = mem::take(&mut self.temp_access_chain);
                let var_name = &self.names[&NameKey::GlobalVariable(var_handle)];
                let cast = kind.to_hlsl_cast();
                write!(self.out, "{}({}.Load{}(", cast, var_name, size as u8)?;
                self.write_storage_address(module, &chain, func_ctx)?;
                write!(self.out, "))")?;
                self.temp_access_chain = chain;
            }
            crate::TypeInner::Matrix {
                columns,
                rows,
                width,
            } => {
                write!(
                    self.out,
                    "transpose({}{}x{}(",
                    crate::ScalarKind::Float.to_hlsl_str(width)?,
                    rows as u8,
                    columns as u8,
                )?;
                let row_stride = width as u32 * columns as u32;
                let iter = (0..rows as u32).map(|i| {
                    let ty_inner = crate::TypeInner::Vector {
                        size: columns,
                        kind: crate::ScalarKind::Float,
                        width,
                    };
                    (TypeResolution::Value(ty_inner), i * row_stride)
                });
                self.write_storage_load_sequence(module, var_handle, iter, func_ctx)?;
                write!(self.out, "))")?;
            }
            crate::TypeInner::Array {
                base,
                size: crate::ArraySize::Constant(const_handle),
                ..
            } => {
                write!(self.out, "{{")?;
                let count = module.constants[const_handle].to_array_length().unwrap();
                let stride = module.types[base].inner.span(&module.constants);
                let iter = (0..count).map(|i| (TypeResolution::Handle(base), stride * i));
                self.write_storage_load_sequence(module, var_handle, iter, func_ctx)?;
                write!(self.out, "}}")?;
            }
            crate::TypeInner::Struct { ref members, .. } => {
                write!(self.out, "{{")?;
                let iter = members
                    .iter()
                    .map(|m| (TypeResolution::Handle(m.ty), m.offset));
                self.write_storage_load_sequence(module, var_handle, iter, func_ctx)?;
                write!(self.out, "}}")?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn write_store_value(
        &mut self,
        module: &crate::Module,
        value: &StoreValue,
        func_ctx: &FunctionCtx,
    ) -> BackendResult {
        match *value {
            StoreValue::Expression(expr) => self.write_expr(module, expr, &func_ctx)?,
            StoreValue::TempIndex {
                depth,
                index,
                ty: _,
            } => write!(self.out, "{}{}[{}]", STORE_TEMP_NAME, depth, index)?,
            StoreValue::TempAccess {
                depth,
                base,
                member_index,
            } => {
                let name = &self.names[&NameKey::StructMember(base, member_index)];
                write!(self.out, "{}{}.{}", STORE_TEMP_NAME, depth, name)?
            }
        }
        Ok(())
    }

    /// Helper function to write down the Store operation on a `ByteAddressBuffer`.
    pub(super) fn write_storage_store(
        &mut self,
        module: &crate::Module,
        var_handle: Handle<crate::GlobalVariable>,
        value: StoreValue,
        func_ctx: &FunctionCtx,
        indent: usize,
    ) -> BackendResult {
        let temp_resolution;
        let ty_resolution = match value {
            StoreValue::Expression(expr) => &func_ctx.info[expr].ty,
            StoreValue::TempIndex {
                depth: _,
                index: _,
                ref ty,
            } => ty,
            StoreValue::TempAccess {
                depth: _,
                base,
                member_index,
            } => {
                let ty_handle = match module.types[base].inner {
                    crate::TypeInner::Struct { ref members, .. } => {
                        members[member_index as usize].ty
                    }
                    _ => unreachable!(),
                };
                temp_resolution = TypeResolution::Handle(ty_handle);
                &temp_resolution
            }
        };
        match *ty_resolution.inner_with(&module.types) {
            crate::TypeInner::Scalar { .. } => {
                // working around the borrow checker in `self.write_expr`
                let chain = mem::take(&mut self.temp_access_chain);
                let var_name = &self.names[&NameKey::GlobalVariable(var_handle)];
                write!(self.out, "{}{}.Store(", INDENT.repeat(indent), var_name)?;
                self.write_storage_address(module, &chain, func_ctx)?;
                write!(self.out, ", asuint(")?;
                self.write_store_value(module, &value, func_ctx)?;
                writeln!(self.out, "));")?;
                self.temp_access_chain = chain;
            }
            crate::TypeInner::Vector { size, .. } => {
                // working around the borrow checker in `self.write_expr`
                let chain = mem::take(&mut self.temp_access_chain);
                let var_name = &self.names[&NameKey::GlobalVariable(var_handle)];
                write!(
                    self.out,
                    "{}{}.Store{}(",
                    INDENT.repeat(indent),
                    var_name,
                    size as u8
                )?;
                self.write_storage_address(module, &chain, func_ctx)?;
                write!(self.out, ", asuint(")?;
                self.write_store_value(module, &value, func_ctx)?;
                writeln!(self.out, "));")?;
                self.temp_access_chain = chain;
            }
            crate::TypeInner::Matrix {
                columns,
                rows,
                width,
            } => {
                // first, assign the value to a temporary
                writeln!(self.out, "{}{{", INDENT.repeat(indent))?;
                let depth = indent + 1;
                write!(
                    self.out,
                    "{}{}{}x{} {}{} = transpose(",
                    INDENT.repeat(indent + 1),
                    crate::ScalarKind::Float.to_hlsl_str(width)?,
                    rows as u8,
                    columns as u8,
                    STORE_TEMP_NAME,
                    depth,
                )?;
                self.write_store_value(module, &value, func_ctx)?;
                writeln!(self.out, ");")?;
                // then iterate the stores
                let row_stride = width as u32 * columns as u32;
                for i in 0..rows as u32 {
                    self.temp_access_chain
                        .push(SubAccess::Offset(i * row_stride));
                    let ty_inner = crate::TypeInner::Vector {
                        size: columns,
                        kind: crate::ScalarKind::Float,
                        width,
                    };
                    let sv = StoreValue::TempIndex {
                        depth,
                        index: i,
                        ty: TypeResolution::Value(ty_inner),
                    };
                    self.write_storage_store(module, var_handle, sv, func_ctx, indent + 1)?;
                    self.temp_access_chain.pop();
                }
                // done
                writeln!(self.out, "{}}}", INDENT.repeat(indent))?;
            }
            crate::TypeInner::Array {
                base,
                size: crate::ArraySize::Constant(const_handle),
                ..
            } => {
                // first, assign the value to a temporary
                writeln!(self.out, "{}{{", INDENT.repeat(indent))?;
                write!(self.out, "{}", INDENT.repeat(indent + 1))?;
                self.write_value_type(module, &module.types[base].inner)?;
                let depth = indent + 1;
                write!(self.out, " {}{}", STORE_TEMP_NAME, depth)?;
                self.write_array_size(module, crate::ArraySize::Constant(const_handle))?;
                write!(self.out, " = ")?;
                self.write_store_value(module, &value, func_ctx)?;
                writeln!(self.out, ";")?;
                // then iterate the stores
                let count = module.constants[const_handle].to_array_length().unwrap();
                let stride = module.types[base].inner.span(&module.constants);
                for i in 0..count {
                    self.temp_access_chain.push(SubAccess::Offset(i * stride));
                    let sv = StoreValue::TempIndex {
                        depth,
                        index: i,
                        ty: TypeResolution::Handle(base),
                    };
                    self.write_storage_store(module, var_handle, sv, func_ctx, indent + 1)?;
                    self.temp_access_chain.pop();
                }
                // done
                writeln!(self.out, "{}}}", INDENT.repeat(indent))?;
            }
            crate::TypeInner::Struct { ref members, .. } => {
                // first, assign the value to a temporary
                writeln!(self.out, "{}{{", INDENT.repeat(indent))?;
                let depth = indent + 1;
                let struct_ty = ty_resolution.handle().unwrap();
                let struct_name = &self.names[&NameKey::Type(struct_ty)];
                write!(
                    self.out,
                    "{}{} {}{} = ",
                    INDENT.repeat(indent + 1),
                    struct_name,
                    STORE_TEMP_NAME,
                    depth
                )?;
                self.write_store_value(module, &value, func_ctx)?;
                writeln!(self.out, ";")?;
                // then iterate the stores
                for (i, member) in members.iter().enumerate() {
                    self.temp_access_chain
                        .push(SubAccess::Offset(member.offset));
                    let sv = StoreValue::TempAccess {
                        depth,
                        base: struct_ty,
                        member_index: i as u32,
                    };
                    self.write_storage_store(module, var_handle, sv, func_ctx, indent + 1)?;
                    self.temp_access_chain.pop();
                }
                // done
                writeln!(self.out, "{}}}", INDENT.repeat(indent))?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    pub(super) fn fill_access_chain(
        &mut self,
        module: &crate::Module,
        mut cur_expr: Handle<crate::Expression>,
        func_ctx: &FunctionCtx,
    ) -> Result<Handle<crate::GlobalVariable>, Error> {
        self.temp_access_chain.clear();
        loop {
            // determine the size of the pontee
            let stride = match *func_ctx.info[cur_expr].ty.inner_with(&module.types) {
                crate::TypeInner::Pointer { base, class: _ } => {
                    module.types[base].inner.span(&module.constants)
                }
                crate::TypeInner::ValuePointer { size, width, .. } => {
                    size.map_or(1, |s| s as u32) * width as u32
                }
                _ => 0,
            };

            let (next_expr, sub) = match func_ctx.expressions[cur_expr] {
                crate::Expression::GlobalVariable(handle) => return Ok(handle),
                crate::Expression::Access { base, index } => (
                    base,
                    SubAccess::Index {
                        value: index,
                        stride,
                    },
                ),
                crate::Expression::AccessIndex { base, index } => {
                    match *func_ctx.info[base].ty.inner_with(&module.types) {
                        crate::TypeInner::Struct { ref members, .. } => {
                            (base, SubAccess::Offset(members[index as usize].offset))
                        }
                        _ => (base, SubAccess::Offset(index * stride)),
                    }
                }
                ref other => {
                    return Err(Error::Unimplemented(format!(
                        "Pointer access of {:?}",
                        other
                    )))
                }
            };
            self.temp_access_chain.push(sub);
            cur_expr = next_expr;
        }
    }
}
